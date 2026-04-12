use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use tokio::sync::broadcast;

use crate::buffer::{OutputBuffer, StreamEvent};
use crate::command::{CommandBlocklist, ParsedCommand};
use crate::config::Config;
use crate::harness::claude::ClaudeHarness;
use crate::harness::{drive_harness, Harness, HarnessKind};
use crate::platform::{Attachment, ChatPlatform, IncomingMessage, PlatformType, ReplyContext};
use crate::session::{self, SessionManager};
use crate::tmux::TmuxClient;

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
}

impl App {
    pub fn new(config: &Config) -> Result<Self> {
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

        Ok(Self {
            blocklist,
            session_mgr,
            buffers: HashMap::new(),
            harnesses,
            stream_tx,
            telegram: None,
            slack: None,
            offline_buffer_max: config.streaming.offline_buffer_max_bytes,
            trigger,
        })
    }

    pub fn set_platforms(
        &mut self,
        telegram: Option<Arc<dyn ChatPlatform>>,
        slack: Option<Arc<dyn ChatPlatform>>,
    ) {
        self.telegram = telegram;
        self.slack = slack;
    }

    pub fn subscribe_stream(&self) -> broadcast::Receiver<StreamEvent> {
        self.stream_tx.subscribe()
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

    pub async fn handle_command(&mut self, msg: &IncomingMessage) {
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

pub fn spawn_delivery_task(
    platform: Arc<dyn ChatPlatform>,
    mut rx: broadcast::Receiver<StreamEvent>,
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
}
