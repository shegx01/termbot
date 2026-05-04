//! `WakeCoordinator` — owns the active-chat sets per platform, the
//! pre-MarkDirty `startup_was_clean` flag, and the gap-handling state
//! machine that runs on `PowerSignal::GapDetected` and at startup.
//!
//! Extracted from `App` because wake is the highest-stakes concurrency
//! path in the codebase: only the `power_rx` branch in `tokio::select!`
//! preempts `cmd_rx`, so `handle_gap` runs inline on the main loop and
//! must coordinate adapter pause/resume, banner-ack waiting, and Slack /
//! Discord catchup task spawning without ever blocking the main loop.
//! Lifting it into a dedicated type lets the wake-recovery flow be
//! unit-tested with mock adapters and a `FakePowerManager`-style harness
//! without instantiating App's harness graph.
//!
//! ## Ownership
//!
//! - `telegram_chats / slack_chats / discord_chats: HashSet<...>` — the
//!   live per-platform chat sets, hydrated from `terminus-state.json` on
//!   startup. Mutated by `handle_command` when a new chat first messages
//!   the bot, and by `App::apply_state_update` when a `Bind*` update
//!   arrives over the state channel.
//! - `startup_was_clean: bool` — snapshot of `last_clean_shutdown` from
//!   the state file *before* `App::new` applied `MarkDirty`. Used by
//!   `emit_startup_gap_banners` to gate the unclean-restart banner so a
//!   graceful previous shutdown doesn't trigger a spurious banner on the
//!   next boot.
//!
//! Platform handles, the `BannerCoordinator`, and `stream_tx` are
//! borrowed at the call site via [`WakeDispatchContext`] (same
//! split-borrow pattern as `harness_registry::PromptDispatchContext`).
//!
//! ## Concurrency contract
//!
//! `handle_gap` runs inline on the main `tokio::select!` branch. While it
//! is awaiting, `cmd_rx` is suspended — so any operation that would
//! `cmd_tx.send` from inside `handle_gap` would deadlock. The Slack and
//! Discord catchup paths therefore `tokio::spawn` background tasks that
//! own a `cmd_tx` clone; those spawned tasks run concurrently with the
//! adapters being resumed below them. A `catchup_in_progress` atomic
//! guard on each platform prevents a second `GapDetected` from spawning
//! a duplicate catchup while the prior one is still draining.

use std::collections::HashSet;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use tokio::sync::{broadcast, mpsc};

use crate::banner::BannerCoordinator;
use crate::buffer::StreamEvent;
#[cfg(feature = "discord")]
use crate::chat_adapters::discord::DiscordAdapter;
#[cfg(feature = "slack")]
use crate::chat_adapters::slack::SlackPlatform;
use crate::chat_adapters::{ChatPlatform, IncomingMessage, PlatformType};
use crate::power::types::PowerSignal;
use crate::state_store::StateUpdate;

/// Bundle of App-owned handles that `handle_gap` and
/// `emit_startup_gap_banners` need to drive adapter pause/resume, banner
/// emission + ack, and Slack/Discord catchup spawning.
///
/// Constructed at the call site in `App` via split-borrow so the wake
/// coordinator (`&self`) and the dispatch handles (immutable borrows of
/// disjoint App fields) can coexist with the `&mut self` receiver of the
/// pass-through method. NOT a getter return value — see
/// `harness_registry::PromptDispatchContext` for the full rationale.
pub struct WakeDispatchContext<'a> {
    pub telegram: Option<&'a (dyn ChatPlatform + 'a)>,
    pub slack: Option<&'a (dyn ChatPlatform + 'a)>,
    pub discord: Option<&'a (dyn ChatPlatform + 'a)>,
    /// Typed Arc to the Slack platform for the `run_catchup` inherent
    /// method (NOT on the `ChatPlatform` trait). Same load-bearing
    /// duplication rationale as `HarnessRegistry`'s typed Arcs.
    #[cfg(feature = "slack")]
    pub slack_platform: Option<&'a Arc<SlackPlatform>>,
    /// Typed Arc to the Discord platform for the `run_catchup` inherent
    /// method.
    #[cfg(feature = "discord")]
    pub discord_platform: Option<&'a Arc<DiscordAdapter>>,
    pub stream_tx: &'a broadcast::Sender<StreamEvent>,
    pub banner: &'a BannerCoordinator,
}

pub struct WakeCoordinator {
    telegram_chats: HashSet<i64>,
    slack_chats: HashSet<String>,
    discord_chats: HashSet<String>,
    startup_was_clean: bool,
}

impl WakeCoordinator {
    /// Construct from the persisted-state snapshot at startup. The chat
    /// sets are typically the `snapshot.chats.{telegram,slack,discord}`
    /// vectors collected into `HashSet`s; `startup_was_clean` is
    /// `snapshot.last_clean_shutdown` captured BEFORE `MarkDirty` is
    /// applied (otherwise it would always read `false`).
    pub fn new(
        telegram_chats: HashSet<i64>,
        slack_chats: HashSet<String>,
        discord_chats: HashSet<String>,
        startup_was_clean: bool,
    ) -> Self {
        Self {
            telegram_chats,
            slack_chats,
            discord_chats,
            startup_was_clean,
        }
    }

    /// Insert a Telegram chat ID. Returns `true` iff the chat was newly
    /// added — the caller (e.g. `handle_command`) uses this to gate the
    /// `state_tx.try_send(BindTelegramChat(id))` call so already-bound
    /// chats don't spam `StateUpdate`s.
    pub fn insert_telegram_chat(&mut self, id: i64) -> bool {
        self.telegram_chats.insert(id)
    }

    /// Insert a Slack channel ID. Returns `true` iff the channel was newly
    /// added — same gating contract as [`Self::insert_telegram_chat`].
    pub fn insert_slack_chat(&mut self, id: String) -> bool {
        self.slack_chats.insert(id)
    }

    /// Insert a Discord channel ID. Returns `true` iff the channel was newly
    /// added — same gating contract as [`Self::insert_telegram_chat`].
    pub fn insert_discord_chat(&mut self, id: String) -> bool {
        self.discord_chats.insert(id)
    }

    /// Apply a `Bind*Chat` `StateUpdate` to the matching set, discarding
    /// the newly-added bool. Used by `App::apply_state_update`'s
    /// state-channel drain path; `handle_command` instead calls the
    /// typed `insert_*` methods because it needs the bool.
    pub fn apply_bind(&mut self, update: &StateUpdate) {
        match update {
            StateUpdate::BindTelegramChat(id) => {
                self.telegram_chats.insert(*id);
            }
            StateUpdate::BindSlackChat(id) => {
                self.slack_chats.insert(id.clone());
            }
            StateUpdate::BindDiscordChat(id) => {
                self.discord_chats.insert(id.clone());
            }
            // All other StateUpdate variants (TelegramOffset, MarkDirty, Tick,
            // SetCleanShutdown, HarnessSessionBatch, SlackWatermark,
            // DiscordWatermark) are not chat-bind events and do not touch
            // the wake coordinator's sets.
            _ => {}
        }
    }

    /// Test-only point-query: does the Telegram set contain `id`?
    #[cfg(test)]
    pub fn contains_telegram_chat(&self, id: i64) -> bool {
        self.telegram_chats.contains(&id)
    }

    /// Test-only point-query: does the Slack set contain `id`?
    #[cfg(test)]
    pub fn contains_slack_chat(&self, id: &str) -> bool {
        self.slack_chats.contains(id)
    }

    /// Test-only point-query: is the Telegram set empty?
    #[cfg(test)]
    pub fn telegram_chats_is_empty(&self) -> bool {
        self.telegram_chats.is_empty()
    }

    /// Test-only point-query: is the Slack set empty?
    #[cfg(test)]
    pub fn slack_chats_is_empty(&self) -> bool {
        self.slack_chats.is_empty()
    }

    /// Test-only point-query: is the Discord set empty?
    #[cfg(test)]
    pub fn discord_chats_is_empty(&self) -> bool {
        self.discord_chats.is_empty()
    }

    /// Emit gap banners for any active chats if we detect an unclean
    /// restart with a wall gap > 30s. Call this once after
    /// `App::reconcile_startup`.
    ///
    /// `last_seen_wall` comes from the persisted state's `last_seen_wall`
    /// field (passed in rather than read directly so the coordinator
    /// doesn't need a `StatePersistor` reference). `stream_tx` is the
    /// broadcast sender into which `StreamEvent::GapBanner`s are emitted.
    pub fn emit_startup_gap_banners(
        &self,
        last_seen_wall: Option<DateTime<Utc>>,
        stream_tx: &broadcast::Sender<StreamEvent>,
    ) {
        let now = Utc::now();
        let last_seen = match last_seen_wall {
            Some(t) => t,
            None => return, // first ever run — no gap
        };
        let wall_gap = now
            .signed_duration_since(last_seen)
            .to_std()
            .unwrap_or(Duration::ZERO);

        // Only fire if the gap exceeds 30s AND the previous shutdown was
        // not clean. We use the cached `startup_was_clean` (captured before
        // MarkDirty) rather than reading the state store now, because the
        // store's `last_clean_shutdown` is always `false` post-MarkDirty.
        if wall_gap <= Duration::from_secs(30) || self.startup_was_clean {
            return;
        }

        let chat_count =
            self.telegram_chats.len() + self.slack_chats.len() + self.discord_chats.len();
        if chat_count == 0 {
            return;
        }

        tracing::info!(
            "Detected unclean restart — emitting gap banners for {} chat(s) \
             (gap={:.0}s, paused_at={}, resumed_at={})",
            chat_count,
            wall_gap.as_secs_f64(),
            last_seen,
            now,
        );

        for &chat_id in &self.telegram_chats {
            let _ = stream_tx.send(StreamEvent::GapBanner {
                chat_id: chat_id.to_string(),
                platform: PlatformType::Telegram,
                paused_at: last_seen,
                resumed_at: now,
                gap: wall_gap,
                missed_count: 0,
            });
        }
        for chat_id in &self.slack_chats {
            let _ = stream_tx.send(StreamEvent::GapBanner {
                chat_id: chat_id.clone(),
                platform: PlatformType::Slack,
                paused_at: last_seen,
                resumed_at: now,
                gap: wall_gap,
                missed_count: 0,
            });
        }
        for chat_id in &self.discord_chats {
            let _ = stream_tx.send(StreamEvent::GapBanner {
                chat_id: chat_id.clone(),
                platform: PlatformType::Discord,
                paused_at: last_seen,
                resumed_at: now,
                gap: wall_gap,
                missed_count: 0,
            });
        }
    }

    /// Handle a `PowerSignal::GapDetected`: pause adapters, broadcast a
    /// `GapBanner` per active chat, await delivery acks (via
    /// [`BannerCoordinator`], 5s timeout per chat), spawn Slack and
    /// Discord catchup tasks, then resume adapters.
    ///
    /// `cmd_tx` is passed in (not stored on the coordinator) so that
    /// dropping the local sender in `main.rs` still closes the channel
    /// when all adapters exit — restoring the adapter-exhaustion shutdown
    /// path. The clone is moved into the spawned catchup task, which
    /// prevents the deadlock where `run_catchup` would otherwise block on
    /// `cmd_tx.send` while `cmd_rx` is suspended waiting for `handle_gap`
    /// to return.
    pub async fn handle_gap(
        &self,
        signal: PowerSignal,
        cmd_tx: mpsc::Sender<IncomingMessage>,
        dispatch: &WakeDispatchContext<'_>,
    ) {
        // `cmd_tx` is consumed only when the slack feature is compiled in
        // (Discord catchup spawns a task that captures other state). The
        // explicit reference suppresses the unused-variable warning for
        // builds that disable slack — covers all current and future
        // feature-combo permutations without a stale `cfg_attr`.
        let _ = &cmd_tx;
        let PowerSignal::GapDetected {
            paused_at,
            resumed_at,
            gap,
        } = signal;

        // 1. Pause all adapters via trait method.
        for p in [dispatch.telegram, dispatch.slack, dispatch.discord]
            .into_iter()
            .flatten()
        {
            p.pause().await;
        }
        tracing::info!(
            "Adapters paused for gap handling \
             (paused_at={}, resumed_at={}, gap={:.0}s)",
            paused_at,
            resumed_at,
            gap.as_secs_f64()
        );

        // 2. Broadcast a GapBanner and wait for delivery ack per chat.
        let all_chats: Vec<(String, PlatformType)> = self
            .telegram_chats
            .iter()
            .map(|id| (id.to_string(), PlatformType::Telegram))
            .chain(
                self.slack_chats
                    .iter()
                    .map(|id| (id.clone(), PlatformType::Slack)),
            )
            .chain(
                self.discord_chats
                    .iter()
                    .map(|id| (id.clone(), PlatformType::Discord)),
            )
            .collect();

        // Two-phase contract: install BEFORE broadcast so a fast delivery
        // task can't race past the install and lose its ack target. See
        // `src/banner.rs` for the full ordering invariant.
        let chat_ids: Vec<String> = all_chats.iter().map(|(id, _)| id.clone()).collect();
        let ack_futures = dispatch.banner.install_pending(&chat_ids).await;

        for (chat_id, platform) in &all_chats {
            let _ = dispatch.stream_tx.send(StreamEvent::GapBanner {
                chat_id: chat_id.clone(),
                platform: *platform,
                paused_at,
                resumed_at,
                gap,
                missed_count: 0,
            });
        }

        // Await acks (5s per chat). On timeout or dropped sender, the
        // coordinator registers an inline-prefix fallback so the next
        // outbound message for that chat carries the gap marker.
        dispatch
            .banner
            .await_acks_with_fallback(ack_futures, gap, paused_at, resumed_at)
            .await;

        // 3. Slack catchup: fetch missed messages since paused_at.
        //
        // Spawned into a separate task so that `run_catchup`'s `cmd_tx.send`
        // calls don't deadlock against the suspended `cmd_rx` branch in the
        // main `tokio::select!` loop. The spawned task runs concurrently;
        // adapters are resumed immediately below without waiting for catchup
        // to finish.
        #[cfg(feature = "slack")]
        if let Some(sp_ref) = dispatch.slack_platform {
            let active: Vec<String> = self.slack_chats.iter().cloned().collect();
            if !active.is_empty() {
                // Guard against concurrent catchup tasks (e.g. two rapid GapDetected
                // signals). If a task is already running, skip rather than spawn a
                // second one — the running task will cover the gap.
                if sp_ref
                    .catchup_in_progress
                    .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                    .is_ok()
                {
                    let oldest_ts = paused_at_to_slack_ts(paused_at);
                    tracing::info!(
                        "Triggering Slack catchup for {} channel(s) from {}",
                        active.len(),
                        oldest_ts
                    );
                    let sp = Arc::clone(sp_ref);
                    tokio::spawn(async move {
                        let result = sp.run_catchup(&cmd_tx, &active, &oldest_ts).await;
                        sp.catchup_in_progress.store(false, Ordering::SeqCst);
                        match result {
                            Ok(()) => tracing::info!("Slack catchup task complete"),
                            Err(e) => tracing::error!("Slack catchup failed: {}", e),
                        }
                    });
                } else {
                    tracing::info!("Slack catchup already in progress, skipping duplicate spawn");
                }
            }
        }

        // 3b. Discord catchup: fetch missed messages since the stored snowflake watermarks.
        //
        // Spawned (not awaited) for the same reason as Slack catchup above:
        // `run_catchup` calls `cmd_tx.send`, which would deadlock against the
        // suspended `cmd_rx` branch in the main `tokio::select!` loop.
        #[cfg(feature = "discord")]
        if let Some(discord_ref) = dispatch.discord_platform {
            if discord_ref
                .catchup_in_progress
                .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                let discord = Arc::clone(discord_ref);
                tokio::spawn(async move {
                    if let Err(e) = discord.run_catchup().await {
                        tracing::warn!("Discord catchup failed: {}", e);
                    }
                    discord.catchup_in_progress.store(false, Ordering::SeqCst);
                });
            } else {
                tracing::warn!("Discord catchup already in progress, skipping duplicate spawn");
            }
        }

        // 4. Resume all adapters via trait method.
        for p in [dispatch.telegram, dispatch.slack, dispatch.discord]
            .into_iter()
            .flatten()
        {
            p.resume().await;
        }
        tracing::info!("Adapters resumed after gap handling");
    }

    // ── Test-only helpers ────────────────────────────────────────────────

    #[cfg(test)]
    pub fn contains_discord_chat(&self, id: &str) -> bool {
        self.discord_chats.contains(id)
    }
}

/// Convert a `DateTime<Utc>` to a Slack-format message timestamp.
///
/// Slack timestamps are `"{unix_seconds}.{microseconds:06}"`. This format
/// is monotonic per-channel and used as the `oldest` parameter for
/// `conversations.history` catchup calls.
#[cfg(feature = "slack")]
fn paused_at_to_slack_ts(dt: DateTime<Utc>) -> String {
    format!("{}.{:06}", dt.timestamp(), dt.timestamp_subsec_micros())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_wake() -> WakeCoordinator {
        WakeCoordinator::new(HashSet::new(), HashSet::new(), HashSet::new(), true)
    }

    #[test]
    fn insert_telegram_chat_returns_true_only_on_first_insert() {
        let mut wake = empty_wake();
        assert!(wake.insert_telegram_chat(42));
        assert!(!wake.insert_telegram_chat(42), "second insert must dedup");
        assert!(wake.insert_telegram_chat(43));
    }

    #[test]
    fn insert_slack_chat_returns_true_only_on_first_insert() {
        let mut wake = empty_wake();
        assert!(wake.insert_slack_chat("C123".into()));
        assert!(
            !wake.insert_slack_chat("C123".into()),
            "second insert must dedup"
        );
        assert!(wake.insert_slack_chat("C456".into()));
    }

    #[test]
    fn insert_discord_chat_returns_true_only_on_first_insert() {
        let mut wake = empty_wake();
        assert!(wake.insert_discord_chat("D123".into()));
        assert!(
            !wake.insert_discord_chat("D123".into()),
            "second insert must dedup"
        );
        assert!(wake.insert_discord_chat("D456".into()));
    }

    #[test]
    fn apply_bind_routes_each_variant_to_the_right_set() {
        let mut wake = empty_wake();
        wake.apply_bind(&StateUpdate::BindTelegramChat(7));
        wake.apply_bind(&StateUpdate::BindSlackChat("C123".into()));
        wake.apply_bind(&StateUpdate::BindDiscordChat("D456".into()));

        assert!(wake.contains_telegram_chat(7));
        assert!(wake.contains_slack_chat("C123"));
        assert!(wake.contains_discord_chat("D456"));
    }

    #[test]
    fn apply_bind_ignores_all_non_bind_state_updates() {
        // Exhaustive: every non-Bind variant of StateUpdate must leave the
        // wake coordinator's chat sets untouched. A new Bind* variant added
        // to the StateUpdate enum without a matching match arm here would
        // silently miss the chat set — this test guards against that drift.
        let mut wake = empty_wake();
        wake.apply_bind(&StateUpdate::TelegramOffset(99));
        wake.apply_bind(&StateUpdate::MarkDirty);
        wake.apply_bind(&StateUpdate::Tick);
        wake.apply_bind(&StateUpdate::SetCleanShutdown(true));
        wake.apply_bind(&StateUpdate::HarnessSessionBatch(vec![]));
        wake.apply_bind(&StateUpdate::SlackWatermark {
            channel_id: "C001".into(),
            ts: "1700000000.000001".into(),
        });
        wake.apply_bind(&StateUpdate::DiscordWatermark {
            channel_id: "D001".into(),
            message_id: 1_700_000_000_000_000_001,
        });

        assert!(wake.telegram_chats_is_empty());
        assert!(wake.slack_chats_is_empty());
        assert!(wake.discord_chats_is_empty());
    }

    #[tokio::test]
    async fn emit_startup_gap_banners_skips_when_first_run() {
        // No `last_seen_wall` ever recorded → no banners.
        let wake = WakeCoordinator::new(
            [42i64].into_iter().collect(),
            HashSet::new(),
            HashSet::new(),
            false,
        );
        let (tx, mut rx) = broadcast::channel::<StreamEvent>(8);
        wake.emit_startup_gap_banners(None, &tx);

        let result = tokio::time::timeout(Duration::from_millis(20), rx.recv()).await;
        assert!(result.is_err(), "no banner expected on first run");
    }

    #[tokio::test]
    async fn emit_startup_gap_banners_skips_on_clean_shutdown() {
        let wake = WakeCoordinator::new(
            [42i64].into_iter().collect(),
            HashSet::new(),
            HashSet::new(),
            true, // ← previous shutdown was clean
        );
        let (tx, mut rx) = broadcast::channel::<StreamEvent>(8);
        let two_minutes_ago = Utc::now() - chrono::Duration::seconds(120);
        wake.emit_startup_gap_banners(Some(two_minutes_ago), &tx);

        let result = tokio::time::timeout(Duration::from_millis(20), rx.recv()).await;
        assert!(
            result.is_err(),
            "no banner expected after a clean previous shutdown"
        );
    }

    // An "exactly 30s" boundary test would be inherently flaky against
    // `Utc::now()` because test scheduling slips microseconds past the
    // intended boundary. We instead pin both sides asymmetrically:
    // `_skips_on_small_gap` (10s) covers the strictly-under case, and
    // `_fires_just_past_30s_boundary` (31s) covers the strictly-over case.

    #[tokio::test]
    async fn emit_startup_gap_banners_fires_just_past_30s_boundary() {
        let wake = WakeCoordinator::new(
            [42i64].into_iter().collect(),
            HashSet::new(),
            HashSet::new(),
            false,
        );
        let (tx, mut rx) = broadcast::channel::<StreamEvent>(8);
        let thirty_one_s_ago = Utc::now() - chrono::Duration::seconds(31);
        wake.emit_startup_gap_banners(Some(thirty_one_s_ago), &tx);
        let event = tokio::time::timeout(Duration::from_millis(50), rx.recv())
            .await
            .expect("banner expected for 31s gap")
            .expect("channel closed");
        match event {
            StreamEvent::GapBanner { chat_id, .. } => assert_eq!(chat_id, "42"),
            other => panic!("expected GapBanner, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn emit_startup_gap_banners_skips_on_small_gap() {
        let wake = WakeCoordinator::new(
            [42i64].into_iter().collect(),
            HashSet::new(),
            HashSet::new(),
            false,
        );
        let (tx, mut rx) = broadcast::channel::<StreamEvent>(8);
        let ten_seconds_ago = Utc::now() - chrono::Duration::seconds(10);
        wake.emit_startup_gap_banners(Some(ten_seconds_ago), &tx);

        let result = tokio::time::timeout(Duration::from_millis(20), rx.recv()).await;
        assert!(result.is_err(), "no banner expected for gap <= 30s");
    }

    #[tokio::test]
    async fn emit_startup_gap_banners_skips_when_no_active_chats() {
        let wake = WakeCoordinator::new(HashSet::new(), HashSet::new(), HashSet::new(), false);
        let (tx, mut rx) = broadcast::channel::<StreamEvent>(8);
        let two_minutes_ago = Utc::now() - chrono::Duration::seconds(120);
        wake.emit_startup_gap_banners(Some(two_minutes_ago), &tx);

        let result = tokio::time::timeout(Duration::from_millis(20), rx.recv()).await;
        assert!(result.is_err(), "no banner expected with zero active chats");
    }

    #[tokio::test]
    async fn emit_startup_gap_banners_fires_one_per_active_chat_across_platforms() {
        // Mixed-platform fire path: one of each, all should emit a
        // GapBanner with the right platform discriminant.
        let wake = WakeCoordinator::new(
            [111i64].into_iter().collect(),
            ["C_slack".to_string()].into_iter().collect(),
            ["D_discord".to_string()].into_iter().collect(),
            false,
        );
        let (tx, mut rx) = broadcast::channel::<StreamEvent>(8);
        let two_minutes_ago = Utc::now() - chrono::Duration::seconds(120);
        wake.emit_startup_gap_banners(Some(two_minutes_ago), &tx);

        let mut platforms_seen: Vec<PlatformType> = Vec::new();
        for _ in 0..3 {
            let event = tokio::time::timeout(Duration::from_millis(50), rx.recv())
                .await
                .expect("timeout waiting for banner")
                .expect("channel closed");
            match event {
                StreamEvent::GapBanner { platform, .. } => platforms_seen.push(platform),
                other => panic!("expected GapBanner, got {:?}", other),
            }
        }
        platforms_seen.sort_by_key(|p| match p {
            PlatformType::Telegram => 0,
            PlatformType::Slack => 1,
            PlatformType::Discord => 2,
            // Socket-origin messages aren't bound to any wake-coordinator
            // chat set, so they never appear in this test's gap-banner stream.
            PlatformType::Socket => {
                unreachable!("Socket chats must never appear in wake coordinator sets")
            }
        });
        assert_eq!(
            platforms_seen,
            vec![
                PlatformType::Telegram,
                PlatformType::Slack,
                PlatformType::Discord
            ]
        );
    }

    #[cfg(feature = "slack")]
    #[test]
    fn paused_at_to_slack_ts_format() {
        let dt = chrono::DateTime::<Utc>::from_timestamp(1_700_000_000, 123_456_000)
            .expect("valid timestamp");
        let ts = paused_at_to_slack_ts(dt);
        assert_eq!(ts, "1700000000.123456");
    }
}
