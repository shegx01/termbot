use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
#[cfg(feature = "slack")]
use chrono::DateTime;
use chrono::Utc;
use tokio::sync::{broadcast, mpsc};

use crate::banner::BannerCoordinator;
use crate::buffer::{OutputBuffer, StreamEvent};
#[cfg(feature = "discord")]
use crate::chat_adapters::discord::DiscordAdapter;
#[cfg(feature = "slack")]
use crate::chat_adapters::slack::SlackPlatform;
use crate::chat_adapters::{Attachment, ChatPlatform, IncomingMessage, PlatformType, ReplyContext};
use crate::command::{CommandBlocklist, HarnessOptions, HarnessSubcommandKind, ParsedCommand};
use crate::config::Config;
use crate::delivery::{split_message, GapInfo, GapPrefixes, PendingBannerAcks};
use crate::harness::{drive_harness, HarnessContext, HarnessKind};
use crate::harness_registry::{HarnessRegistry, PromptDispatchContext};
use crate::power::types::PowerSignal;
use crate::session::{self, SessionManager};
use crate::socket::events::AmbientEvent;
use crate::state_persistor::StatePersistor;
#[cfg(test)]
use crate::state_store::NamedSessionEntry;
use crate::state_store::{StateStore, StateUpdate};
use crate::structured_output::{spawn_retry_worker, DeliveryQueue, SchemaRegistry, WebhookClient};
use crate::tmux::TmuxClient;

// ──────────────────────────────────────────────────────────────────────────────
// App
// ──────────────────────────────────────────────────────────────────────────────

pub struct App {
    blocklist: CommandBlocklist,
    session_mgr: SessionManager,
    buffers: HashMap<String, OutputBuffer>,
    /// Owns the four harness implementations, the named-session index +
    /// LRU policy, and the prompt-dispatch logic. See `src/harness_registry.rs`.
    harness_registry: HarnessRegistry,
    stream_tx: broadcast::Sender<StreamEvent>,
    telegram: Option<Arc<dyn ChatPlatform>>,
    slack: Option<Arc<dyn ChatPlatform>>,
    /// Typed Arc to the Slack platform for direct catchup calls.
    /// Kept alongside the dyn `slack` field — both point to the same object.
    #[cfg(feature = "slack")]
    slack_platform: Option<Arc<SlackPlatform>>,
    discord: Option<Arc<dyn ChatPlatform>>,
    /// Typed Arc to the Discord platform for direct catchup calls.
    /// Kept alongside the dyn `discord` field — both point to the same object.
    #[cfg(feature = "discord")]
    discord_platform: Option<Arc<DiscordAdapter>>,
    offline_buffer_max: usize,
    trigger: char,

    // ── Sleep/wake recovery state ─────────────────────────────────────────────
    /// State store + debounce-or-force-persist policy + first-interaction
    /// dirty guard. See `src/state_persistor.rs` for the extracted
    /// implementation. App owns it; the rest of the system writes through
    /// either `state_tx` (which the main loop drains and forwards to
    /// `apply_state_update`) or via direct method calls on `App`.
    state_persistor: StatePersistor,
    /// mpsc sender to push `StateUpdate`s into the App's `state_rx` channel.
    /// Kept here so `App` itself can send updates (e.g. `MarkDirty`, `Tick`).
    state_tx: mpsc::Sender<StateUpdate>,
    /// Active Telegram chat IDs (hydrated from persisted state on startup).
    active_telegram_chats: HashSet<i64>,
    /// Active Slack channel IDs (hydrated from persisted state on startup).
    active_slack_chats: HashSet<String>,
    /// Active Discord channel IDs (hydrated from persisted state on startup).
    active_discord_chats: HashSet<String>,
    /// Banner-ack and inline-prefix coordinator. Owns the `pending_banner_acks`
    /// and `gap_prefix` shared maps; runs the install/await/fallback dance
    /// for `handle_gap`. See `src/banner.rs` for the extracted state machine
    /// and concurrency contract.
    banner: BannerCoordinator,
    /// Snapshot of `last_clean_shutdown` from the state file *before* we
    /// applied `MarkDirty` on startup.  Used by `emit_startup_gap_banners`
    /// to check whether the previous run exited cleanly.
    startup_was_clean: bool,

    // ── Structured output ─────────────────────────────────────────────────────
    schema_registry: Arc<SchemaRegistry>,
    delivery_queue: Arc<DeliveryQueue>,
    webhook_client: Arc<WebhookClient>,
    // ── Socket ambient event bus ───────────────────────────────────────────
    /// Broadcast sender for genuinely-new ambient events consumed by socket
    /// subscribers.  Capacity 512.  Fire-and-forget: send errors are ignored
    /// (expected when no socket connections are active or socket is disabled).
    ambient_tx: broadcast::Sender<AmbientEvent>,

    /// Shutdown notify (async wakeup) for the retry worker.  Notified by
    /// `mark_clean_shutdown` for immediate wakeup during idle/backoff sleeps.
    retry_worker_shutdown: Arc<tokio::sync::Notify>,
    /// Shutdown flag (non-blocking poll) for the retry worker.  Flipped by
    /// `mark_clean_shutdown` so the worker can check between-jobs and break out
    /// of an active drain loop without waiting for the next await point.
    retry_worker_shutdown_flag: Arc<AtomicBool>,
}

impl App {
    /// Construct `App`, hydrating active chats and initial state from `store`.
    pub fn new(
        config: &Config,
        store: StateStore,
        state_tx: mpsc::Sender<StateUpdate>,
    ) -> Result<Self> {
        let blocklist = CommandBlocklist::from_config(&config.blocklist.patterns)?;
        let trigger = config.commands.trigger;
        let session_mgr =
            SessionManager::new(TmuxClient::new(), config.streaming.max_sessions, trigger);
        // Build structured output infrastructure.
        let schema_registry = Arc::new(SchemaRegistry::from_config(&config.schemas)?);
        let delivery_queue = Arc::new(DeliveryQueue::new(
            config.structured_output.queue_dir.clone(),
        )?);
        let webhook_client = Arc::new(WebhookClient::new(config.structured_output.timeout_ms));
        let retry_worker_shutdown = Arc::new(tokio::sync::Notify::new());
        let retry_worker_shutdown_flag = Arc::new(AtomicBool::new(false));

        let (stream_tx, _) = broadcast::channel::<StreamEvent>(256);
        let (ambient_tx, _) = broadcast::channel::<AmbientEvent>(512);

        // Hydrate active chat sets from persisted state.
        let snapshot = store.snapshot();
        let active_telegram_chats: HashSet<i64> = snapshot.chats.telegram.iter().copied().collect();
        let active_slack_chats: HashSet<String> = snapshot.chats.slack.iter().cloned().collect();
        let active_discord_chats: HashSet<String> =
            snapshot.chats.discord.iter().cloned().collect();
        // Capture this BEFORE applying MarkDirty, so emit_startup_gap_banners
        // knows whether the *previous* run exited cleanly.
        let startup_was_clean = snapshot.last_clean_shutdown;
        let named_harness_sessions = snapshot.harness_sessions.clone();
        let legacy_count = named_harness_sessions
            .keys()
            .filter(|k| !k.contains(':'))
            .count();
        if legacy_count > 0 {
            tracing::info!(
                legacy_keys = legacy_count,
                "unprefixed legacy named-session keys remaining"
            );
        }

        // Wrap the loaded store in the persistor. We do NOT apply MarkDirty
        // yet — `startup_was_clean` was captured above from the pre-dirty
        // snapshot, and we want the dirty marker on disk before constructing
        // the rest of `App`.
        let mut state_persistor = StatePersistor::new(store);

        // Immediately mark state dirty (sets last_clean_shutdown=false) and
        // force-persist so a crash within the first ~5s (before the debounce
        // window fires) still triggers a restart banner on the next boot.
        // Bubble persist failures up: the restart-banner gate depends on this
        // write completing, so we'd rather fail App::new than silently boot
        // with a dirty bit only in memory.
        state_persistor.apply_force(StateUpdate::MarkDirty)?;

        // Build the harness registry (owns the four harness implementations,
        // the named-session index, and the persist channel clone).
        let harness_registry = HarnessRegistry::new(
            config,
            state_tx.clone(),
            ambient_tx.clone(),
            Arc::clone(&schema_registry),
            named_harness_sessions,
        );

        let app = Self {
            blocklist,
            session_mgr,
            buffers: HashMap::new(),
            harness_registry,
            stream_tx,
            telegram: None,
            slack: None,
            #[cfg(feature = "slack")]
            slack_platform: None,
            discord: None,
            #[cfg(feature = "discord")]
            discord_platform: None,
            offline_buffer_max: config.streaming.offline_buffer_max_bytes,
            trigger,
            state_persistor,
            state_tx,
            active_telegram_chats,
            active_slack_chats,
            active_discord_chats,
            banner: BannerCoordinator::new(),
            startup_was_clean,
            schema_registry: Arc::clone(&schema_registry),
            delivery_queue: Arc::clone(&delivery_queue),
            webhook_client: Arc::clone(&webhook_client),
            ambient_tx,
            retry_worker_shutdown: Arc::clone(&retry_worker_shutdown),
            retry_worker_shutdown_flag: Arc::clone(&retry_worker_shutdown_flag),
        };

        // Spawn the retry worker with the broadcast sender's subscribe handle.
        // We subscribe once here; the worker holds its own receiver.
        {
            let events_tx = app.stream_tx.clone();
            spawn_retry_worker(
                Arc::clone(&delivery_queue),
                Arc::clone(&webhook_client),
                Arc::clone(&schema_registry),
                events_tx,
                Arc::clone(&retry_worker_shutdown),
                Arc::clone(&retry_worker_shutdown_flag),
                config.structured_output.max_retry_age_hours,
            );
        }

        Ok(app)
    }

    pub fn set_platforms(
        &mut self,
        telegram: Option<Arc<dyn ChatPlatform>>,
        slack: Option<Arc<dyn ChatPlatform>>,
        #[cfg(feature = "slack")] slack_platform: Option<Arc<SlackPlatform>>,
        discord: Option<Arc<dyn ChatPlatform>>,
        #[cfg(feature = "discord")] discord_platform: Option<Arc<DiscordAdapter>>,
    ) {
        self.telegram = telegram;
        self.slack = slack;
        #[cfg(feature = "slack")]
        {
            self.slack_platform = slack_platform;
        }
        self.discord = discord;
        #[cfg(feature = "discord")]
        {
            self.discord_platform = discord_platform;
        }
    }

    pub fn subscribe_stream(&self) -> broadcast::Receiver<StreamEvent> {
        self.stream_tx.subscribe()
    }

    /// Subscribe to the ambient event bus for socket consumers.
    #[allow(dead_code)] // used by SocketServer when integration tests are wired
    pub fn subscribe_ambient(&self) -> broadcast::Receiver<AmbientEvent> {
        self.ambient_tx.subscribe()
    }

    /// Clone the stream broadcast sender (for socket server).
    pub fn stream_tx_clone(&self) -> broadcast::Sender<StreamEvent> {
        self.stream_tx.clone()
    }

    /// Clone the ambient broadcast sender (for socket server).
    pub fn ambient_tx_clone(&self) -> broadcast::Sender<AmbientEvent> {
        self.ambient_tx.clone()
    }

    /// Fire-and-forget emit of an ambient event.  Ignores closed-channel
    /// errors (expected when socket is disabled / no subscribers).
    fn emit_ambient(&self, event: AmbientEvent) {
        let _ = self.ambient_tx.send(event);
    }

    /// Shared handle to the banner-ack map — passed into `spawn_delivery_task`
    /// so delivery tasks can resolve pending oneshots directly.
    pub fn pending_banner_acks_handle(&self) -> PendingBannerAcks {
        self.banner.pending_acks_handle()
    }

    /// Shared handle to the gap-prefix map — passed into `spawn_delivery_task`
    /// so delivery tasks can consume inline `[gap: …]` markers set by
    /// `handle_gap` when banner delivery times out.
    pub fn gap_prefix_handle(&self) -> GapPrefixes {
        self.banner.prefixes_handle()
    }

    /// Returns the initial Telegram offset from the persisted state snapshot.
    /// Call this before constructing the `TelegramAdapter` with
    /// `.with_initial_offset(...)`.
    pub fn initial_telegram_offset(&self) -> i64 {
        self.state_persistor.snapshot().telegram.offset
    }

    /// Returns the per-channel Slack watermarks from the persisted state snapshot.
    /// Call this before `seed_watermarks` on `SlackPlatform`.
    pub fn initial_slack_watermarks(&self) -> std::collections::HashMap<String, String> {
        self.state_persistor.snapshot().slack_watermarks.clone()
    }

    /// Returns the per-channel Discord snowflake watermarks from the persisted state snapshot.
    /// Pass to `DiscordAdapter::new` as `initial_watermarks`.
    pub fn initial_discord_watermarks(&self) -> std::collections::HashMap<String, u64> {
        self.state_persistor.snapshot().discord_watermarks.clone()
    }

    pub async fn reconcile_startup(&mut self) {
        // Clean stale pipe-pane output files (legacy)
        let tmp_dir = std::path::Path::new("/tmp");
        if let Ok(entries) = std::fs::read_dir(tmp_dir) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name_str = name.to_string_lossy();
                if name_str.starts_with("terminus-") && name_str.ends_with(".out") {
                    tracing::info!("Cleaning stale output file: {}", entry.path().display());
                    let _ = std::fs::remove_file(entry.path());
                }
                // Clean stale attachment temp files from Slack downloads and
                // socket binary uploads (both now use the `terminus-attachment-` prefix).
                if name_str.starts_with("terminus-attachment-") {
                    tracing::info!(
                        "Cleaning stale attachment temp file: {}",
                        entry.path().display()
                    );
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }

        // Verify tmux is available before anything else.
        match self.session_mgr.tmux().verify_available().await {
            Ok(version) => tracing::info!("tmux available: {}", version),
            Err(e) => tracing::error!(
                "tmux is not available: {:#}. Session commands will fail.",
                e
            ),
        }

        // Reconnect to surviving term-* tmux sessions
        match self.session_mgr.tmux().list_sessions().await {
            Ok(sessions) if !sessions.is_empty() => {
                tracing::info!(
                    "Found {} existing tmux session(s), reconnecting...",
                    sessions.len()
                );
                for name in sessions {
                    match self.session_mgr.reconnect_session(&name).await {
                        Ok(()) => {
                            let mut buf = OutputBuffer::new(&name, self.offline_buffer_max);
                            buf.sync_offset(self.session_mgr.tmux()).await;
                            self.buffers.insert(name.clone(), buf);
                            tracing::info!("Reconnected session '{}'", name);
                        }
                        Err(e) => {
                            tracing::warn!("Failed to reconnect session '{}': {:#}", name, e);
                        }
                    }
                }
                if let Some(fg) = self.session_mgr.foreground_session() {
                    tracing::info!("Foreground session: '{}'", fg);
                }
            }
            Ok(_) => {
                tracing::info!("No existing tmux sessions found");
            }
            Err(e) => {
                tracing::warn!("Failed to list tmux sessions: {:#}", e);
            }
        }
    }

    /// Emit gap banners for any active chats if we detect an unclean restart
    /// with a wall gap > 30s.  Call this once after `reconcile_startup()`.
    pub fn emit_startup_gap_banners(&mut self) {
        let snapshot = self.state_persistor.snapshot().clone();
        let now = Utc::now();
        let last_seen = match snapshot.last_seen_wall {
            Some(t) => t,
            None => return, // first ever run — no gap
        };
        let wall_gap = now
            .signed_duration_since(last_seen)
            .to_std()
            .unwrap_or(Duration::ZERO);

        // Only fire if the gap exceeds 30s AND last shutdown was not clean.
        // Use `startup_was_clean` (captured before MarkDirty) rather than the
        // current store value (which is always false after MarkDirty).
        if wall_gap <= Duration::from_secs(30) || self.startup_was_clean {
            return;
        }

        let chat_count = self.active_telegram_chats.len()
            + self.active_slack_chats.len()
            + self.active_discord_chats.len();
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

        // Emit one GapBanner per active Telegram chat.
        for &chat_id in &self.active_telegram_chats {
            let _ = self.stream_tx.send(StreamEvent::GapBanner {
                chat_id: chat_id.to_string(),
                platform: PlatformType::Telegram,
                paused_at: last_seen,
                resumed_at: now,
                gap: wall_gap,
                missed_count: 0,
            });
        }
        // Emit one GapBanner per active Slack chat.
        for chat_id in &self.active_slack_chats {
            let _ = self.stream_tx.send(StreamEvent::GapBanner {
                chat_id: chat_id.clone(),
                platform: PlatformType::Slack,
                paused_at: last_seen,
                resumed_at: now,
                gap: wall_gap,
                missed_count: 0,
            });
        }
        // Emit one GapBanner per active Discord chat.
        for chat_id in &self.active_discord_chats {
            let _ = self.stream_tx.send(StreamEvent::GapBanner {
                chat_id: chat_id.clone(),
                platform: PlatformType::Discord,
                paused_at: last_seen,
                resumed_at: now,
                gap: wall_gap,
                missed_count: 0,
            });
        }
    }

    /// Handle a `PowerSignal::GapDetected`: pause polling, broadcast a
    /// GapBanner per active chat, wait for delivery acks (5s timeout),
    /// then resume polling.
    ///
    /// `cmd_tx` is passed in (not stored on `App`) so that dropping the local
    /// sender in `main.rs` still closes the channel when all adapters exit —
    /// restoring the adapter-exhaustion shutdown path.  The `cmd_tx` clone is
    /// moved into the spawned catchup task, which prevents the deadlock where
    /// `run_catchup` blocks on `cmd_tx.send` while `cmd_rx` is suspended
    /// waiting for `handle_gap` to return.
    ///
    /// Callers pass `cmd_tx.clone()` so only platform-adapter senders and
    /// actively-running catchup tasks keep the channel alive.
    pub async fn handle_gap(&mut self, signal: PowerSignal, cmd_tx: mpsc::Sender<IncomingMessage>) {
        // `cmd_tx` is consumed only when the slack feature is compiled in
        // (discord catchup spawns a task that captures other state). The
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
        for p in [&self.telegram, &self.slack, &self.discord]
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
            .active_telegram_chats
            .iter()
            .map(|id| (id.to_string(), PlatformType::Telegram))
            .chain(
                self.active_slack_chats
                    .iter()
                    .map(|id| (id.clone(), PlatformType::Slack)),
            )
            .chain(
                self.active_discord_chats
                    .iter()
                    .map(|id| (id.clone(), PlatformType::Discord)),
            )
            .collect();

        // Two-phase contract: install BEFORE broadcast so a fast delivery
        // task can't race past the install and lose its ack target. See
        // `src/banner.rs` for the full ordering invariant.
        let chat_ids: Vec<String> = all_chats.iter().map(|(id, _)| id.clone()).collect();
        let ack_futures = self.banner.install_pending(&chat_ids).await;

        for (chat_id, platform) in &all_chats {
            let _ = self.stream_tx.send(StreamEvent::GapBanner {
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
        self.banner
            .await_acks_with_fallback(ack_futures, gap, paused_at, resumed_at)
            .await;

        // 3. Slack catchup: fetch missed messages since paused_at.
        //
        // Spawned into a separate task so that `run_catchup`'s `cmd_tx.send`
        // calls don't deadlock against the suspended `cmd_rx` branch in the
        // main `tokio::select!` loop.  The spawned task runs concurrently;
        // adapters are resumed immediately below without waiting for catchup to
        // finish.
        #[cfg(feature = "slack")]
        if let Some(sp) = self.slack_platform.clone() {
            let active: Vec<String> = self.active_slack_chats.iter().cloned().collect();
            if !active.is_empty() {
                // Guard against concurrent catchup tasks (e.g. two rapid GapDetected
                // signals).  If a task is already running, skip rather than spawn a
                // second one — the running task will cover the gap.
                if sp
                    .catchup_in_progress
                    .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                    .is_ok()
                {
                    let oldest_ts = Self::paused_at_to_slack_ts(paused_at);
                    tracing::info!(
                        "Triggering Slack catchup for {} channel(s) from {}",
                        active.len(),
                        oldest_ts
                    );
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
        if let Some(discord) = self.discord_platform.clone() {
            // Guard against concurrent catchup tasks (e.g. two rapid GapDetected
            // signals).  If a task is already running, skip rather than spawn a
            // second one — the running task will cover the gap.
            if discord
                .catchup_in_progress
                .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
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
        for p in [&self.telegram, &self.slack, &self.discord]
            .into_iter()
            .flatten()
        {
            p.resume().await;
        }
        tracing::info!("Adapters resumed after gap handling");
    }

    /// Convert a `DateTime<Utc>` to a Slack-format message timestamp.
    ///
    /// Slack timestamps are `"{unix_seconds}.{microseconds:06}"`.
    /// This format is monotonic per-channel and used as the `oldest` parameter
    /// for `conversations.history` catchup calls.
    #[cfg(feature = "slack")]
    fn paused_at_to_slack_ts(dt: DateTime<Utc>) -> String {
        format!("{}.{:06}", dt.timestamp(), dt.timestamp_subsec_micros())
    }

    /// Apply a `StateUpdate` and let the persistor decide whether to flush
    /// to disk now or defer to the debounce window. Also tracks active chat
    /// IDs as they bind (kept on `App` because the live sets are read by
    /// `handle_gap` and `emit_startup_gap_banners`, neither of which the
    /// persistor knows about).
    pub async fn apply_state_update(&mut self, update: StateUpdate) {
        match &update {
            StateUpdate::BindTelegramChat(id) => {
                self.active_telegram_chats.insert(*id);
            }
            StateUpdate::BindSlackChat(id) => {
                self.active_slack_chats.insert(id.clone());
            }
            StateUpdate::BindDiscordChat(id) => {
                self.active_discord_chats.insert(id.clone());
            }
            _ => {}
        }
        self.state_persistor.apply(update);
    }

    /// Mark last_clean_shutdown=true, force-persist, and wake the retry
    /// worker so it can break out of an idle sleep. Called on graceful
    /// ctrl-c before `cleanup()`.
    pub async fn mark_clean_shutdown(&mut self) {
        self.state_persistor.mark_clean_shutdown();
        // Signal the retry worker to stop after completing its current job.
        // Set the atomic flag FIRST so any next non-blocking poll sees `true`,
        // then notify so an idle/backoff sleep wakes immediately.
        self.retry_worker_shutdown_flag
            .store(true, Ordering::SeqCst);
        self.retry_worker_shutdown.notify_one();
    }

    /// Consume and return the inline gap prefix for `chat_id` if one is
    /// pending.  The delivery task consumes entries directly via
    /// `gap_prefix_handle()`. This pass-through is also called from the
    /// integration test at `tests/sleep_wake_banner.rs` and from the
    /// in-module test below — the `cfg_attr` keeps the dead-code lint
    /// quiet in non-test builds (where neither caller is compiled in)
    /// without weakening the lint elsewhere. Do NOT delete on a "no
    /// callers" sweep; the integration-test caller is real.
    #[cfg_attr(not(test), allow(dead_code))]
    pub async fn consume_gap_prefix(&self, chat_id: &str) -> Option<GapInfo> {
        self.banner.consume_prefix(chat_id).await
    }

    pub async fn handle_command(&mut self, msg: &IncomingMessage) {
        // Ensure MarkDirty is sent on the first real user interaction.
        if self.state_persistor.claim_dirty_send() {
            if let Err(e) = self.state_tx.try_send(StateUpdate::MarkDirty) {
                tracing::warn!("state_tx full, update dropped: {}", e);
            }
        }

        // Emit ChatForward ambient event for chat-origin messages (not socket).
        if msg.socket_request_id.is_none() {
            self.emit_ambient(AmbientEvent::ChatForward {
                platform: format!("{:?}", msg.platform),
                user_id: msg.user_id.clone(),
                text: msg.text.clone(),
            });
        }

        // Skip chat binding for socket-origin messages — they use
        // PlatformType::Telegram as a wire-compat placeholder but should NOT
        // pollute active_telegram_chats or trigger gap banners.
        if msg.reply_context.socket_reply_tx.is_none() {
            // Bind chat IDs for active platform (so we know which chats to banner).
            match msg.reply_context.platform {
                PlatformType::Telegram => {
                    if let Ok(id) = msg.reply_context.chat_id.parse::<i64>() {
                        if self.active_telegram_chats.insert(id) {
                            if let Err(e) =
                                self.state_tx.try_send(StateUpdate::BindTelegramChat(id))
                            {
                                tracing::warn!("state_tx full, update dropped: {}", e);
                            }
                        }
                    }
                }
                PlatformType::Slack => {
                    let id = msg.reply_context.chat_id.clone();
                    if self.active_slack_chats.insert(id.clone()) {
                        if let Err(e) = self.state_tx.try_send(StateUpdate::BindSlackChat(id)) {
                            tracing::warn!("state_tx full, update dropped: {}", e);
                        }
                    }
                }
                PlatformType::Discord => {
                    let id = msg.reply_context.chat_id.clone();
                    if self.active_discord_chats.insert(id.clone()) {
                        if let Err(e) = self.state_tx.try_send(StateUpdate::BindDiscordChat(id)) {
                            tracing::warn!("state_tx full, update dropped: {}", e);
                        }
                    }
                }
            }
        }

        let cmd = match ParsedCommand::parse(&msg.text, self.trigger) {
            Ok(cmd) => cmd,
            Err(e) => {
                self.send_error(&msg.reply_context, &format!("{:#}", e))
                    .await;
                return;
            }
        };

        // Multi-line StdinInput guard: the parser allows multi-line StdinInput,
        // but the dispatcher must check harness state before routing to tmux.
        if let ParsedCommand::StdinInput { ref text } = cmd {
            if (text.contains('\n') || text.contains('\r'))
                && self.session_mgr.foreground_harness().is_none()
            {
                self.send_error(
                    &msg.reply_context,
                    &format!(
                        "Multi-line input requires harness mode. \
                         Use `{} claude on` to enable, or prefix with `{} claude <prompt>` for one-off.",
                        self.trigger, self.trigger
                    ),
                )
                .await;
                return;
            }
        }

        tracing::info!(
            "handle_command: {:?} (fg={:?})",
            cmd,
            self.session_mgr.foreground_session()
        );

        // Always bind the foreground session to the current chat context.
        // This ensures delivery tasks know where to send output — critical after
        // restart (reconnected sessions have no chat binding) and for cross-platform use.
        if let Some(fg) = self.session_mgr.foreground_session() {
            let _ = self.stream_tx.send(StreamEvent::SessionStarted {
                session: fg.to_string(),
                chat_id: msg.reply_context.chat_id.clone(),
                thread_ts: msg.reply_context.thread_ts.clone(),
            });
        }

        match cmd {
            ParsedCommand::ShellCommand { ref cmd } if self.blocklist.is_blocked(cmd) => {
                self.send_error(&msg.reply_context, "Command blocked by security policy")
                    .await;
            }
            ParsedCommand::NewSession { name } => match self.session_mgr.new_session(&name).await {
                Ok(()) => {
                    let mut buf = OutputBuffer::new(&name, self.offline_buffer_max);
                    buf.sync_offset(self.session_mgr.tmux()).await;
                    self.buffers.insert(name.clone(), buf);
                    let _ = self.stream_tx.send(StreamEvent::SessionStarted {
                        session: name.clone(),
                        chat_id: msg.reply_context.chat_id.clone(),
                        thread_ts: msg.reply_context.thread_ts.clone(),
                    });
                    self.emit_ambient(AmbientEvent::SessionCreated {
                        session: name.clone(),
                        origin_chat: Some(msg.reply_context.chat_id.clone()),
                        created_at: chrono::Utc::now(),
                    });
                    self.send_reply(&msg.reply_context, &format!("Session '{}' created", name))
                        .await;
                }
                Err(e) => {
                    // Check if the error is a session-limit error and emit ambient.
                    let err_str = format!("{:#}", e);
                    if err_str.contains("Maximum session limit") {
                        self.emit_ambient(AmbientEvent::SessionLimitReached {
                            attempted: name.clone(),
                            current: self.session_mgr.session_count(),
                            max: self.session_mgr.max_sessions(),
                        });
                    }
                    self.send_error(&msg.reply_context, &err_str).await;
                }
            },
            ParsedCommand::Foreground { name } => match self.session_mgr.fg(&name) {
                Ok(()) => {
                    let _ = self.stream_tx.send(StreamEvent::SessionStarted {
                        session: name.clone(),
                        chat_id: msg.reply_context.chat_id.clone(),
                        thread_ts: msg.reply_context.thread_ts.clone(),
                    });
                    self.send_reply(
                        &msg.reply_context,
                        &format!("Session '{}' foregrounded", name),
                    )
                    .await;
                }
                Err(e) => {
                    self.send_error(&msg.reply_context, &format!("{:#}", e))
                        .await;
                }
            },
            ParsedCommand::Background => match self.session_mgr.bg() {
                Ok(Some(name)) => {
                    self.send_reply(
                        &msg.reply_context,
                        &format!("Session '{}' backgrounded", name),
                    )
                    .await;
                }
                Ok(None) => {
                    self.send_error(&msg.reply_context, "No foreground session to background")
                        .await;
                }
                Err(e) => {
                    self.send_error(&msg.reply_context, &format!("{:#}", e))
                        .await;
                }
            },
            ParsedCommand::ListSessions => {
                let sessions = self.session_mgr.list();
                if sessions.is_empty() {
                    self.send_reply(&msg.reply_context, "No active sessions")
                        .await;
                } else {
                    let mut lines = Vec::new();
                    for (name, status, created) in &sessions {
                        let status_str = match status {
                            session::SessionStatus::Foreground => "[foreground]",
                            session::SessionStatus::Background => "[background]",
                        };
                        let elapsed = created.elapsed();
                        lines.push(format!(
                            "  {} {} (uptime: {}s)",
                            name,
                            status_str,
                            elapsed.as_secs()
                        ));
                    }
                    self.send_reply(&msg.reply_context, &lines.join("\n")).await;
                }
            }
            ParsedCommand::Screen => match self.session_mgr.foreground_session() {
                Some(fg) => match self.session_mgr.tmux().capture_pane(fg).await {
                    Ok(screen) => {
                        let trimmed = screen
                            .lines()
                            .map(|l| l.trim_end())
                            .collect::<Vec<_>>()
                            .join("\n")
                            .trim()
                            .to_string();
                        if trimmed.is_empty() {
                            self.send_reply(&msg.reply_context, "(empty screen)").await;
                        } else {
                            for chunk in split_message(&trimmed, 4000) {
                                self.send_reply(&msg.reply_context, &chunk).await;
                            }
                        }
                    }
                    Err(e) => {
                        self.send_error(&msg.reply_context, &format!("{:#}", e))
                            .await;
                    }
                },
                None => {
                    self.send_error(&msg.reply_context, "No active session")
                        .await;
                }
            },
            ParsedCommand::KillSession { name } => match self.session_mgr.kill(&name).await {
                Ok(()) => {
                    self.buffers.remove(&name);
                    self.emit_ambient(AmbientEvent::SessionKilled {
                        session: name.clone(),
                        reason: "user_request".to_string(),
                        killed_at: chrono::Utc::now(),
                    });
                    self.send_reply(&msg.reply_context, &format!("Session '{}' killed", name))
                        .await;
                }
                Err(e) => {
                    self.send_error(&msg.reply_context, &format!("{:#}", e))
                        .await;
                }
            },
            ParsedCommand::HarnessOn {
                harness: kind,
                options,
                initial_prompt,
            } => {
                // Verify the harness is actually registered (stubs are not)
                if !self.harness_registry.contains_kind(&kind) {
                    self.send_error(
                        &msg.reply_context,
                        &format!("{} harness is not available", kind.name()),
                    )
                    .await;
                    return;
                }
                let fg = match self.session_mgr.foreground_session() {
                    Some(fg) => fg.to_string(),
                    None => {
                        self.send_error(&msg.reply_context, "No active session")
                            .await;
                        return;
                    }
                };
                // Validate --name/--resume on non-resumable harnesses.
                // Dispatch context built inline so its field-level borrows
                // end after the reject call, freeing `&mut self.session_mgr`
                // for the `set_harness` calls further down.
                {
                    let dispatch = PromptDispatchContext {
                        session_mgr: &self.session_mgr,
                        telegram: self.telegram.as_deref(),
                        slack: self.slack.as_deref(),
                        discord: self.discord.as_deref(),
                        schema_registry: &self.schema_registry,
                        delivery_queue: &self.delivery_queue,
                        webhook_client: &self.webhook_client,
                        stream_tx: &self.stream_tx,
                        ambient_tx: &self.ambient_tx,
                    };
                    if self
                        .harness_registry
                        .reject_if_not_resumable(&msg.reply_context, &kind, &options, &dispatch)
                        .await
                    {
                        return;
                    }
                }

                // Resolve named session for --name/--resume. We do NOT pre-create
                // an empty-session_id entry here — persistence happens only after
                // the first prompt returns a real session_id. This keeps the
                // state file free of "zombie" entries when the user runs
                // `claude on --name foo` then exits without prompting, and
                // avoids the "exists but has no conversation yet" dead-end on
                // a subsequent `--resume foo`.
                let mut named_notification: Option<String> = None;
                if let Some(ref resume_name) = options.resume {
                    match self
                        .harness_registry
                        .lookup_named_session(kind, resume_name)
                    {
                        Some(entry) if !entry.session_id.is_empty() => {
                            named_notification =
                                Some(format!("Resuming session '{}'", resume_name));
                        }
                        _ => {
                            self.send_error(
                                &msg.reply_context,
                                &format!(
                                    "No session named '{}'. Use --name to create one.",
                                    resume_name
                                ),
                            )
                            .await;
                            return;
                        }
                    }
                } else if let Some(ref name) = options.name {
                    let is_resumable = self
                        .harness_registry
                        .lookup_named_session(kind, name)
                        .is_some_and(|e| !e.session_id.is_empty());
                    let primary = if is_resumable {
                        format!("Resuming existing session '{}'", name)
                    } else {
                        format!("Created new session '{}'", name)
                    };
                    // Only hint on creation, not on resume — if the user is
                    // resuming a numeric name, they clearly know it exists.
                    let hint = (!is_resumable).then(|| numeric_name_hint(name)).flatten();
                    named_notification = Some(match hint {
                        Some(h) => format!("{}\n{}", primary, h),
                        None => primary,
                    });
                }

                // Check if switching from another harness or updating options
                if let Some(current) = self.session_mgr.foreground_harness() {
                    if current == kind {
                        // Same harness re-activated — check if named session changed
                        let new_session_name =
                            options.name.as_deref().or(options.resume.as_deref());
                        let prev_resolved = self
                            .session_mgr
                            .foreground_named_session_resolved()
                            .map(|s| s.to_string());

                        self.session_mgr
                            .set_harness(&fg, Some(kind), options.clone());

                        // Update named_session_resolved if session name changed
                        if new_session_name.map(|s| s.to_string()) != prev_resolved {
                            self.session_mgr.set_named_session_resolved(
                                new_session_name.map(|s| s.to_string()),
                            );
                            if let Some(note) = named_notification {
                                self.send_reply(&msg.reply_context, &note).await;
                            }
                        }

                        let opts_msg = if options.is_empty() {
                            format!("{} mode: options reset to defaults.", kind.name())
                        } else {
                            format!(
                                "{} mode: options updated.\nOptions: {}",
                                kind.name(),
                                options.summary()
                            )
                        };
                        self.send_reply(&msg.reply_context, &opts_msg).await;
                        // Fire the initial prompt, if provided (e.g.
                        // `: claude on --resume review please look at the bag`).
                        if let Some(ref prompt) = initial_prompt {
                            self.send_harness_prompt(
                                &msg.reply_context,
                                &kind,
                                prompt,
                                &msg.attachments,
                                &options,
                            )
                            .await;
                        }
                        return;
                    }
                    // Check resume support of the CURRENT harness before switching away
                    if let Some(h) = self.harness_registry.harness_for(&current) {
                        if !h.supports_resume() {
                            self.send_error(
                                &msg.reply_context,
                                &format!(
                                    "{} is active but doesn't support resume. Run `{} {} off` first to avoid losing context.",
                                    current.name(),
                                    self.trigger,
                                    current.name().to_lowercase()
                                ),
                            )
                            .await;
                            return;
                        }
                    }
                    self.send_reply(
                        &msg.reply_context,
                        &format!(
                            "Switching from {} to {} ({} session preserved)",
                            current.name(),
                            kind.name(),
                            current.name()
                        ),
                    )
                    .await;
                }
                // Send named session notification before the ON message
                if let Some(note) = named_notification {
                    self.send_reply(&msg.reply_context, &note).await;
                }

                let opts_summary = if options.is_empty() {
                    String::new()
                } else {
                    format!("\nOptions: {}", options.summary())
                };
                // Track the resolved named session name for notification suppression
                let resolved_name = options
                    .name
                    .as_deref()
                    .or(options.resume.as_deref())
                    .map(|s| s.to_string());
                // Keep a copy for the optional initial prompt, since
                // `set_harness` moves `options` into session state.
                let options_for_prompt = initial_prompt.as_ref().map(|_| options.clone());
                self.session_mgr.set_harness(&fg, Some(kind), options);
                self.session_mgr.set_named_session_resolved(resolved_name);
                self.send_reply(
                    &msg.reply_context,
                    &format!(
                        "{} mode ON. Plain text now goes to {}. Use `{} {} off` to switch back.{}",
                        kind.name(),
                        kind.name(),
                        self.trigger,
                        kind.name().to_lowercase(),
                        opts_summary
                    ),
                )
                .await;
                // Fire the initial prompt, if provided.
                if let (Some(prompt), Some(opts)) = (initial_prompt, options_for_prompt) {
                    self.send_harness_prompt(
                        &msg.reply_context,
                        &kind,
                        &prompt,
                        &msg.attachments,
                        &opts,
                    )
                    .await;
                }
            }
            ParsedCommand::HarnessOff { harness: kind } => {
                let fg = match self.session_mgr.foreground_session() {
                    Some(fg) => fg.to_string(),
                    None => {
                        self.send_error(&msg.reply_context, "No active session")
                            .await;
                        return;
                    }
                };
                // Verify the specified harness is actually the active one
                let current = self.session_mgr.foreground_harness();
                if current != Some(kind) {
                    self.send_error(
                        &msg.reply_context,
                        &format!(
                            "{} is not active{}",
                            kind.name(),
                            current
                                .map(|c| format!(" (current: {})", c.name()))
                                .unwrap_or_default()
                        ),
                    )
                    .await;
                    return;
                }
                self.session_mgr
                    .set_harness(&fg, None, HarnessOptions::default());
                self.send_reply(
                    &msg.reply_context,
                    &format!(
                        "{} mode OFF. Plain text now goes to the terminal session.",
                        kind.name()
                    ),
                )
                .await;
            }
            ParsedCommand::HarnessPrompt {
                harness: kind,
                ref prompt,
                ref options,
            } => {
                if self.blocklist.is_blocked(prompt) {
                    self.send_error(
                        &msg.reply_context,
                        &format!("{} prompt blocked by security policy", kind.name()),
                    )
                    .await;
                } else {
                    // One-shot prompt (`: claude --schema=foo <prompt>`): use parsed options.
                    self.send_harness_prompt(
                        &msg.reply_context,
                        &kind,
                        prompt,
                        &msg.attachments,
                        options,
                    )
                    .await;
                }
            }
            ParsedCommand::HarnessSubcommand {
                harness,
                subcommand,
                args,
            } => {
                let hctx = HarnessContext {
                    ctx: &msg.reply_context,
                    telegram: self.telegram.as_deref(),
                    slack: self.slack.as_deref(),
                    discord: self.discord.as_deref(),
                    schema_registry: &self.schema_registry,
                    delivery_queue: &self.delivery_queue,
                    webhook_client: &self.webhook_client,
                    stream_tx: &self.stream_tx,
                };
                // Route to the per-harness subcommand runner. Both runners
                // emit the same Text+Done / Error+Done shape that
                // drive_harness consumes — subcommands don't create sessions
                // and don't fire ambient HarnessStarted/Finished events.
                let setup = match (harness, subcommand) {
                    (HarnessKind::Opencode, HarnessSubcommandKind::Opencode(sub)) => {
                        self.harness_registry
                            .opencode()
                            .run_subcommand(sub, args)
                            .await
                    }
                    (HarnessKind::Gemini, HarnessSubcommandKind::Gemini(sub)) => {
                        self.harness_registry
                            .gemini()
                            .run_subcommand(sub, args)
                            .await
                    }
                    (HarnessKind::Codex, HarnessSubcommandKind::Codex(sub)) => {
                        self.harness_registry
                            .codex()
                            .run_subcommand(sub, args)
                            .await
                    }
                    (h, _) => {
                        self.send_error(
                            &msg.reply_context,
                            &format!(
                                "HarnessSubcommand kind mismatch for {} — internal bug",
                                h.name()
                            ),
                        )
                        .await;
                        return;
                    }
                };
                match setup {
                    Ok(event_rx) => {
                        let _ = drive_harness(event_rx, &hctx).await;
                    }
                    Err(e) => {
                        self.send_error(
                            &msg.reply_context,
                            &format!("{} subcommand setup failed: {}", harness.name(), e),
                        )
                        .await;
                    }
                }
            }
            ParsedCommand::ShellCommand { cmd } => {
                // Snapshot pane before command so we can diff afterward
                match self.session_mgr.foreground_session() {
                    None => {
                        tracing::warn!("ShellCommand '{}': no foreground session", cmd);
                    }
                    Some(fg) => {
                        if let Some(buf) = self.buffers.get_mut(fg) {
                            buf.snapshot_before_command(self.session_mgr.tmux(), Some(&cmd))
                                .await;
                            tracing::debug!("ShellCommand '{}': snapshot taken", cmd);
                        } else {
                            tracing::warn!(
                                "ShellCommand '{}': no buffer for session '{}', output won't be captured",
                                cmd, fg
                            );
                        }
                    }
                }
                if let Err(e) = self.session_mgr.execute_in_foreground(&cmd).await {
                    self.send_error(&msg.reply_context, &format!("{:#}", e))
                        .await;
                }
            }
            ParsedCommand::StdinInput { text } => {
                // Single blocklist check regardless of routing
                if self.blocklist.is_blocked(&text) {
                    self.send_error(&msg.reply_context, "Command blocked by security policy")
                        .await;
                    return;
                }
                if let Some(kind) = self.session_mgr.foreground_harness() {
                    // Route to active harness with stored session options
                    let opts = self.session_mgr.foreground_harness_options();
                    self.send_harness_prompt(
                        &msg.reply_context,
                        &kind,
                        &text,
                        &msg.attachments,
                        &opts,
                    )
                    .await;
                } else {
                    // Images are only supported in harness mode
                    if !msg.attachments.is_empty() {
                        self.send_error(
                            &msg.reply_context,
                            &format!("Images are only supported in harness mode. Use `{} claude on` first.", self.trigger),
                        )
                        .await;
                        return;
                    }
                    // Route to terminal
                    if let Some(fg) = self.session_mgr.foreground_session() {
                        if let Some(buf) = self.buffers.get_mut(fg) {
                            buf.snapshot_before_command(self.session_mgr.tmux(), Some(&text))
                                .await;
                        }
                    }
                    if let Err(e) = self.session_mgr.send_stdin_to_foreground(&text).await {
                        self.send_error(&msg.reply_context, &format!("{:#}", e))
                            .await;
                    }
                }
            }
        }
    }

    pub async fn health_check(&mut self) {
        let crashed = self.session_mgr.health_check().await;
        for (name, code) in crashed {
            self.buffers.remove(&name);
            let _ = self.stream_tx.send(StreamEvent::SessionExited {
                session: name,
                code,
            });
        }
        // Emit a Tick to keep last_seen_wall fresh for restart-gap computation.
        if let Err(e) = self.state_tx.try_send(StateUpdate::Tick) {
            tracing::warn!("state_tx full, update dropped: {}", e);
        }
    }

    pub async fn poll_output(&mut self) {
        // Only poll the foreground session — keeps cost low (one tmux subprocess per tick)
        if let Some(fg) = self.session_mgr.foreground_session() {
            if let Some(buffer) = self.buffers.get_mut(fg) {
                let tmux = self.session_mgr.tmux();
                let events = buffer.poll(tmux).await;
                if !events.is_empty() {
                    tracing::info!("[poll] {} event(s) from session '{}'", events.len(), fg);
                }
                for event in events {
                    let _ = self.stream_tx.send(event);
                }
            }
        }
    }

    pub async fn cleanup(&mut self) {
        self.session_mgr.cleanup_all().await;
    }

    /// Send Ctrl+C interrupt to the foreground tmux session.
    /// Called by the main loop when a socket cancel request is received.
    pub async fn interrupt_foreground(&self) -> Result<()> {
        self.session_mgr.interrupt_foreground().await
    }

    /// Pass-through to [`HarnessRegistry::send_harness_prompt`]. Builds
    /// [`PromptDispatchContext`] inline (rather than via a getter on `&self`)
    /// so the borrow checker can verify field-level disjoint borrows: the
    /// dispatch context immutably borrows `session_mgr/telegram/slack/.../ambient_tx`,
    /// and the registry call mutably borrows `harness_registry` — disjoint
    /// fields, no conflict.
    async fn send_harness_prompt(
        &mut self,
        ctx: &ReplyContext,
        kind: &HarnessKind,
        prompt: &str,
        attachments: &[Attachment],
        options: &HarnessOptions,
    ) {
        let dispatch = PromptDispatchContext {
            session_mgr: &self.session_mgr,
            telegram: self.telegram.as_deref(),
            slack: self.slack.as_deref(),
            discord: self.discord.as_deref(),
            schema_registry: &self.schema_registry,
            delivery_queue: &self.delivery_queue,
            webhook_client: &self.webhook_client,
            stream_tx: &self.stream_tx,
            ambient_tx: &self.ambient_tx,
        };
        self.harness_registry
            .send_harness_prompt(ctx, kind, prompt, attachments, options, &dispatch)
            .await;
    }

    async fn send_reply(&self, ctx: &ReplyContext, text: &str) {
        // Socket-origin: route to the per-request response channel.
        if let Some(ref tx) = ctx.socket_reply_tx {
            let _ = tx.send(text.to_string());
            return;
        }
        // Chat-origin: route to the platform adapter.
        let platform: Option<&dyn ChatPlatform> = match ctx.platform {
            PlatformType::Telegram => self.telegram.as_deref(),
            PlatformType::Slack => self.slack.as_deref(),
            PlatformType::Discord => self.discord.as_deref(),
        };
        if let Some(p) = platform {
            if let Err(e) = p
                .send_message(text, &ctx.chat_id, ctx.thread_ts.as_deref())
                .await
            {
                tracing::error!("Failed to send reply: {}", e);
            }
        }
    }

    async fn send_error(&self, ctx: &ReplyContext, error: &str) {
        self.send_reply(ctx, &format!("Error: {}", error)).await;
    }
}

/// If a session name parses as a plain `u32`, return a hint explaining that
/// `-n` now means `--name` (not `--max-turns`). `-n 5` is still accepted as
/// a valid session name, but the user very likely meant `-t 5`.
///
/// Fires only on session *creation*, so resuming an existing numeric name
/// stays silent. Mirrors `harness_registry::numeric_name_hint` (SYNC: keep
/// both copies in lock-step — `App::handle_command`'s HarnessOn branch and
/// `HarnessRegistry::send_harness_prompt` each emit this hint, and a behavior
/// change to one must propagate to the other).
fn numeric_name_hint(name: &str) -> Option<String> {
    name.parse::<u32>().ok().map(|_| {
        format!(
            "Hint: '{name}' is a valid session name, but if you meant --max-turns use `-t {name}` (the `-n` short flag is now --name).",
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::state_store::StateStore;
    use tempfile::tempdir;
    use tokio::sync::oneshot;

    fn make_test_config(dir: &std::path::Path) -> Config {
        // Write a minimal valid terminus.toml to the temp dir.
        let toml_path = dir.join("terminus.toml");
        std::fs::write(
            &toml_path,
            r#"
[auth]
telegram_user_id = 12345

[telegram]
bot_token = "test_token_that_is_not_real"

[blocklist]
patterns = []
"#,
        )
        .unwrap();
        Config::load(&toml_path).expect("test config load")
    }

    fn make_app(dir: &std::path::Path) -> (App, mpsc::Receiver<StateUpdate>) {
        let config = make_test_config(dir);
        let state_path = dir.join("terminus-state.json");
        let store = StateStore::load(&state_path).expect("load state");
        let (state_tx, state_rx) = mpsc::channel::<StateUpdate>(64);
        let app = App::new(&config, store, state_tx).expect("App::new");
        (app, state_rx)
    }

    // ─── Sleep/wake recovery tests ───────────────────────────────────────────
    //
    // The pure-persistor cases (`apply_*` debounce, `mark_clean_shutdown`)
    // live in `src/state_persistor.rs` next to the type they exercise. The
    // App-level integration tests below pin behavior that the persistor
    // can't see in isolation: chat-set tracking, the startup MarkDirty
    // happening through the App constructor, and the force-persist path
    // when reached via `apply_state_update`.

    #[tokio::test]
    async fn app_new_marks_dirty_in_memory() {
        let dir = tempdir().unwrap();
        let (app, _state_rx) = make_app(dir.path());
        // App::new applies MarkDirty immediately via the persistor.
        assert!(
            !app.state_persistor.snapshot().last_clean_shutdown,
            "App::new should mark state dirty (last_clean_shutdown=false)"
        );
    }

    #[tokio::test]
    async fn restart_banner_gate_fires_on_unclean_gap() {
        let dir = tempdir().unwrap();

        // Write a state file with last_clean_shutdown=false and old last_seen_wall.
        let old_time = Utc::now() - chrono::Duration::seconds(120);
        let state = serde_json::json!({
            "schema_version": 1,
            "telegram": { "offset": 0 },
            "chats": { "telegram": [111i64], "slack": [] },
            "last_seen_wall": old_time.to_rfc3339(),
            "last_clean_shutdown": false
        });
        let state_path = dir.path().join("terminus-state.json");
        std::fs::write(&state_path, state.to_string()).unwrap();

        let config = make_test_config(dir.path());
        let store = StateStore::load(&state_path).unwrap();
        let (state_tx, _state_rx) = mpsc::channel::<StateUpdate>(64);
        let mut app = App::new(&config, store, state_tx).unwrap();

        // Subscribe before emitting.
        let mut sub = app.subscribe_stream();

        app.emit_startup_gap_banners();

        // Should receive a GapBanner for chat 111.
        let event = tokio::time::timeout(Duration::from_millis(100), sub.recv())
            .await
            .expect("timeout waiting for GapBanner")
            .expect("channel error");

        match event {
            StreamEvent::GapBanner { chat_id, .. } => {
                assert_eq!(chat_id, "111");
            }
            other => panic!("expected GapBanner, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn restart_banner_gate_no_fire_on_clean_shutdown() {
        let dir = tempdir().unwrap();

        // Write a state file with last_clean_shutdown=true.
        let old_time = Utc::now() - chrono::Duration::seconds(120);
        let state = serde_json::json!({
            "schema_version": 1,
            "telegram": { "offset": 0 },
            "chats": { "telegram": [111i64], "slack": [] },
            "last_seen_wall": old_time.to_rfc3339(),
            "last_clean_shutdown": true
        });
        std::fs::write(dir.path().join("terminus-state.json"), state.to_string()).unwrap();

        let config = make_test_config(dir.path());
        let store = StateStore::load(dir.path().join("terminus-state.json")).unwrap();
        let (state_tx, _state_rx) = mpsc::channel::<StateUpdate>(64);
        let mut app = App::new(&config, store, state_tx).unwrap();

        let mut sub = app.subscribe_stream();
        app.emit_startup_gap_banners();

        // Should NOT receive any GapBanner.
        let result = tokio::time::timeout(Duration::from_millis(50), sub.recv()).await;
        assert!(result.is_err(), "no GapBanner expected for clean shutdown");
    }

    #[tokio::test]
    async fn restart_banner_gate_no_fire_on_small_gap() {
        let dir = tempdir().unwrap();

        // last_seen_wall only 10s ago — below 30s threshold.
        let recent = Utc::now() - chrono::Duration::seconds(10);
        let state = serde_json::json!({
            "schema_version": 1,
            "telegram": { "offset": 0 },
            "chats": { "telegram": [111i64], "slack": [] },
            "last_seen_wall": recent.to_rfc3339(),
            "last_clean_shutdown": false
        });
        std::fs::write(dir.path().join("terminus-state.json"), state.to_string()).unwrap();

        let config = make_test_config(dir.path());
        let store = StateStore::load(dir.path().join("terminus-state.json")).unwrap();
        let (state_tx, _state_rx) = mpsc::channel::<StateUpdate>(64);
        let mut app = App::new(&config, store, state_tx).unwrap();

        let mut sub = app.subscribe_stream();
        app.emit_startup_gap_banners();

        let result = tokio::time::timeout(Duration::from_millis(50), sub.recv()).await;
        assert!(result.is_err(), "no GapBanner expected for gap < 30s");
    }

    #[tokio::test]
    async fn delivery_resolves_banner_ack_directly() {
        // Regression test for the Architect-identified deadlock: the delivery
        // task must resolve the pending oneshot *directly* via the shared
        // map, not via an mpsc the main loop drains.
        let dir = tempdir().unwrap();
        let (app, _state_rx) = make_app(dir.path());

        let (tx, rx) = oneshot::channel::<()>();
        let acks = app.pending_banner_acks_handle();
        acks.lock().await.insert("chat123".to_string(), tx);

        // Delivery-task-style resolution: lock briefly, remove, send.
        {
            let removed = acks.lock().await.remove("chat123");
            assert!(removed.is_some(), "sender should be registered");
            let _ = removed.unwrap().send(());
        }

        assert!(rx.await.is_ok(), "oneshot should be resolved directly");
        assert!(
            acks.lock().await.is_empty(),
            "pending_banner_acks should be cleared"
        );
    }

    #[tokio::test]
    async fn handle_gap_completes_when_delivery_resolves_directly() {
        // End-to-end regression test for the Architect-identified deadlock.
        let dir = tempdir().unwrap();
        let (mut app, _state_rx) = make_app(dir.path());

        // Seed an active Telegram chat so handle_gap has a chat to emit for.
        app.active_telegram_chats.insert(7777i64);

        // A background task imitating the delivery task: subscribe to the
        // stream, and whenever a GapBanner arrives, resolve the matching
        // pending-ack oneshot via the shared handle.
        let acks = app.pending_banner_acks_handle();
        let mut rx = app.subscribe_stream();
        let delivery_task = tokio::spawn(async move {
            while let Ok(event) = rx.recv().await {
                if let StreamEvent::GapBanner { chat_id, .. } = event {
                    if let Some(tx) = acks.lock().await.remove(&chat_id) {
                        let _ = tx.send(());
                    }
                    return;
                }
            }
        });

        let signal = PowerSignal::GapDetected {
            paused_at: Utc::now() - chrono::Duration::seconds(90),
            resumed_at: Utc::now(),
            gap: Duration::from_secs(90),
        };

        let (cmd_tx, _cmd_rx) = mpsc::channel::<IncomingMessage>(16);
        let result =
            tokio::time::timeout(Duration::from_secs(1), app.handle_gap(signal, cmd_tx)).await;
        assert!(
            result.is_ok(),
            "handle_gap deadlocked or timed out beyond the 1s bound"
        );

        // No gap_prefix fallback should have been engaged.
        assert!(
            app.gap_prefix_handle().lock().await.is_empty(),
            "gap_prefix should be empty when the ack resolved in time"
        );

        let _ = tokio::time::timeout(Duration::from_millis(100), delivery_task).await;
    }

    /// Pins the App-level pass-through to `BannerCoordinator::consume_prefix`.
    /// The banner mechanics themselves are unit-tested in `src/banner.rs::tests`;
    /// this case keeps `App::consume_gap_prefix` reachable so dead-code lint
    /// doesn't trip in the binary's test target (integration tests at
    /// `tests/sleep_wake_banner.rs` are compiled separately).
    #[tokio::test]
    async fn consume_gap_prefix_pass_through_returns_registered_entry() {
        let dir = tempdir().unwrap();
        let (app, _state_rx) = make_app(dir.path());

        let now = Utc::now();
        app.gap_prefix_handle().lock().await.insert(
            "chat42".to_string(),
            GapInfo {
                gap: Duration::from_secs(90),
                paused_at: now - chrono::Duration::seconds(90),
                resumed_at: now,
            },
        );

        let prefix = app.consume_gap_prefix("chat42").await;
        assert!(prefix.is_some());
        assert_eq!(prefix.unwrap().gap, Duration::from_secs(90));
        assert!(app.consume_gap_prefix("chat42").await.is_none());
    }

    #[tokio::test]
    async fn handle_command_discord_binds_chat_and_emits_state_update() {
        let dir = tempdir().unwrap();
        let (mut app, mut state_rx) = make_app(dir.path());

        let msg = IncomingMessage {
            user_id: "999".to_string(),
            text: ": ls".to_string(),
            platform: PlatformType::Discord,
            reply_context: ReplyContext {
                platform: PlatformType::Discord,
                chat_id: "discord_channel_42".to_string(),
                thread_ts: None,
                socket_reply_tx: None,
            },
            attachments: vec![],
            socket_request_id: None,
            socket_client_name: None,
        };

        app.handle_command(&msg).await;

        // Check in-memory set was updated.
        assert!(
            app.active_discord_chats.contains("discord_channel_42"),
            "active_discord_chats should contain the chat ID"
        );

        // Drain state_rx to find BindDiscordChat.
        // First update is MarkDirty (from handle_command's dirty_sent guard).
        let mut found_bind = false;
        while let Ok(update) = state_rx.try_recv() {
            if let StateUpdate::BindDiscordChat(id) = update {
                assert_eq!(id, "discord_channel_42");
                found_bind = true;
                break;
            }
        }
        assert!(
            found_bind,
            "expected StateUpdate::BindDiscordChat to be emitted"
        );
    }

    // ─── Named session / LRU tests ───────────────────────────────────────────
    //
    // `evict_lru_session_*` and `persist_named_session_*` cases moved to
    // `src/harness_registry.rs::tests` next to the type that owns the
    // named-session map. The App-level pass-throughs are exercised via
    // `harness_session_batch_force_persists_immediately` (below) and via
    // the `send_harness_prompt` integration tests further down.

    #[tokio::test]
    async fn harness_session_batch_force_persists_immediately() {
        // Regression: batches must not wait for the 10-update / 5s debounce,
        // otherwise an unclean exit drops the session before it hits disk and
        // a subsequent `--resume` errors "No session named …".
        let dir = tempdir().unwrap();
        let (mut app, _state_rx) = make_app(dir.path());

        let entry = NamedSessionEntry {
            session_id: "sid-1".into(),
            cwd: std::path::PathBuf::from("/tmp"),
            last_used: Utc::now(),
        };
        app.apply_state_update(StateUpdate::HarnessSessionBatch(vec![(
            "auth".into(),
            Some(entry),
        )]))
        .await;

        // Counter should have been reset by the force-persist.
        assert_eq!(
            app.state_persistor.updates_since_persist(),
            0,
            "HarnessSessionBatch must bypass the debounce window"
        );

        // And the entry should be visible on a fresh reload from disk.
        let reloaded = StateStore::load(dir.path().join("terminus-state.json")).unwrap();
        assert!(
            reloaded.snapshot().harness_sessions.contains_key("auth"),
            "batch should be on disk after a single apply_state_update"
        );
    }

    #[tokio::test]
    async fn named_session_round_trips_through_state_store() {
        let dir = tempdir().unwrap();
        let state_path = dir.path().join("terminus-state.json");

        // Seed the store with a batch, persist, then reload from disk.
        {
            let mut store = StateStore::load(&state_path).unwrap();
            let entry = NamedSessionEntry {
                session_id: "sid-auth".into(),
                cwd: std::path::PathBuf::from("/tmp/project"),
                last_used: Utc::now(),
            };
            store.apply(StateUpdate::HarnessSessionBatch(vec![(
                "auth".into(),
                Some(entry),
            )]));
            store.persist().unwrap();
        }

        // Second App::new should see the persisted entry in its index.
        let config = make_test_config(dir.path());
        let store = StateStore::load(&state_path).unwrap();
        let (state_tx, _state_rx) = mpsc::channel::<StateUpdate>(64);
        let app = App::new(&config, store, state_tx).unwrap();
        assert_eq!(app.harness_registry.named_sessions_len(), 1);
        let entry = app
            .harness_registry
            .named_sessions_get("auth")
            .expect("auth entry should survive reload");
        assert_eq!(entry.session_id, "sid-auth");
        assert_eq!(entry.cwd, std::path::PathBuf::from("/tmp/project"));
    }

    #[tokio::test]
    async fn reject_if_not_resumable_is_noop_without_flags() {
        let dir = tempdir().unwrap();
        let (app, _state_rx) = make_app(dir.path());
        let ctx = ReplyContext {
            platform: PlatformType::Telegram,
            chat_id: "0".into(),
            thread_ts: None,
            socket_reply_tx: None,
        };
        let opts = HarnessOptions::default();
        let dispatch = PromptDispatchContext {
            session_mgr: &app.session_mgr,
            telegram: app.telegram.as_deref(),
            slack: app.slack.as_deref(),
            discord: app.discord.as_deref(),
            schema_registry: &app.schema_registry,
            delivery_queue: &app.delivery_queue,
            webhook_client: &app.webhook_client,
            stream_tx: &app.stream_tx,
            ambient_tx: &app.ambient_tx,
        };
        assert!(
            !app.harness_registry
                .reject_if_not_resumable(&ctx, &HarnessKind::Claude, &opts, &dispatch)
                .await
        );
    }

    // `current_dir_or_dot_never_returns_empty_path` migrated to
    // `harness_registry::tests` along with the function.

    #[test]
    fn numeric_name_hint_fires_on_plain_integer() {
        let hint = super::numeric_name_hint("5").expect("should produce a hint");
        assert!(
            hint.contains("--max-turns"),
            "hint should mention --max-turns"
        );
        assert!(
            hint.contains("-t 5"),
            "hint should suggest the -t short flag"
        );
    }

    #[test]
    fn numeric_name_hint_silent_for_alphanumeric_names() {
        assert!(super::numeric_name_hint("auth").is_none());
        assert!(super::numeric_name_hint("v1").is_none());
        assert!(super::numeric_name_hint("5auth").is_none());
        assert!(super::numeric_name_hint("auth-5").is_none());
    }

    #[test]
    fn numeric_name_hint_silent_for_oversized_number() {
        // u32::MAX is 4294967295; anything larger overflows the parse.
        assert!(super::numeric_name_hint("99999999999999999999").is_none());
    }

    // ─── Mock harness + end-to-end HarnessOn/initial_prompt coverage ─────────

    /// Records each `run_prompt` call so tests can assert the App wired
    /// through the expected prompt / session_id.
    #[derive(Clone)]
    struct RecordingHarness {
        calls: Arc<std::sync::Mutex<Vec<RecordingCall>>>,
        kind: crate::harness::HarnessKind,
    }

    #[derive(Clone, Debug)]
    struct RecordingCall {
        prompt: String,
        resume_id: Option<String>,
    }

    #[async_trait::async_trait]
    impl crate::harness::Harness for RecordingHarness {
        fn kind(&self) -> crate::harness::HarnessKind {
            self.kind
        }
        fn supports_resume(&self) -> bool {
            true
        }
        async fn run_prompt(
            &self,
            prompt: &str,
            _attachments: &[Attachment],
            _cwd: &std::path::Path,
            session_id: Option<&str>,
            _options: &HarnessOptions,
        ) -> Result<mpsc::Receiver<crate::harness::HarnessEvent>> {
            self.calls.lock().unwrap().push(RecordingCall {
                prompt: prompt.to_string(),
                resume_id: session_id.map(String::from),
            });
            let (tx, rx) = mpsc::channel(2);
            tokio::spawn(async move {
                let _ = tx
                    .send(crate::harness::HarnessEvent::Text("ok".to_string()))
                    .await;
                let _ = tx
                    .send(crate::harness::HarnessEvent::Done {
                        session_id: "mock-sid-1".to_string(),
                    })
                    .await;
            });
            Ok(rx)
        }
        fn get_session_id(&self, _: &str) -> Option<String> {
            None
        }
        fn set_session_id(&self, _: &str, _: String) {}
    }

    fn install_recording_claude(app: &mut App) -> Arc<std::sync::Mutex<Vec<RecordingCall>>> {
        let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
        let harness = RecordingHarness {
            calls: Arc::clone(&calls),
            kind: crate::harness::HarnessKind::Claude,
        };
        app.harness_registry
            .insert_harness_for_test(crate::harness::HarnessKind::Claude, Box::new(harness));
        calls
    }

    fn bootstrap_foreground_session(app: &mut App) {
        // App::new doesn't create a tmux session by default; for these tests
        // we just register one in the session manager directly.
        app.session_mgr.install_test_foreground("test-session");
    }

    fn fake_telegram_msg(text: &str) -> IncomingMessage {
        IncomingMessage {
            user_id: "12345".to_string(),
            text: text.to_string(),
            platform: PlatformType::Telegram,
            reply_context: ReplyContext {
                platform: PlatformType::Telegram,
                chat_id: "chat-1".to_string(),
                thread_ts: None,
                socket_reply_tx: None,
            },
            attachments: vec![],
            socket_request_id: None,
            socket_client_name: None,
        }
    }

    #[tokio::test]
    async fn harness_on_with_initial_prompt_fires_send_harness_prompt() {
        // G4 (coverage review): the new `on <flags> <prompt>` form must
        // actually call send_harness_prompt with the trailing prompt.
        let dir = tempdir().unwrap();
        let (mut app, _state_rx) = make_app(dir.path());
        bootstrap_foreground_session(&mut app);
        let calls = install_recording_claude(&mut app);

        let msg = fake_telegram_msg(": claude on --name review please look at the bag");
        app.handle_command(&msg).await;

        // Give the spawned Done emitter a chance to run.
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        let recorded = calls.lock().unwrap().clone();
        assert_eq!(
            recorded.len(),
            1,
            "expected exactly one run_prompt call, got {:?}",
            recorded
        );
        assert_eq!(recorded[0].prompt, "please look at the bag");
        // New session — no resume_id yet.
        assert!(recorded[0].resume_id.is_none());
    }

    #[tokio::test]
    async fn harness_on_without_initial_prompt_does_not_fire_prompt() {
        // Regression guard: bare `on --name foo` should NOT send a prompt,
        // even though initial_prompt support exists.
        let dir = tempdir().unwrap();
        let (mut app, _state_rx) = make_app(dir.path());
        bootstrap_foreground_session(&mut app);
        let calls = install_recording_claude(&mut app);

        let msg = fake_telegram_msg(": claude on --name review");
        app.handle_command(&msg).await;
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        assert!(
            calls.lock().unwrap().is_empty(),
            "bare `on --name` must not fire a prompt: {:?}",
            calls.lock().unwrap()
        );
    }

    #[tokio::test]
    async fn zombie_entry_in_state_resumes_to_error() {
        // G5: an old-format state file with an empty-session_id entry is
        // loaded into the App but `--resume` on it errors cleanly (not a
        // panic, not a successful "resume" with no conversation). The
        // guard is `!entry.session_id.is_empty()` at the resume resolver.
        let dir = tempdir().unwrap();
        let state_path = dir.path().join("terminus-state.json");

        // Pre-seed state with a zombie entry (empty session_id).
        {
            let mut store = StateStore::load(&state_path).unwrap();
            let zombie = NamedSessionEntry {
                session_id: String::new(),
                cwd: std::path::PathBuf::from("/tmp"),
                last_used: Utc::now(),
            };
            store.apply(StateUpdate::HarnessSessionBatch(vec![(
                "ghost".into(),
                Some(zombie),
            )]));
            store.persist().unwrap();
        }

        let config = make_test_config(dir.path());
        let store = StateStore::load(&state_path).unwrap();
        let (state_tx, _rx) = mpsc::channel::<StateUpdate>(64);
        let app = App::new(&config, store, state_tx).unwrap();

        // The zombie loaded, but the resume filter treats it as "not found".
        assert!(
            app.harness_registry.named_sessions_contains_key("ghost"),
            "zombie should survive the load (we don't scrub on startup)"
        );
        let entry = app
            .harness_registry
            .named_sessions_get("ghost")
            .expect("ghost entry should be loaded");
        assert!(
            entry.session_id.is_empty(),
            "zombie should have empty session_id as seeded"
        );
    }

    #[tokio::test]
    async fn named_session_preserves_last_used_across_reload() {
        // G10: `last_used` must round-trip through the state file so LRU
        // ordering is stable across restarts. A silent field drop would
        // make every reload look freshly-used and defeat the LRU policy.
        let dir = tempdir().unwrap();
        let state_path = dir.path().join("terminus-state.json");

        let stamp = Utc::now() - chrono::Duration::minutes(42);
        {
            let mut store = StateStore::load(&state_path).unwrap();
            let entry = NamedSessionEntry {
                session_id: "sid-1".into(),
                cwd: std::path::PathBuf::from("/tmp/project"),
                last_used: stamp,
            };
            store.apply(StateUpdate::HarnessSessionBatch(vec![(
                "auth".into(),
                Some(entry),
            )]));
            store.persist().unwrap();
        }

        let reloaded = StateStore::load(&state_path).unwrap();
        let entry = &reloaded.snapshot().harness_sessions["auth"];
        // Serde on chrono DateTime<Utc> is RFC3339 with nanos — full
        // equality is fine.
        assert_eq!(entry.last_used, stamp);
    }

    // ── Attachment cleanup tests (AC-3 through AC-7) ──────────────────────────
    //
    // These tests verify that `Attachment::drop` (the RAII safety net) removes
    // the underlying temp file on all dispatch paths — parse errors, HarnessOn
    // guards, harness rejection, run_prompt Err, and the happy path.
    //
    // Pattern for each test:
    //   1. write a real temp file
    //   2. build an IncomingMessage with an Attachment pointing to it
    //   3. call app.handle_command(&msg).await
    //   4. drop msg (Attachment Drop fires)
    //   5. assert the file is gone

    fn make_real_temp_file() -> std::path::PathBuf {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static CTR: AtomicUsize = AtomicUsize::new(0);
        let n = CTR.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "terminus-app-test-{}-{}.txt",
            std::process::id(),
            n
        ));
        std::fs::write(&path, b"attachment data").expect("write temp file");
        path
    }

    fn fake_msg_with_attachment(text: &str, att_path: std::path::PathBuf) -> IncomingMessage {
        IncomingMessage {
            user_id: "12345".to_string(),
            text: text.to_string(),
            platform: PlatformType::Telegram,
            reply_context: ReplyContext {
                platform: PlatformType::Telegram,
                chat_id: "chat-1".to_string(),
                thread_ts: None,
                socket_reply_tx: None,
            },
            attachments: vec![Attachment {
                path: att_path,
                filename: "img.jpg".to_string(),
                media_type: "image/jpeg".to_string(),
            }],
            socket_request_id: None,
            socket_client_name: None,
        }
    }

    /// AC-3: Parse-error path — malformed `: ` command drops attachments.
    ///
    /// A bare `: ` (trigger char followed by whitespace, no command keyword)
    /// fails ParsedCommand::parse, which returns early from handle_command.
    /// The Attachment Drop must still remove the file.
    #[tokio::test]
    async fn parse_error_path_cleans_up_attachment() {
        let dir = tempdir().unwrap();
        let (mut app, _state_rx) = make_app(dir.path());
        let path = make_real_temp_file();
        assert!(path.exists(), "file must exist before dispatch");

        // ":" is a valid trigger; the command text is just the trigger char alone,
        // which fails to parse as any known command and produces a parse error.
        //
        // Path-confirmation: wire a socket_reply_tx so we can observe the error
        // reply that handle_command emits on parse failure.  An error reply being
        // present confirms the parse-error branch was reached, distinguishing it
        // from a silent no-op.
        let (reply_tx, mut reply_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let mut msg = fake_msg_with_attachment(": ", path.clone());
        msg.reply_context.socket_reply_tx = Some(reply_tx);
        app.handle_command(&msg).await;

        // The parse-error branch must have sent back an error message.
        let reply = reply_rx.try_recv().expect(
            "AC-3 path-confirmation: expected an error reply on parse failure, got nothing",
        );
        assert!(
            reply.contains("Error"),
            "AC-3: expected 'Error' in reply, got: {:?}",
            reply
        );

        // Drop msg — triggers Attachment::drop
        drop(msg);

        assert!(
            !path.exists(),
            "attachment file should be removed after parse-error path"
        );
    }

    /// AC-4: HarnessOn guard path — no foreground session drops attachments.
    ///
    /// `: claude on` with no foreground session hits the `None => return;`
    /// guard inside the HarnessOn branch.
    #[tokio::test]
    async fn harness_on_guard_cleans_up_attachment() {
        let dir = tempdir().unwrap();
        let (mut app, _state_rx) = make_app(dir.path());
        // Install the recording harness so the `!harnesses.contains_key` guard passes,
        // but deliberately do NOT bootstrap a foreground session so the
        // `foreground_session() == None => return` guard fires.
        let calls = install_recording_claude(&mut app);

        let path = make_real_temp_file();
        assert!(path.exists(), "file must exist before dispatch");

        let msg = fake_msg_with_attachment(": claude on", path.clone());
        app.handle_command(&msg).await;

        // Path-confirmation: the HarnessOn guard fires before run_prompt is ever
        // reached.  If RecordingHarness::run_prompt had been called the guard did
        // NOT fire; an empty call list proves the early-return path was taken.
        assert!(
            calls.lock().unwrap().is_empty(),
            "AC-4 path-confirmation: run_prompt must not be called when HarnessOn guard fires (no foreground session)"
        );

        drop(msg);

        assert!(
            !path.exists(),
            "attachment file should be removed after HarnessOn guard (no session) return"
        );
    }

    /// A harness whose `run_prompt` returns `Err(...)` synchronously, without
    /// spawning any background task. Used for AC-6 / AC-7 tests.
    #[derive(Clone)]
    struct ErrHarness {
        kind: crate::harness::HarnessKind,
        /// When false, returns Err from run_prompt. When true, returns Ok with
        /// a minimal success event stream.
        succeed: bool,
        /// Counts how many times `run_prompt` was actually invoked.
        /// Shared so the test can assert after handle_command returns.
        call_count: Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl crate::harness::Harness for ErrHarness {
        fn kind(&self) -> crate::harness::HarnessKind {
            self.kind
        }
        fn supports_resume(&self) -> bool {
            false
        }
        async fn run_prompt(
            &self,
            _prompt: &str,
            _attachments: &[crate::chat_adapters::Attachment],
            _cwd: &std::path::Path,
            _session_id: Option<&str>,
            _options: &crate::command::HarnessOptions,
        ) -> anyhow::Result<tokio::sync::mpsc::Receiver<crate::harness::HarnessEvent>> {
            self.call_count
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if self.succeed {
                let (tx, rx) = tokio::sync::mpsc::channel(2);
                tokio::spawn(async move {
                    let _ = tx
                        .send(crate::harness::HarnessEvent::Done {
                            session_id: String::new(),
                        })
                        .await;
                });
                Ok(rx)
            } else {
                Err(anyhow::anyhow!("simulated run_prompt failure"))
            }
        }
        fn get_session_id(&self, _: &str) -> Option<String> {
            None
        }
        fn set_session_id(&self, _: &str, _: String) {}
    }

    /// AC-5: Gemini-style rejection — a harness that rejects attachments
    /// synchronously (by returning Err from run_prompt). The attachment file
    /// must be removed via Drop after dispatch returns.
    ///
    /// This simulates gemini's synchronous rejection behaviour without
    /// requiring the actual gemini CLI on the test machine.
    #[tokio::test]
    async fn gemini_reject_cleans_up_attachment() {
        let dir = tempdir().unwrap();
        let (mut app, _state_rx) = make_app(dir.path());
        bootstrap_foreground_session(&mut app);

        // Install an ErrHarness as the Gemini harness (synchronous rejection).
        let call_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let harness = ErrHarness {
            kind: crate::harness::HarnessKind::Gemini,
            succeed: false,
            call_count: Arc::clone(&call_count),
        };
        app.harness_registry
            .insert_harness_for_test(crate::harness::HarnessKind::Gemini, Box::new(harness));

        let path = make_real_temp_file();
        assert!(path.exists(), "file must exist before dispatch");

        // `: gemini <prompt>` routes through HarnessPrompt -> send_harness_prompt
        // -> run_prompt (which returns Err).
        let msg = fake_msg_with_attachment(": gemini process this image", path.clone());
        app.handle_command(&msg).await;

        // Path-confirmation: the gemini rejection path calls run_prompt (which
        // returns Err synchronously).  A call count of exactly 1 proves the
        // error-return branch was taken rather than an early exit before spawn.
        assert_eq!(
            call_count.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "AC-5 path-confirmation: run_prompt must be called exactly once on gemini rejection"
        );

        drop(msg);

        assert!(
            !path.exists(),
            "attachment file should be removed after gemini synchronous rejection"
        );
    }

    /// AC-6: `send_harness_prompt` --resume lookup miss — when the user passes
    /// `--resume nonexistent`, the lookup fails and the function returns early.
    /// The attachment Drop must fire at msg drop.
    #[tokio::test]
    async fn send_harness_prompt_resume_lookup_miss_cleans_up_attachment() {
        let dir = tempdir().unwrap();
        let (mut app, _state_rx) = make_app(dir.path());
        bootstrap_foreground_session(&mut app);
        let calls = install_recording_claude(&mut app);

        let path = make_real_temp_file();
        assert!(path.exists(), "file must exist before dispatch");

        // --resume with a name that does not exist triggers an early return in
        // send_harness_prompt (the strict-resume lookup fails) BEFORE run_prompt
        // is invoked.  Use a socket_reply_tx to also capture the error message.
        let (reply_tx, mut reply_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let mut msg = fake_msg_with_attachment(": claude --resume nonexistent foo", path.clone());
        msg.reply_context.socket_reply_tx = Some(reply_tx);
        app.handle_command(&msg).await;

        // Path-confirmation (dual signal):
        // 1. run_prompt was never called — the lookup-miss returned early.
        assert!(
            calls.lock().unwrap().is_empty(),
            "AC-6 path-confirmation: run_prompt must not be called on --resume lookup miss"
        );
        // 2. An error reply was sent back confirming the miss-path was taken.
        let reply = reply_rx
            .try_recv()
            .expect("AC-6 path-confirmation: expected an error reply on --resume lookup miss");
        assert!(
            reply.contains("No session named"),
            "AC-6: expected 'No session named' in error reply, got: {:?}",
            reply
        );

        drop(msg);

        assert!(
            !path.exists(),
            "attachment file should be removed after --resume lookup miss"
        );
    }

    /// AC-7: `run_prompt` Err path — the harness returns Err synchronously.
    /// The attachment Drop must fire at msg drop.
    #[tokio::test]
    async fn harness_run_prompt_err_cleans_up_attachment() {
        let dir = tempdir().unwrap();
        let (mut app, _state_rx) = make_app(dir.path());
        bootstrap_foreground_session(&mut app);

        // Install a harness that always returns Err from run_prompt.
        let call_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let harness = ErrHarness {
            kind: crate::harness::HarnessKind::Claude,
            succeed: false,
            call_count: Arc::clone(&call_count),
        };
        app.harness_registry
            .insert_harness_for_test(crate::harness::HarnessKind::Claude, Box::new(harness));

        let path = make_real_temp_file();
        assert!(path.exists(), "file must exist before dispatch");

        let (reply_tx, mut reply_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let mut msg = fake_msg_with_attachment(": claude fix the bug", path.clone());
        msg.reply_context.socket_reply_tx = Some(reply_tx);
        app.handle_command(&msg).await;

        // Path-confirmation (dual signal):
        // 1. run_prompt was called exactly once — the prompt reached the harness.
        assert_eq!(
            call_count.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "AC-7 path-confirmation: run_prompt must be called exactly once on Err path"
        );
        // 2. An error reply was sent back, confirming the Err branch was handled.
        let reply = reply_rx
            .try_recv()
            .expect("AC-7 path-confirmation: expected an error reply after run_prompt returns Err");
        assert!(
            reply.contains("Error"),
            "AC-7: expected 'Error' in reply, got: {:?}",
            reply
        );

        drop(msg);

        assert!(
            !path.exists(),
            "attachment file should be removed after run_prompt Err return"
        );
    }

    // -------------------------------------------------------------------------
    // Step 4 tests
    // -------------------------------------------------------------------------

    #[cfg(feature = "slack")]
    #[test]
    fn test_paused_at_to_slack_ts_format() {
        // Verify the Slack timestamp format: "{unix_secs}.{microseconds:06}"
        use chrono::TimeZone;
        let dt = Utc.timestamp_opt(1_000_000_000, 123_456_000).unwrap();
        let ts = App::paused_at_to_slack_ts(dt);
        // 1_000_000_000 seconds, 123_456 microseconds (from 123_456_000 nanos)
        assert_eq!(
            ts, "1000000000.123456",
            "Slack ts format must be {{unix_secs}}.{{microseconds:06}}"
        );
    }

    #[cfg(feature = "slack")]
    #[tokio::test]
    async fn test_handle_gap_triggers_slack_catchup() {
        // Verify that handle_gap completes with a slack_platform set and no
        // active_slack_chats (empty set skips the catchup HTTP call, so the
        // test finishes within the banner-ack timeout window).
        let dir = tempdir().unwrap();
        let (mut app, _state_rx) = make_app(dir.path());

        // No active Slack chats → catchup branch skips HTTP.
        // slack_platform is set to confirm the branch is entered without panicking.
        let platform = Arc::new(
            crate::chat_adapters::slack::SlackPlatform::new_with_endpoint(
                "xoxb-test".into(),
                "xapp-test".into(),
                "C001".into(),
                "U001".into(),
                0,
                reqwest::Client::new(),
                "http://127.0.0.1:1".into(),
            ),
        );
        app.slack_platform = Some(platform);

        // Seed a Telegram chat so GapBanner is emitted and acked by a mock task.
        app.active_telegram_chats.insert(7777i64);

        let acks = app.pending_banner_acks_handle();
        let mut rx = app.subscribe_stream();
        let delivery_task = tokio::spawn(async move {
            while let Ok(event) = rx.recv().await {
                if let StreamEvent::GapBanner { chat_id, .. } = event {
                    if let Some(tx) = acks.lock().await.remove(&chat_id) {
                        let _ = tx.send(());
                    }
                    return;
                }
            }
        });

        let signal = PowerSignal::GapDetected {
            paused_at: Utc::now() - chrono::Duration::seconds(90),
            resumed_at: Utc::now(),
            gap: Duration::from_secs(90),
        };

        // handle_gap should complete without panicking.  No Slack chats → no HTTP.
        let (cmd_tx, _cmd_rx) = mpsc::channel::<IncomingMessage>(16);
        let result =
            tokio::time::timeout(Duration::from_secs(2), app.handle_gap(signal, cmd_tx)).await;
        assert!(
            result.is_ok(),
            "handle_gap should not deadlock with slack_platform set"
        );

        let _ = tokio::time::timeout(Duration::from_millis(100), delivery_task).await;
    }

    #[cfg(feature = "discord")]
    #[tokio::test]
    async fn test_handle_gap_triggers_discord_catchup() {
        // Verify that handle_gap completes with a discord_platform set, no
        // active_discord_chats (empty watermark map → catchup spawn proceeds
        // but does nothing internally), and that the catchup_in_progress
        // CAS gate flips and resets correctly.
        use crate::chat_adapters::DiscordAdapter;
        use crate::config::DiscordConfig;

        let dir = tempdir().unwrap();
        let (mut app, _state_rx) = make_app(dir.path());
        let (state_tx, _adapter_state_rx) = mpsc::channel::<StateUpdate>(16);

        let adapter = Arc::new(
            DiscordAdapter::new(
                DiscordConfig {
                    bot_token: "test-token".to_string(),
                    guild_id: None,
                    channel_id: None,
                },
                serenity::all::UserId::new(1),
                0,
                state_tx,
                std::collections::HashMap::new(),
            )
            .expect("DiscordAdapter::new should succeed for test"),
        );
        app.discord_platform = Some(Arc::clone(&adapter));

        // Seed a Telegram chat so GapBanner is emitted and acked by a mock task.
        app.active_telegram_chats.insert(7777i64);

        let acks = app.pending_banner_acks_handle();
        let mut rx = app.subscribe_stream();
        let delivery_task = tokio::spawn(async move {
            while let Ok(event) = rx.recv().await {
                if let StreamEvent::GapBanner { chat_id, .. } = event {
                    if let Some(tx) = acks.lock().await.remove(&chat_id) {
                        let _ = tx.send(());
                    }
                    return;
                }
            }
        });

        let signal = PowerSignal::GapDetected {
            paused_at: Utc::now() - chrono::Duration::seconds(90),
            resumed_at: Utc::now(),
            gap: Duration::from_secs(90),
        };

        let (cmd_tx, _cmd_rx) = mpsc::channel::<IncomingMessage>(16);
        let result =
            tokio::time::timeout(Duration::from_secs(2), app.handle_gap(signal, cmd_tx)).await;
        assert!(
            result.is_ok(),
            "handle_gap should not deadlock with discord_platform set"
        );

        let _ = tokio::time::timeout(Duration::from_millis(100), delivery_task).await;
    }
}
