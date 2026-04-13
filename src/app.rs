use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use chrono::{Local, Utc};
use tokio::sync::{broadcast, mpsc, oneshot, Mutex as AsyncMutex};

use crate::buffer::{OutputBuffer, StreamEvent};
use crate::command::{CommandBlocklist, ParsedCommand};
use crate::config::Config;
use crate::harness::claude::ClaudeHarness;
use crate::harness::{drive_harness, Harness, HarnessKind};
use crate::platform::{Attachment, ChatPlatform, IncomingMessage, PlatformType, ReplyContext};
use crate::platform::telegram::TelegramAdapter;
use crate::power::types::PowerSignal;
use crate::session::{self, SessionManager};
use crate::state_store::{StateStore, StateUpdate};
use crate::tmux::TmuxClient;

// ──────────────────────────────────────────────────────────────────────────────
// Banner-ack map — shared between App (writer/waiter) and delivery tasks (resolver)
// ──────────────────────────────────────────────────────────────────────────────

/// Shared registry of pending banner-delivery notifications.
/// `App::handle_gap` inserts a oneshot sender keyed by chat_id before
/// broadcasting a `StreamEvent::GapBanner`.  The delivery task removes
/// and fires it immediately on successful send so `handle_gap`'s await
/// unblocks without routing through the main `tokio::select!` loop.
///
/// This direct-resolution pattern is required because `handle_gap` runs
/// inline on the main select branch — routing the ack through a separate
/// mpsc would deadlock (the main loop can't process the ack channel
/// while blocked inside `handle_gap`).
pub type PendingBannerAcks = Arc<AsyncMutex<HashMap<String, oneshot::Sender<()>>>>;

// ──────────────────────────────────────────────────────────────────────────────
// GapInfo — stored for the fallback inline-prefix path
// ──────────────────────────────────────────────────────────────────────────────

/// Retained gap information for the fallback inline-prefix path.
/// When `handle_gap` banner delivery times out, we store this and prepend
/// `[gap: Xm Ys]` to the next outbound message for the chat.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct GapInfo {
    pub gap: Duration,
    pub paused_at: chrono::DateTime<Utc>,
    pub resumed_at: chrono::DateTime<Utc>,
}

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
    offline_buffer_max: usize,
    trigger: char,

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
    /// Pending oneshot senders keyed by chat_id.  `handle_gap` inserts;
    /// the delivery task resolves directly on successful banner send.
    /// Shared via `Arc<AsyncMutex<_>>` so the map can be locked briefly
    /// from either side without holding across await boundaries in `handle_gap`.
    pending_banner_acks: PendingBannerAcks,
    /// Debounce counters for `store.persist()`.
    last_state_persist: Instant,
    updates_since_persist: u32,
    /// Concrete Telegram adapter (for pause/resume on gap).
    telegram_adapter: Option<Arc<TelegramAdapter>>,
    /// Inline fallback gap prefixes: if banner delivery times out, store here
    /// and prepend `[gap: Xm Ys]` to the next message for that chat.
    gap_prefix: HashMap<String, GapInfo>,
    /// Whether we have already sent a `MarkDirty` update to state.
    dirty_sent: bool,
    /// Snapshot of `last_clean_shutdown` from the state file *before* we
    /// applied `MarkDirty` on startup.  Used by `emit_startup_gap_banners`
    /// to check whether the previous run exited cleanly.
    startup_was_clean: bool,
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
        let cwd = std::env::current_dir()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| ".".to_string());
        let mut harnesses: HashMap<HarnessKind, Box<dyn Harness>> = HashMap::new();
        harnesses.insert(HarnessKind::Claude, Box::new(ClaudeHarness::new(cwd)));
        let (stream_tx, _) = broadcast::channel::<StreamEvent>(256);

        // Hydrate active chat sets from persisted state.
        let snapshot = store.snapshot();
        let active_telegram_chats: HashSet<i64> =
            snapshot.chats.telegram.iter().copied().collect();
        let active_slack_chats: HashSet<String> =
            snapshot.chats.slack.iter().cloned().collect();
        // Capture this BEFORE applying MarkDirty, so emit_startup_gap_banners
        // knows whether the *previous* run exited cleanly.
        let startup_was_clean = snapshot.last_clean_shutdown;

        let mut app = Self {
            blocklist,
            session_mgr,
            buffers: HashMap::new(),
            harnesses,
            stream_tx,
            telegram: None,
            slack: None,
            offline_buffer_max: config.streaming.offline_buffer_max_bytes,
            trigger,
            store,
            state_tx,
            active_telegram_chats,
            active_slack_chats,
            pending_banner_acks: Arc::new(AsyncMutex::new(HashMap::new())),
            last_state_persist: Instant::now(),
            updates_since_persist: 0,
            telegram_adapter: None,
            gap_prefix: HashMap::new(),
            dirty_sent: false,
            startup_was_clean,
        };

        // Immediately mark state dirty in memory (sets last_clean_shutdown=false).
        // This ensures that a crash before `mark_clean_shutdown()` is called
        // will correctly fire a restart banner on the next boot.
        app.store.apply(StateUpdate::MarkDirty);

        Ok(app)
    }

    pub fn set_platforms(
        &mut self,
        telegram: Option<Arc<dyn ChatPlatform>>,
        slack: Option<Arc<dyn ChatPlatform>>,
    ) {
        self.telegram = telegram;
        self.slack = slack;
    }

    /// Store the concrete `TelegramAdapter` so we can call `pause_polling` /
    /// `resume_polling` during gap handling.
    pub fn set_telegram_adapter(&mut self, adapter: Arc<TelegramAdapter>) {
        self.telegram_adapter = Some(adapter);
    }

    pub fn subscribe_stream(&self) -> broadcast::Receiver<StreamEvent> {
        self.stream_tx.subscribe()
    }

    /// Shared handle to the banner-ack map — passed into `spawn_delivery_task`
    /// so delivery tasks can resolve pending oneshots directly.
    pub fn pending_banner_acks_handle(&self) -> PendingBannerAcks {
        Arc::clone(&self.pending_banner_acks)
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
                if name_str.starts_with("termbot-") && name_str.ends_with(".out") {
                    tracing::info!("Cleaning stale output file: {}", entry.path().display());
                    let _ = std::fs::remove_file(entry.path());
                }
                // Clean stale image temp files from previous runs
                if name_str.starts_with("termbot-img-") {
                    tracing::info!("Cleaning stale image temp file: {}", entry.path().display());
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }

        // Reconnect to surviving tb-* tmux sessions
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
                            tracing::warn!("Failed to reconnect session '{}': {}", name, e);
                        }
                    }
                }
                if let Some(fg) = self.session_mgr.foreground_session() {
                    tracing::info!("Foreground session: '{}'", fg);
                }
            }
            _ => {
                tracing::info!("No existing tmux sessions found");
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

        let chat_count = self.active_telegram_chats.len() + self.active_slack_chats.len();
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

        // 1. Pause Telegram polling.
        if let Some(adapter) = &self.telegram_adapter {
            adapter.pause_polling();
            tracing::info!(
                "Telegram polling paused for gap handling \
                 (paused_at={}, resumed_at={}, gap={:.0}s)",
                paused_at,
                resumed_at,
                gap.as_secs_f64()
            );
        }

        // 2. Broadcast a GapBanner and wait for delivery ack per chat.
        let telegram_chats: Vec<i64> = self.active_telegram_chats.iter().copied().collect();
        let slack_chats: Vec<String> = self.active_slack_chats.iter().cloned().collect();

        let all_chats: Vec<String> = telegram_chats
            .iter()
            .map(|id| id.to_string())
            .chain(slack_chats.iter().cloned())
            .collect();

        // Register pending acks and broadcast banners.  The lock is held only
        // for the duration of the inserts — never across `.await`.  The
        // delivery task resolves each oneshot directly by locking the same
        // map from its own task, so we can safely `.await` below without
        // deadlocking the main `select!` loop.
        let mut ack_futures = Vec::new();
        {
            let mut pending = self.pending_banner_acks.lock().await;
            for chat_id in &all_chats {
                let (tx, rx) = oneshot::channel::<()>();
                pending.insert(chat_id.clone(), tx);
                ack_futures.push((chat_id.clone(), rx));
            }
        } // lock released

        for chat_id in &all_chats {
            let _ = self.stream_tx.send(StreamEvent::GapBanner {
                chat_id: chat_id.clone(),
                paused_at,
                resumed_at,
                gap,
                missed_count: 0,
            });
        }

        for (chat_id, rx) in ack_futures {
            match tokio::time::timeout(Duration::from_secs(5), rx).await {
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
                    self.gap_prefix.insert(
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

        // 3. Resume Telegram polling.
        if let Some(adapter) = &self.telegram_adapter {
            adapter.resume_polling();
            tracing::info!("Telegram polling resumed after gap handling");
        }
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
            _ => {}
        }
        self.store.apply(update);
        self.updates_since_persist += 1;
        if self.updates_since_persist >= 10
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
    }

    /// Consume and return the inline gap prefix for `chat_id` if one is
    /// pending.  The delivery task can call this to prepend `[gap: Xm Ys]`
    /// to the next outbound message when banner delivery timed out.
    #[allow(dead_code)]
    pub fn consume_gap_prefix(&mut self, chat_id: &str) -> Option<GapInfo> {
        self.gap_prefix.remove(chat_id)
    }

    pub async fn handle_command(&mut self, msg: &IncomingMessage) {
        // Ensure MarkDirty is sent on the first real user interaction.
        if !self.dirty_sent {
            let _ = self.state_tx.try_send(StateUpdate::MarkDirty);
            self.dirty_sent = true;
        }

        // Bind chat IDs for active platform (so we know which chats to banner).
        match msg.reply_context.platform {
            PlatformType::Telegram => {
                if let Ok(id) = msg.reply_context.chat_id.parse::<i64>() {
                    if self.active_telegram_chats.insert(id) {
                        let _ = self.state_tx.try_send(StateUpdate::BindTelegramChat(id));
                    }
                }
            }
            PlatformType::Slack => {
                let id = msg.reply_context.chat_id.clone();
                if self.active_slack_chats.insert(id.clone()) {
                    let _ = self.state_tx.try_send(StateUpdate::BindSlackChat(id));
                }
            }
        }

        let cmd = match ParsedCommand::parse(&msg.text, self.trigger) {
            Ok(cmd) => cmd,
            Err(e) => {
                self.send_error(&msg.reply_context, &e.to_string()).await;
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
                    self.send_reply(&msg.reply_context, &format!("Session '{}' created", name))
                        .await;
                }
                Err(e) => {
                    self.send_error(&msg.reply_context, &e.to_string()).await;
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
                    self.send_error(&msg.reply_context, &e.to_string()).await;
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
                    self.send_error(&msg.reply_context, &e.to_string()).await;
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
                        self.send_error(&msg.reply_context, &e.to_string()).await;
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
                    self.send_reply(&msg.reply_context, &format!("Session '{}' killed", name))
                        .await;
                }
                Err(e) => {
                    self.send_error(&msg.reply_context, &e.to_string()).await;
                }
            },
            ParsedCommand::HarnessOn { harness: kind } => {
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
                // Check if switching from another harness
                if let Some(current) = self.session_mgr.foreground_harness() {
                    if current == kind {
                        self.send_reply(
                            &msg.reply_context,
                            &format!("{} mode already active", kind.name()),
                        )
                        .await;
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
                self.session_mgr.set_harness(&fg, Some(kind));
                self.send_reply(
                    &msg.reply_context,
                    &format!(
                        "{} mode ON. Plain text now goes to {}. Use `{} {} off` to switch back.",
                        kind.name(),
                        kind.name(),
                        self.trigger,
                        kind.name().to_lowercase()
                    ),
                )
                .await;
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
                self.session_mgr.set_harness(&fg, None);
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
            } => {
                if self.blocklist.is_blocked(prompt) {
                    cleanup_attachments(&msg.attachments).await;
                    self.send_error(
                        &msg.reply_context,
                        &format!("{} prompt blocked by security policy", kind.name()),
                    )
                    .await;
                } else {
                    self.send_harness_prompt(&msg.reply_context, &kind, prompt, &msg.attachments)
                        .await;
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
                    self.send_error(&msg.reply_context, &e.to_string()).await;
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
                    // Route to active harness
                    self.send_harness_prompt(&msg.reply_context, &kind, &text, &msg.attachments)
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
                        self.send_error(&msg.reply_context, &e.to_string()).await;
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
        let _ = self.state_tx.try_send(StateUpdate::Tick);
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

    /// Send a prompt to a harness (Claude, Gemini, Codex) with real-time streaming to chat.
    async fn send_harness_prompt(
        &self,
        ctx: &ReplyContext,
        kind: &HarnessKind,
        prompt: &str,
        attachments: &[Attachment],
    ) {
        let session_name = self
            .session_mgr
            .foreground_session()
            .unwrap_or("default")
            .to_string();

        let harness = match self.harnesses.get(kind) {
            Some(h) => h,
            None => {
                self.send_error(ctx, &format!("{} harness not available", kind.name()))
                    .await;
                return;
            }
        };
        let resume_id = harness.get_session_id(&session_name);
        let cwd = std::env::current_dir().unwrap_or_default();

        match harness
            .run_prompt(prompt, attachments, &cwd, resume_id.as_deref())
            .await
        {
            Ok(event_rx) => {
                let (got_output, session_id) = drive_harness(
                    event_rx,
                    ctx,
                    self.telegram.as_deref(),
                    self.slack.as_deref(),
                )
                .await;
                if let Some(sid) = session_id {
                    harness.set_session_id(&session_name, sid);
                }
                if !got_output {
                    self.send_error(ctx, &format!("{}: no response received", kind.name()))
                        .await;
                }
            }
            Err(e) => {
                self.send_error(ctx, &format!("{}: {}", kind.name(), e))
                    .await;
            }
        }
    }

    async fn send_reply(&self, ctx: &ReplyContext, text: &str) {
        let platform: Option<&dyn ChatPlatform> = match ctx.platform {
            PlatformType::Telegram => self.telegram.as_deref(),
            PlatformType::Slack => self.slack.as_deref(),
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

// --- Free functions (used by delivery tasks, not tied to App state) ---

/// Spawn a delivery task that consumes `StreamEvent`s from the broadcast
/// channel and sends them via `platform`.  `pending_banner_acks` is the
/// shared map used to resolve `handle_gap`'s per-chat oneshot waiters
/// directly (avoids the main-loop deadlock that an mpsc ack channel would
/// have introduced).
pub fn spawn_delivery_task(
    platform: Arc<dyn ChatPlatform>,
    mut rx: broadcast::Receiver<StreamEvent>,
    pending_banner_acks: PendingBannerAcks,
) {
    tokio::spawn(async move {
        let mut session_chat_ids: HashMap<String, String> = HashMap::new();
        let mut session_thread_ts: HashMap<String, Option<String>> = HashMap::new();

        loop {
            match rx.recv().await {
                Ok(event) => {
                    handle_stream_event(
                        &*platform,
                        event,
                        &mut session_chat_ids,
                        &mut session_thread_ts,
                        &pending_banner_acks,
                    )
                    .await;
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(
                        "{:?} delivery lagged by {} events",
                        platform.platform_type(),
                        n
                    );
                }
                Err(broadcast::error::RecvError::Closed) => {
                    tracing::info!("{:?} delivery channel closed", platform.platform_type());
                    break;
                }
            }
        }
    });
}

async fn handle_stream_event(
    platform: &dyn ChatPlatform,
    event: StreamEvent,
    session_chat_ids: &mut HashMap<String, String>,
    session_thread_ts: &mut HashMap<String, Option<String>>,
    pending_banner_acks: &PendingBannerAcks,
) {
    match event {
        StreamEvent::SessionStarted {
            session,
            chat_id,
            thread_ts,
        } => {
            session_chat_ids.insert(session.clone(), chat_id);
            session_thread_ts.insert(session, thread_ts);
        }
        StreamEvent::NewMessage { session, content } => {
            let chat_id = match session_chat_ids.get(&session) {
                Some(id) => id.clone(),
                None => {
                    tracing::warn!(
                        "[delivery] no chat_id for session '{}', dropping message ({} chars)",
                        session,
                        content.len()
                    );
                    return;
                }
            };
            let thread_ts = session_thread_ts.get(&session).and_then(|t| t.as_deref());
            // Split for Telegram's 4096-char limit
            for chunk in split_message(&content, 4000) {
                if let Err(e) = platform.send_message(&chunk, &chat_id, thread_ts).await {
                    tracing::error!("Failed to send message: {}", e);
                }
            }
        }
        StreamEvent::SessionExited { session, code } => {
            let chat_id = match session_chat_ids.get(&session) {
                Some(id) => id.clone(),
                None => return,
            };
            let thread_ts = session_thread_ts.get(&session).and_then(|t| t.as_deref());
            let msg = match code {
                Some(c) => format!("Session '{}' exited (code {})", session, c),
                None => format!("Session '{}' has exited unexpectedly", session),
            };
            let _ = platform.send_message(&msg, &chat_id, thread_ts).await;
            session_chat_ids.remove(&session);
            session_thread_ts.remove(&session);
        }
        StreamEvent::GapBanner {
            chat_id,
            paused_at,
            resumed_at,
            gap,
            missed_count,
        } => {
            // Format timestamps in local time for readability.
            let paused_local = paused_at.with_timezone(&Local);
            let resumed_local = resumed_at.with_timezone(&Local);
            let gap_mins = gap.as_secs() / 60;
            let gap_secs = gap.as_secs() % 60;
            let msg = if missed_count > 0 {
                format!(
                    "\u{23f8} paused at {}, resumed at {} (gap: {}m {}s), processing {} queued messages",
                    paused_local.format("%H:%M"),
                    resumed_local.format("%H:%M"),
                    gap_mins,
                    gap_secs,
                    missed_count,
                )
            } else {
                format!(
                    "\u{23f8} paused at {}, resumed at {} (gap: {}m {}s)",
                    paused_local.format("%H:%M"),
                    resumed_local.format("%H:%M"),
                    gap_mins,
                    gap_secs,
                )
            };
            if let Err(e) = platform.send_message(&msg, &chat_id, None).await {
                tracing::error!(
                    "Failed to send GapBanner for chat_id={}: {}",
                    chat_id,
                    e
                );
                // Do NOT resolve the oneshot on failure — let handle_gap's
                // 5s timeout fire so the inline-prefix fallback engages.
            } else {
                // Directly resolve the waiter in App::handle_gap.  Lock held
                // only for the remove+send; never across `.await` of outgoing
                // I/O.
                if let Some(tx) = pending_banner_acks.lock().await.remove(&chat_id) {
                    let _ = tx.send(());
                }
            }
        }
    }
}

/// Split a message into chunks of at most `max_len` characters,
/// breaking at newline boundaries when possible.
pub fn split_message(text: &str, max_len: usize) -> Vec<String> {
    if text.len() <= max_len {
        return vec![text.to_string()];
    }
    let mut chunks = Vec::new();
    let mut remaining = text;
    while !remaining.is_empty() {
        if remaining.len() <= max_len {
            chunks.push(remaining.to_string());
            break;
        }
        // Find a safe byte boundary (don't split inside a multi-byte char)
        let byte_limit = max_len.min(remaining.len());
        let safe_limit = match remaining.get(..byte_limit) {
            Some(_) => byte_limit,
            None => remaining
                .char_indices()
                .take_while(|(i, _)| *i <= byte_limit)
                .last()
                .map(|(i, c)| i + c.len_utf8())
                .unwrap_or(remaining.len()),
        };
        // Prefer splitting at a newline
        let split_at = remaining[..safe_limit]
            .rfind('\n')
            .map(|i| i + 1)
            .unwrap_or(safe_limit);
        chunks.push(remaining[..split_at].to_string());
        remaining = &remaining[split_at..];
    }
    chunks
}

/// Clean up temp files from attachments that won't reach the harness
/// (which normally handles its own cleanup in the spawned task).
async fn cleanup_attachments(attachments: &[Attachment]) {
    for att in attachments {
        let _ = tokio::fs::remove_file(&att.path).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::state_store::StateStore;
    use tempfile::tempdir;

    fn make_test_config(dir: &std::path::Path) -> Config {
        // Write a minimal valid termbot.toml to the temp dir.
        let toml_path = dir.join("termbot.toml");
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
        let state_path = dir.join("termbot-state.json");
        let store = StateStore::load(&state_path).expect("load state");
        let (state_tx, state_rx) = mpsc::channel::<StateUpdate>(64);
        let app = App::new(&config, store, state_tx).expect("App::new");
        (app, state_rx)
    }

    // ─── split_message tests (preserved from original) ──────────────────────

    #[test]
    fn split_short_message() {
        let chunks = split_message("hello", 4000);
        assert_eq!(chunks, vec!["hello"]);
    }

    #[test]
    fn split_at_newline_boundary() {
        let text = "line1\nline2\nline3";
        let chunks = split_message(text, 10);
        // "line1\n" = 6 chars, fits. "line2\n" would make 12, too long.
        // So first chunk is "line1\n", second is "line2\nline3"
        assert!(chunks.len() >= 2);
        assert!(chunks[0].contains("line1"));
        assert!(chunks.last().unwrap().contains("line3"));
    }

    #[test]
    fn split_unicode_safe() {
        // Create a string with multi-byte emoji that would panic if sliced at byte boundary
        let text = "a\u{1f916}b".repeat(2000); // ~6000 chars, each emoji is 4 bytes
        let chunks = split_message(&text, 4000);
        assert!(chunks.len() >= 2);
        // Verify no panic and each chunk is valid UTF-8
        for chunk in &chunks {
            assert!(chunk.len() <= 4100); // some slack for boundary finding
        }
    }

    #[test]
    fn split_exact_limit() {
        let text = "a".repeat(4000);
        let chunks = split_message(&text, 4000);
        assert_eq!(chunks.len(), 1);
    }

    #[test]
    fn split_just_over_limit() {
        let text = "a".repeat(4001);
        let chunks = split_message(&text, 4000);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].len(), 4000);
        assert_eq!(chunks[1].len(), 1);
    }

    // ─── New sleep/wake recovery tests ──────────────────────────────────────

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
        assert!(dir.path().join("termbot-state.json").exists());
    }

    #[tokio::test]
    async fn mark_clean_shutdown_persists_true() {
        let dir = tempdir().unwrap();
        let (mut app, _state_rx) = make_app(dir.path());

        // Trigger some activity.
        app.apply_state_update(StateUpdate::TelegramOffset(1)).await;
        app.mark_clean_shutdown().await;

        // Reload from disk and check.
        let reloaded = StateStore::load(dir.path().join("termbot-state.json")).unwrap();
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
        let state_path = dir.path().join("termbot-state.json");
        std::fs::write(&state_path, state.to_string()).unwrap();

        let config = make_test_config(dir.path());
        let store = StateStore::load(&state_path).unwrap();
        let (state_tx, _state_rx) = mpsc::channel::<StateUpdate>(64);
        let mut app = App::new(&config, store, state_tx).unwrap();

        // Subscribe before emitting.
        let mut sub = app.subscribe_stream();

        app.emit_startup_gap_banners();

        // Should receive a GapBanner for chat 111.
        let event = tokio::time::timeout(
            Duration::from_millis(100),
            sub.recv(),
        )
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
        std::fs::write(dir.path().join("termbot-state.json"), state.to_string()).unwrap();

        let config = make_test_config(dir.path());
        let store = StateStore::load(dir.path().join("termbot-state.json")).unwrap();
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
        std::fs::write(dir.path().join("termbot-state.json"), state.to_string()).unwrap();

        let config = make_test_config(dir.path());
        let store = StateStore::load(dir.path().join("termbot-state.json")).unwrap();
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
        // map, not via an mpsc the main loop drains.  We simulate that here
        // by taking the handle and resolving a pending oneshot through it.
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

        // The handle_gap waiter would see this without the main loop ever
        // running.  We assert the same here.
        assert!(rx.await.is_ok(), "oneshot should be resolved directly");
        assert!(
            acks.lock().await.is_empty(),
            "pending_banner_acks should be cleared"
        );
    }

    #[tokio::test]
    async fn handle_gap_completes_when_delivery_resolves_directly() {
        // End-to-end regression test for the Architect-identified deadlock.
        // Before the fix: handle_gap awaited on a oneshot whose sender could
        // only be produced by a main-loop branch the awaiting task itself was
        // blocking — so every gap hit the 5s timeout and engaged the inline
        // fallback unconditionally.  After the fix: a simulated delivery task
        // resolves the oneshot directly via the shared map handle, and
        // handle_gap should return in ~milliseconds, well under the 5s bound.
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
                    // Brief lock + remove + resolve — the pattern the real
                    // delivery task uses on a successful send.
                    if let Some(tx) = acks.lock().await.remove(&chat_id) {
                        let _ = tx.send(());
                    }
                    return; // one banner is enough for this test
                }
            }
        });

        let signal = PowerSignal::GapDetected {
            paused_at: Utc::now() - chrono::Duration::seconds(90),
            resumed_at: Utc::now(),
            gap: Duration::from_secs(90),
        };

        // Timebox the whole call — if deadlocked, this would hit 5s (the
        // internal timeout) + some overhead.  We allow up to 1s for ample
        // scheduling slack on slow CI; a deadlocked version would blow past.
        let result = tokio::time::timeout(Duration::from_secs(1), app.handle_gap(signal)).await;
        assert!(
            result.is_ok(),
            "handle_gap deadlocked or timed out beyond the 1s bound"
        );

        // And no gap_prefix fallback should have been engaged (meaning the
        // ack was received cleanly, not via the timeout path).
        assert!(
            app.gap_prefix.is_empty(),
            "gap_prefix should be empty when the ack resolved in time; got {:?}",
            app.gap_prefix
        );

        // Clean up the delivery task.
        let _ = tokio::time::timeout(Duration::from_millis(100), delivery_task).await;
    }

    #[test]
    fn consume_gap_prefix_returns_and_clears() {
        let dir = tempdir().unwrap();
        let (mut app, _state_rx) = {
            let config = make_test_config(dir.path());
            let store =
                StateStore::load(dir.path().join("termbot-state.json")).expect("load state");
            let (state_tx, state_rx) = mpsc::channel::<StateUpdate>(64);
            let app = App::new(&config, store, state_tx).expect("App::new");
            (app, state_rx)
        };

        let now = Utc::now();
        app.gap_prefix.insert(
            "chat42".to_string(),
            GapInfo {
                gap: Duration::from_secs(90),
                paused_at: now - chrono::Duration::seconds(90),
                resumed_at: now,
            },
        );

        let prefix = app.consume_gap_prefix("chat42");
        assert!(prefix.is_some(), "should return the gap info");
        assert_eq!(prefix.unwrap().gap, Duration::from_secs(90));
        // Second call: cleared.
        assert!(app.consume_gap_prefix("chat42").is_none());
    }
}
