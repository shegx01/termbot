use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use chrono::Utc;
use futures_util::future::join_all;
use tokio::sync::{broadcast, mpsc, oneshot, Mutex as AsyncMutex};

use crate::buffer::{OutputBuffer, StreamEvent};
use crate::chat_adapters::{Attachment, ChatPlatform, IncomingMessage, PlatformType, ReplyContext};
use crate::command::{CommandBlocklist, HarnessOptions, ParsedCommand};
use crate::config::Config;
use crate::delivery::{split_message, GapInfo, GapPrefixes, PendingBannerAcks};
use crate::harness::claude::ClaudeHarness;
use crate::harness::opencode::OpencodeHarness;
use crate::harness::{build_session_key, drive_harness, Harness, HarnessContext, HarnessKind};
use crate::power::types::PowerSignal;
use crate::session::{self, SessionManager};
use crate::socket::events::AmbientEvent;
use crate::state_store::{NamedSessionEntry, StateStore, StateUpdate};
use crate::structured_output::{spawn_retry_worker, DeliveryQueue, SchemaRegistry, WebhookClient};
use crate::tmux::TmuxClient;

// ──────────────────────────────────────────────────────────────────────────────
// App
// ──────────────────────────────────────────────────────────────────────────────

pub struct App {
    blocklist: CommandBlocklist,
    session_mgr: SessionManager,
    buffers: HashMap<String, OutputBuffer>,
    harnesses: HashMap<HarnessKind, Box<dyn Harness>>,
    stream_tx: broadcast::Sender<StreamEvent>,
    telegram: Option<Arc<dyn ChatPlatform>>,
    slack: Option<Arc<dyn ChatPlatform>>,
    discord: Option<Arc<dyn ChatPlatform>>,
    offline_buffer_max: usize,
    trigger: char,

    // ── Named harness sessions ──────────────────────────────────────────────
    /// Named harness session index (name → session_id + cwd + last_used).
    /// Populated from persisted state on startup, updated on each prompt.
    named_harness_sessions: HashMap<String, NamedSessionEntry>,
    /// Maximum named sessions before LRU eviction.
    max_named_sessions: usize,

    // ── Sleep/wake recovery state ─────────────────────────────────────────────
    /// Owned state store (single owner — adapters send updates via mpsc).
    store: StateStore,
    /// mpsc sender to push `StateUpdate`s into the App's `state_rx` channel.
    /// Kept here so `App` itself can send updates (e.g. `MarkDirty`, `Tick`).
    state_tx: mpsc::Sender<StateUpdate>,
    /// Active Telegram chat IDs (hydrated from persisted state on startup).
    active_telegram_chats: HashSet<i64>,
    /// Active Slack channel IDs (hydrated from persisted state on startup).
    active_slack_chats: HashSet<String>,
    /// Active Discord channel IDs (hydrated from persisted state on startup).
    active_discord_chats: HashSet<String>,
    /// Pending oneshot senders keyed by chat_id.  `handle_gap` inserts;
    /// the delivery task resolves directly on successful banner send.
    /// Shared via `Arc<AsyncMutex<_>>` so the map can be locked briefly
    /// from either side without holding across await boundaries in `handle_gap`.
    pending_banner_acks: PendingBannerAcks,
    /// Debounce counters for `store.persist()`.
    last_state_persist: Instant,
    updates_since_persist: u32,
    /// Inline fallback gap prefixes: if banner delivery times out, store here
    /// and prepend `[gap: Xm Ys]` to the next message for that chat.
    /// Shared via `Arc<AsyncMutex<_>>` so delivery tasks can consume entries
    /// without going through the main `tokio::select!` loop.
    gap_prefix: GapPrefixes,
    /// Whether we have already sent a `MarkDirty` update to state.
    dirty_sent: bool,
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
    /// Opencode CLI-subprocess harness (kept as a typed Arc for direct access
    /// alongside the type-erased entry in `harnesses`).
    #[allow(dead_code)]
    pub opencode: Arc<OpencodeHarness>,
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

        let mut harnesses: HashMap<HarnessKind, Box<dyn Harness>> = HashMap::new();
        harnesses.insert(
            HarnessKind::Claude,
            Box::new(ClaudeHarness::new().with_schema_registry(Arc::clone(&schema_registry))),
        );
        let opencode_cfg = config.harness.opencode.clone().unwrap_or_default();
        let opencode = Arc::new(OpencodeHarness::new(opencode_cfg, ambient_tx.clone()));
        harnesses.insert(
            HarnessKind::Opencode,
            Box::new(Arc::clone(&opencode)) as Box<dyn Harness>,
        );

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

        let mut app = Self {
            blocklist,
            session_mgr,
            buffers: HashMap::new(),
            harnesses,
            stream_tx,
            telegram: None,
            slack: None,
            discord: None,
            offline_buffer_max: config.streaming.offline_buffer_max_bytes,
            trigger,
            named_harness_sessions,
            max_named_sessions: config.harness.max_named_sessions.unwrap_or(50),
            store,
            state_tx,
            active_telegram_chats,
            active_slack_chats,
            active_discord_chats,
            pending_banner_acks: Arc::new(AsyncMutex::new(HashMap::new())),
            last_state_persist: Instant::now(),
            updates_since_persist: 0,
            gap_prefix: Arc::new(AsyncMutex::new(HashMap::new())),
            dirty_sent: false,
            startup_was_clean,
            schema_registry: Arc::clone(&schema_registry),
            delivery_queue: Arc::clone(&delivery_queue),
            webhook_client: Arc::clone(&webhook_client),
            ambient_tx,
            retry_worker_shutdown: Arc::clone(&retry_worker_shutdown),
            retry_worker_shutdown_flag: Arc::clone(&retry_worker_shutdown_flag),
            opencode,
        };

        // Immediately mark state dirty in memory (sets last_clean_shutdown=false)
        // and force-persist to disk so a crash within the first ~5s (before the
        // debounce window fires) still triggers a restart banner on the next boot.
        app.store.apply(StateUpdate::MarkDirty);
        app.store.persist()?;

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
        discord: Option<Arc<dyn ChatPlatform>>,
    ) {
        self.telegram = telegram;
        self.slack = slack;
        self.discord = discord;
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
        Arc::clone(&self.pending_banner_acks)
    }

    /// Shared handle to the gap-prefix map — passed into `spawn_delivery_task`
    /// so delivery tasks can consume inline `[gap: …]` markers set by
    /// `handle_gap` when banner delivery times out.
    pub fn gap_prefix_handle(&self) -> GapPrefixes {
        Arc::clone(&self.gap_prefix)
    }

    /// Returns the initial Telegram offset from the persisted state snapshot.
    /// Call this before constructing the `TelegramAdapter` with
    /// `.with_initial_offset(...)`.
    pub fn initial_telegram_offset(&self) -> i64 {
        self.store.snapshot().telegram.offset
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
                // Clean stale image temp files from previous runs
                if name_str.starts_with("terminus-img-") {
                    tracing::info!("Cleaning stale image temp file: {}", entry.path().display());
                    let _ = std::fs::remove_file(entry.path());
                }
                // Clean stale attachment temp files from socket binary uploads
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
        let snapshot = self.store.snapshot().clone();
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
    pub async fn handle_gap(&mut self, signal: PowerSignal) {
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

        // Register pending acks and broadcast banners.  The lock is held only
        // for the duration of the inserts — never across `.await`.  The
        // delivery task resolves each oneshot directly by locking the same
        // map from its own task, so we can safely `.await` below without
        // deadlocking the main `select!` loop.
        let mut ack_futures = Vec::new();
        {
            let mut pending = self.pending_banner_acks.lock().await;
            for (chat_id, _) in &all_chats {
                let (tx, rx) = oneshot::channel::<()>();
                // If `handle_gap` is invoked a second time while the first is still
                // awaiting acks, this insert replaces the first oneshot sender.  The
                // first `rx.await` then returns `Err(RecvError)` and the inline-prefix
                // fallback fires for it — acceptable single-user semantics since the user
                // will still see a banner for the second gap.
                pending.insert(chat_id.clone(), tx);
                ack_futures.push((chat_id.clone(), rx));
            }
        } // lock released

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

        // Await all banner acks concurrently (5s timeout each) so a slow
        // delivery task on one platform does not delay others.
        let results = join_all(ack_futures.into_iter().map(|(chat_id, rx)| async move {
            let result = tokio::time::timeout(Duration::from_secs(5), rx).await;
            (chat_id, result)
        }))
        .await;

        for (chat_id, result) in results {
            match result {
                Ok(Ok(())) => {
                    tracing::info!(
                        "GapBanner delivered for chat_id={} (gap={:.0}s)",
                        chat_id,
                        gap.as_secs_f64()
                    );
                }
                Ok(Err(_)) | Err(_) => {
                    // `Ok(Err(_))` = sender dropped (delivery task died);
                    // `Err(_)` = 5s elapsed without the delivery task acking.
                    tracing::error!(
                        "GapBanner delivery timed out for chat_id={} — \
                         using inline fallback prefix",
                        chat_id
                    );
                    // Store for inline fallback on next outbound message.
                    // Lock held only for the insert — never across an await.
                    self.gap_prefix.lock().await.insert(
                        chat_id.clone(),
                        GapInfo {
                            gap,
                            paused_at,
                            resumed_at,
                        },
                    );
                    // Clean up the pending entry if still present (briefly lock).
                    self.pending_banner_acks.lock().await.remove(&chat_id);
                }
            }
        }

        // 3. Resume all adapters via trait method.
        for p in [&self.telegram, &self.slack, &self.discord]
            .into_iter()
            .flatten()
        {
            p.resume().await;
        }
        tracing::info!("Adapters resumed after gap handling");
    }

    /// Apply a `StateUpdate` to in-memory state and debounce persists.
    ///
    /// Persists when: >= 10 updates accumulated OR >= 5s since last persist.
    pub async fn apply_state_update(&mut self, update: StateUpdate) {
        // Track active chat IDs as they bind.
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
        // Safety-critical variants bypass the debounce window so a crash
        // immediately after cannot leave the on-disk state inconsistent.
        // Named-session batches must force-persist too: a user expects
        // `--resume foo` to work after process restart, and without this
        // an unclean exit between the prompt and the next 10 updates / 5s
        // window would silently drop the session from disk.
        let force = matches!(
            &update,
            StateUpdate::MarkDirty
                | StateUpdate::SetCleanShutdown(_)
                | StateUpdate::HarnessSessionBatch(_)
        );
        self.store.apply(update);
        self.updates_since_persist += 1;
        if force
            || self.updates_since_persist >= 10
            || self.last_state_persist.elapsed() >= Duration::from_secs(5)
        {
            if let Err(e) = self.store.persist() {
                tracing::error!("Failed to persist state: {}", e);
            }
            self.last_state_persist = Instant::now();
            self.updates_since_persist = 0;
        }
    }

    /// Mark last_clean_shutdown=true and force-persist.  Called on graceful
    /// ctrl-c before `cleanup()`.
    pub async fn mark_clean_shutdown(&mut self) {
        self.store.apply(StateUpdate::SetCleanShutdown(true));
        if let Err(e) = self.store.persist() {
            tracing::error!("Failed to persist clean-shutdown state: {}", e);
        } else {
            tracing::info!("Clean shutdown persisted");
        }
        // Signal the retry worker to stop after completing its current job.
        // Set the atomic flag FIRST so any next non-blocking poll sees `true`,
        // then notify so an idle/backoff sleep wakes immediately.
        self.retry_worker_shutdown_flag
            .store(true, Ordering::SeqCst);
        self.retry_worker_shutdown.notify_one();
    }

    /// Consume and return the inline gap prefix for `chat_id` if one is
    /// pending.  The delivery task consumes entries directly via
    /// `gap_prefix_handle()`, but this method is also used by unit tests.
    #[cfg_attr(not(test), allow(dead_code))]
    pub async fn consume_gap_prefix(&self, chat_id: &str) -> Option<GapInfo> {
        self.gap_prefix.lock().await.remove(chat_id)
    }

    pub async fn handle_command(&mut self, msg: &IncomingMessage) {
        // Ensure MarkDirty is sent on the first real user interaction.
        if !self.dirty_sent {
            if let Err(e) = self.state_tx.try_send(StateUpdate::MarkDirty) {
                tracing::warn!("state_tx full, update dropped: {}", e);
            }
            self.dirty_sent = true;
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
                cleanup_attachments(&msg.attachments).await;
                return;
            }
        }

        tracing::info!(
            "handle_command: {:?} (fg={:?})",
            cmd,
            self.session_mgr.foreground_session()
        );

        // Clean up image temp files for commands that won't route to a harness.
        // Harness paths (HarnessPrompt, StdinInput->harness) handle their own cleanup
        // inside the spawned harness task.
        let harness_will_handle = matches!(cmd, ParsedCommand::HarnessPrompt { .. })
            || matches!(cmd, ParsedCommand::HarnessSubcommand { .. })
            || matches!(
                cmd,
                ParsedCommand::HarnessOn {
                    initial_prompt: Some(_),
                    ..
                }
            )
            || (matches!(cmd, ParsedCommand::StdinInput { .. })
                && self.session_mgr.foreground_harness().is_some());
        if !harness_will_handle && !msg.attachments.is_empty() {
            cleanup_attachments(&msg.attachments).await;
        }

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
                if !self.harnesses.contains_key(&kind) {
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
                // Validate --name/--resume on non-resumable harnesses
                if self
                    .reject_if_not_resumable(&msg.reply_context, &kind, &options)
                    .await
                {
                    return;
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
                    match self.lookup_named_session(kind, resume_name) {
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
                    if let Some(h) = self.harnesses.get(&current) {
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
                    cleanup_attachments(&msg.attachments).await;
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
                if harness != HarnessKind::Opencode {
                    // Parser should only emit this for opencode, but be defensive.
                    self.send_error(
                        &msg.reply_context,
                        &format!("HarnessSubcommand not supported for {}", harness.name()),
                    )
                    .await;
                    return;
                }
                match self.opencode.run_subcommand(subcommand, args).await {
                    Ok(event_rx) => {
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
                        // Subcommands don't create sessions and don't emit ambient
                        // HarnessStarted/Finished — drive_harness still works fine;
                        // we just ignore the session_id return value.
                        let _ = drive_harness(event_rx, &hctx).await;
                    }
                    Err(e) => {
                        self.send_error(
                            &msg.reply_context,
                            &format!("opencode subcommand setup failed: {}", e),
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
                    cleanup_attachments(&msg.attachments).await;
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
                        cleanup_attachments(&msg.attachments).await;
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

    /// Look up a named session by harness kind + name.
    ///
    /// Tries the prefixed key `{kind}:{name}` first (new format), then falls
    /// back to the bare `name` for legacy unprefixed keys that pre-date the
    /// cross-harness isolation scheme.
    ///
    /// TODO: remove legacy fallback once state-file shows zero unprefixed keys
    /// for 14 consecutive days (tracked in
    /// .omc/plans/opencode-harness-followups.md).
    fn lookup_named_session(&self, kind: HarnessKind, name: &str) -> Option<&NamedSessionEntry> {
        let prefixed = build_session_key(kind, name);
        self.named_harness_sessions
            .get(&prefixed)
            .or_else(|| self.named_harness_sessions.get(name))
    }

    /// Evict the least-recently-used named session, returning its name.
    fn evict_lru_session(&mut self) -> Option<String> {
        let oldest = self
            .named_harness_sessions
            .iter()
            .min_by_key(|(_, e)| e.last_used)
            .map(|(k, _)| k.clone());
        if let Some(ref key) = oldest {
            self.named_harness_sessions.remove(key);
        }
        oldest
    }

    /// Send an error and return `true` if `--name`/`--resume` are specified
    /// on a harness that doesn't support resume. Callers should `return`
    /// immediately when this returns `true`.
    async fn reject_if_not_resumable(
        &self,
        ctx: &ReplyContext,
        kind: &HarnessKind,
        options: &HarnessOptions,
    ) -> bool {
        if options.name.is_none() && options.resume.is_none() {
            return false;
        }
        let Some(h) = self.harnesses.get(kind) else {
            return false;
        };
        if h.supports_resume() {
            return false;
        }
        self.send_error(
            ctx,
            &format!(
                "--name/--resume not supported for {} (no session resume)",
                kind.name()
            ),
        )
        .await;
        true
    }

    /// Send a prompt to a harness (Claude, Gemini, Codex) with real-time streaming to chat.
    async fn send_harness_prompt(
        &mut self,
        ctx: &ReplyContext,
        kind: &HarnessKind,
        prompt: &str,
        attachments: &[Attachment],
        options: &HarnessOptions,
    ) {
        // Check --name/--resume on non-resumable harnesses
        if self.reject_if_not_resumable(ctx, kind, options).await {
            return;
        }

        // Resolve session name from flags, falling back to implicit path
        let using_named_session;
        let session_name;
        let resume_id;
        let cwd;
        let mut notification: Option<String> = None;

        if let Some(ref resume_name) = options.resume {
            // --resume: strict lookup, error if not found (or has no session_id yet)
            match self.lookup_named_session(*kind, resume_name) {
                Some(entry) if !entry.session_id.is_empty() => {
                    session_name = resume_name.clone();
                    resume_id = Some(entry.session_id.clone());
                    cwd = entry.cwd.clone();
                    using_named_session = true;
                }
                _ => {
                    self.send_error(
                        ctx,
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
            // --name: create-or-resume (upsert)
            if let Some(entry) = self
                .lookup_named_session(*kind, name)
                .filter(|e| !e.session_id.is_empty())
            {
                session_name = name.clone();
                resume_id = Some(entry.session_id.clone());
                cwd = entry.cwd.clone();
                using_named_session = true;
                // Only notify if this session hasn't been resolved yet in
                // interactive mode (suppresses repeated notifications).
                let already_resolved =
                    self.session_mgr.foreground_named_session_resolved() == Some(name.as_str());
                if !already_resolved {
                    notification = Some(format!("Resuming existing session '{}'", name));
                }
            } else {
                // New session — record current cwd
                session_name = name.clone();
                resume_id = None;
                cwd = current_dir_or_dot();
                using_named_session = true;
                // Hint on creation for numeric names (e.g. `-n 5`), but
                // suppress if the interactive-mode `on` already emitted it.
                let already_resolved =
                    self.session_mgr.foreground_named_session_resolved() == Some(name.as_str());
                if !already_resolved {
                    notification = numeric_name_hint(name);
                }
            }
        } else {
            // Implicit path — unchanged
            session_name = self
                .session_mgr
                .foreground_session()
                .unwrap_or("default")
                .to_string();
            using_named_session = false;
            resume_id = self
                .harnesses
                .get(kind)
                .and_then(|h| h.get_session_id(&session_name));
            cwd = current_dir_or_dot();
        }

        let harness = match self.harnesses.get(kind) {
            Some(h) => h,
            None => {
                self.send_error(ctx, &format!("{} harness not available", kind.name()))
                    .await;
                return;
            }
        };

        // Send notification before the prompt (if any)
        if let Some(ref note) = notification {
            self.send_reply(ctx, note).await;
        }

        match harness
            .run_prompt(prompt, attachments, &cwd, resume_id.as_deref(), options)
            .await
        {
            Ok(event_rx) => {
                let run_id = ulid::Ulid::new().to_string();
                self.emit_ambient(AmbientEvent::HarnessStarted {
                    harness: kind.name().to_string(),
                    run_id: run_id.clone(),
                    prompt_hash: None,
                });

                let hctx = HarnessContext {
                    ctx,
                    telegram: self.telegram.as_deref(),
                    slack: self.slack.as_deref(),
                    discord: self.discord.as_deref(),
                    schema_registry: &self.schema_registry,
                    delivery_queue: &self.delivery_queue,
                    webhook_client: &self.webhook_client,
                    stream_tx: &self.stream_tx,
                };
                let (got_output, session_id) = drive_harness(event_rx, &hctx).await;

                let status = if got_output { "completed" } else { "no_output" };
                self.emit_ambient(AmbientEvent::HarnessFinished {
                    harness: kind.name().to_string(),
                    run_id,
                    status: status.to_string(),
                });

                if using_named_session {
                    // Use a prefixed key for cross-harness isolation.
                    let prefixed_key = build_session_key(*kind, &session_name);
                    // Prefer the session_id returned from this run; otherwise
                    // fall back to the existing entry's id so that a zero-output
                    // resume still refreshes `last_used` (prevents the session
                    // from becoming a stale eviction target).
                    let effective_sid = session_id.or_else(|| {
                        self.lookup_named_session(*kind, &session_name)
                            .map(|e| e.session_id.clone())
                            .filter(|s| !s.is_empty())
                    });
                    if let Some(sid) = effective_sid {
                        let entry = NamedSessionEntry {
                            session_id: sid,
                            cwd: cwd.clone(),
                            last_used: Utc::now(),
                        };

                        // Evict LRU if at cap (before inserting, in case this is a new name)
                        let mut batch_ops: Vec<(String, Option<NamedSessionEntry>)> = Vec::new();
                        if !self.named_harness_sessions.contains_key(&prefixed_key)
                            && self.named_harness_sessions.len() >= self.max_named_sessions
                        {
                            if let Some(evicted) = self.evict_lru_session() {
                                batch_ops.push((evicted, None));
                            }
                        }
                        self.named_harness_sessions
                            .insert(prefixed_key.clone(), entry.clone());
                        batch_ops.push((prefixed_key, Some(entry)));

                        // Atomic persist
                        if let Err(e) = self
                            .state_tx
                            .try_send(StateUpdate::HarnessSessionBatch(batch_ops))
                        {
                            tracing::warn!("state_tx full, session update dropped: {}", e);
                        }
                        // Skip harness.set_session_id() — prevents cross-index leakage
                    }
                } else if let Some(sid) = session_id {
                    // Implicit path — use existing harness-internal tracking
                    if let Some(h) = self.harnesses.get(kind) {
                        h.set_session_id(&session_name, sid);
                    }
                }
                if !got_output {
                    self.send_error(ctx, &format!("{}: no response received", kind.name()))
                        .await;
                }
            }
            Err(e) => {
                self.send_error(ctx, &format!("{}: {:#}", kind.name(), e))
                    .await;
            }
        }
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

/// Clean up temp files from attachments that won't reach the harness
/// (which normally handles its own cleanup in the spawned task).
async fn cleanup_attachments(attachments: &[Attachment]) {
    for att in attachments {
        let _ = tokio::fs::remove_file(&att.path).await;
    }
}

/// Resolve the current working directory, falling back to `"."` when the
/// platform call fails (matches `App::new`'s original fallback and avoids
/// propagating an empty `PathBuf` into the session index).
fn current_dir_or_dot() -> std::path::PathBuf {
    std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."))
}

/// If a session name parses as a plain `u32`, return a hint explaining that
/// `-n` now means `--name` (not `--max-turns`). `-n 5` is still accepted as
/// a valid session name, but the user very likely meant `-t 5`.
///
/// Fires only on session *creation*, so resuming an existing numeric name
/// stays silent.
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

    #[tokio::test]
    async fn apply_state_update_debounces_below_threshold() {
        let dir = tempdir().unwrap();
        let (mut app, _state_rx) = make_app(dir.path());

        // Apply 5 updates — below the 10-update threshold and under 5s.
        for i in 0..5i64 {
            app.apply_state_update(StateUpdate::TelegramOffset(i)).await;
        }
        // persist should NOT have been called yet (5 < 10, and < 5s elapsed).
        assert_eq!(app.updates_since_persist, 5);
    }

    #[tokio::test]
    async fn apply_state_update_persists_at_threshold() {
        let dir = tempdir().unwrap();
        let (mut app, _state_rx) = make_app(dir.path());

        // Apply exactly 10 updates — should trigger a persist.
        for i in 0..10i64 {
            app.apply_state_update(StateUpdate::TelegramOffset(i)).await;
        }
        // After persist, counter resets to 0.
        assert_eq!(app.updates_since_persist, 0);
        // State file should exist.
        assert!(dir.path().join("terminus-state.json").exists());
    }

    #[tokio::test]
    async fn mark_clean_shutdown_persists_true() {
        let dir = tempdir().unwrap();
        let (mut app, _state_rx) = make_app(dir.path());

        // Trigger some activity.
        app.apply_state_update(StateUpdate::TelegramOffset(1)).await;
        app.mark_clean_shutdown().await;

        // Reload from disk and check.
        let reloaded = StateStore::load(dir.path().join("terminus-state.json")).unwrap();
        assert!(
            reloaded.snapshot().last_clean_shutdown,
            "mark_clean_shutdown should persist last_clean_shutdown=true"
        );
    }

    #[tokio::test]
    async fn app_new_marks_dirty_in_memory() {
        let dir = tempdir().unwrap();
        let (app, _state_rx) = make_app(dir.path());
        // App::new applies MarkDirty immediately.
        assert!(
            !app.store.snapshot().last_clean_shutdown,
            "App::new should mark store dirty (last_clean_shutdown=false)"
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

        let result = tokio::time::timeout(Duration::from_secs(1), app.handle_gap(signal)).await;
        assert!(
            result.is_ok(),
            "handle_gap deadlocked or timed out beyond the 1s bound"
        );

        // No gap_prefix fallback should have been engaged.
        assert!(
            app.gap_prefix.lock().await.is_empty(),
            "gap_prefix should be empty when the ack resolved in time"
        );

        let _ = tokio::time::timeout(Duration::from_millis(100), delivery_task).await;
    }

    #[tokio::test]
    async fn consume_gap_prefix_returns_and_clears() {
        let dir = tempdir().unwrap();
        let (app, _state_rx) = make_app(dir.path());

        let now = Utc::now();
        app.gap_prefix.lock().await.insert(
            "chat42".to_string(),
            GapInfo {
                gap: Duration::from_secs(90),
                paused_at: now - chrono::Duration::seconds(90),
                resumed_at: now,
            },
        );

        let prefix = app.consume_gap_prefix("chat42").await;
        assert!(prefix.is_some(), "should return the gap info");
        assert_eq!(prefix.unwrap().gap, Duration::from_secs(90));
        // Second call: cleared.
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

    fn make_entry(sid: &str, secs_ago: i64) -> NamedSessionEntry {
        NamedSessionEntry {
            session_id: sid.into(),
            cwd: std::path::PathBuf::from("/tmp"),
            last_used: Utc::now() - chrono::Duration::seconds(secs_ago),
        }
    }

    #[tokio::test]
    async fn evict_lru_session_removes_oldest_entry() {
        let dir = tempdir().unwrap();
        let (mut app, _state_rx) = make_app(dir.path());

        app.named_harness_sessions
            .insert("new".into(), make_entry("sid-new", 5));
        app.named_harness_sessions
            .insert("oldest".into(), make_entry("sid-old", 3600));
        app.named_harness_sessions
            .insert("middle".into(), make_entry("sid-mid", 300));

        let evicted = app.evict_lru_session();
        assert_eq!(evicted.as_deref(), Some("oldest"));
        assert_eq!(app.named_harness_sessions.len(), 2);
        assert!(!app.named_harness_sessions.contains_key("oldest"));
        assert!(app.named_harness_sessions.contains_key("new"));
        assert!(app.named_harness_sessions.contains_key("middle"));
    }

    #[tokio::test]
    async fn evict_lru_session_on_empty_index_returns_none() {
        let dir = tempdir().unwrap();
        let (mut app, _state_rx) = make_app(dir.path());
        assert!(app.evict_lru_session().is_none());
    }

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
            app.updates_since_persist, 0,
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
        assert_eq!(app.named_harness_sessions.len(), 1);
        let entry = app
            .named_harness_sessions
            .get("auth")
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
        assert!(
            !app.reject_if_not_resumable(&ctx, &HarnessKind::Claude, &opts)
                .await
        );
    }

    #[test]
    fn current_dir_or_dot_never_returns_empty_path() {
        let p = super::current_dir_or_dot();
        assert!(!p.as_os_str().is_empty());
    }

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
        app.harnesses
            .insert(crate::harness::HarnessKind::Claude, Box::new(harness));
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
            app.named_harness_sessions.contains_key("ghost"),
            "zombie should survive the load (we don't scrub on startup)"
        );
        let entry = &app.named_harness_sessions["ghost"];
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
}
