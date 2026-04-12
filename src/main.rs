mod buffer;
mod command;
mod config;
mod harness;
mod platform;
mod session;
mod tmux;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tokio::sync::{broadcast, mpsc};

use buffer::{OutputBuffer, StreamEvent};
use command::{CommandBlocklist, ParsedCommand};
use config::Config;
use harness::claude::ClaudeHarness;
use harness::{drive_harness, Harness, HarnessKind};
use platform::slack::SlackPlatform;
use platform::telegram::TelegramAdapter;
use platform::{Attachment, ChatPlatform, IncomingMessage, PlatformType, ReplyContext};
use session::SessionManager;
use tmux::TmuxClient;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    tracing::info!("Starting termbot...");

    let config_path =
        std::env::var("TERMBOT_CONFIG").unwrap_or_else(|_| "termbot.toml".to_string());
    let config = Config::load(&config_path)?;
    tracing::info!("Config loaded successfully");

    let blocklist = CommandBlocklist::from_config(&config.blocklist.patterns)?;
    let mut session_mgr = SessionManager::new(TmuxClient::new(), config.streaming.max_sessions);
    let mut buffers: HashMap<String, OutputBuffer> = HashMap::new();
    let cwd = std::env::current_dir()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| ".".to_string());
    let harnesses: HashMap<HarnessKind, Box<dyn Harness>> = {
        let mut h: HashMap<HarnessKind, Box<dyn Harness>> = HashMap::new();
        h.insert(HarnessKind::Claude, Box::new(ClaudeHarness::new(cwd)));
        h
    };

    let offline_buffer_max = config.streaming.offline_buffer_max_bytes;

    // Startup reconciliation — reconnect surviving sessions
    reconcile_startup(&mut session_mgr, &mut buffers, offline_buffer_max).await;

    // Channels
    let (cmd_tx, mut cmd_rx) = mpsc::channel::<IncomingMessage>(100);
    let (stream_tx, _) = broadcast::channel::<StreamEvent>(256);

    // Conditionally create and start platform adapters
    let telegram: Option<Arc<dyn ChatPlatform>> = if config.telegram_enabled() {
        let tg_config = config.telegram.as_ref().unwrap();
        let tg_user_id = config.auth.telegram_user_id.unwrap();
        let adapter = Arc::new(TelegramAdapter::new(
            tg_config.bot_token.clone(),
            tg_user_id,
            config.streaming.edit_throttle_ms,
        ));
        adapter.start(cmd_tx.clone()).await?;
        spawn_delivery_task(
            Arc::clone(&adapter) as Arc<dyn ChatPlatform>,
            stream_tx.subscribe(),
        );
        tracing::info!("Telegram adapter started (user_id={})", tg_user_id);
        Some(adapter)
    } else {
        tracing::info!("Telegram not configured, skipping");
        None
    };

    let slack: Option<Arc<dyn ChatPlatform>> = if config.slack_enabled() {
        let sl_config = config.slack.as_ref().unwrap();
        let sl_user_id = config.auth.slack_user_id.as_ref().unwrap().clone();
        let adapter = Arc::new(SlackPlatform::new(
            sl_config.bot_token.clone(),
            sl_config.app_token.clone(),
            sl_config.channel_id.clone(),
            sl_user_id.clone(),
            config.streaming.edit_throttle_ms,
        ));
        let adapter_clone = Arc::clone(&adapter);
        let cmd_tx_clone = cmd_tx.clone();
        tokio::spawn(async move {
            if let Err(e) = adapter_clone.start(cmd_tx_clone).await {
                tracing::error!("Slack adapter error: {}", e);
            }
        });
        spawn_delivery_task(
            Arc::clone(&adapter) as Arc<dyn ChatPlatform>,
            stream_tx.subscribe(),
        );
        tracing::info!("Slack adapter started (user_id={})", sl_user_id);
        Some(adapter)
    } else {
        tracing::info!("Slack not configured, skipping");
        None
    };

    drop(cmd_tx); // Drop our copy so channel closes when adapters stop

    // Timers
    let mut health_interval = tokio::time::interval(Duration::from_secs(5));
    let mut poll_interval =
        tokio::time::interval(Duration::from_millis(config.streaming.poll_interval_ms));

    let platforms_desc: Vec<&str> = [
        telegram.as_ref().map(|_| "Telegram"),
        slack.as_ref().map(|_| "Slack"),
    ]
    .into_iter()
    .flatten()
    .collect();
    tracing::info!(
        "termbot ready — listening on {}",
        platforms_desc.join(" and ")
    );

    loop {
        tokio::select! {
            biased;

            msg = cmd_rx.recv() => {
                match msg {
                    Some(msg) => {
                        handle_command(
                            &msg,
                            &blocklist,
                            &mut session_mgr,
                            &mut buffers,
                            &harnesses,
                            &stream_tx,
                            telegram.as_deref(),
                            slack.as_deref(),
                            offline_buffer_max,
                        ).await;
                    }
                    None => {
                        tracing::info!("All platform adapters disconnected, shutting down");
                        break;
                    }
                }
            }

            _ = health_interval.tick() => {
                let crashed = session_mgr.health_check().await;
                for (name, code) in crashed {
                    buffers.remove(&name);
                    let _ = stream_tx.send(StreamEvent::SessionExited {
                        session: name,
                        code,
                    });
                }
            }

            _ = poll_interval.tick() => {
                // Only poll the foreground session — keeps cost low (one tmux subprocess per tick)
                if let Some(fg) = session_mgr.foreground_session() {
                    if let Some(buffer) = buffers.get_mut(fg) {
                        let tmux = session_mgr.tmux();
                        let events = buffer.poll(tmux).await;
                        if !events.is_empty() {
                            tracing::info!("[poll] {} event(s) from session '{}'", events.len(), fg);
                        }
                        for event in events {
                            let _ = stream_tx.send(event);
                        }
                    }
                }
            }

            _ = tokio::signal::ctrl_c() => {
                tracing::info!("Shutting down...");
                session_mgr.cleanup_all().await;
                break;
            }
        }
    }

    tracing::info!("termbot stopped");
    Ok(())
}

async fn handle_command(
    msg: &IncomingMessage,
    blocklist: &CommandBlocklist,
    session_mgr: &mut SessionManager,
    buffers: &mut HashMap<String, OutputBuffer>,
    harnesses: &HashMap<HarnessKind, Box<dyn Harness>>,
    stream_tx: &broadcast::Sender<StreamEvent>,
    telegram: Option<&dyn ChatPlatform>,
    slack: Option<&dyn ChatPlatform>,
    offline_buffer_max: usize,
) {
    let cmd = match ParsedCommand::parse(&msg.text) {
        Ok(cmd) => cmd,
        Err(e) => {
            send_error(&msg.reply_context, &e.to_string(), telegram, slack).await;
            return;
        }
    };

    tracing::info!(
        "handle_command: {:?} (fg={:?})",
        cmd,
        session_mgr.foreground_session()
    );

    // Clean up image temp files for commands that won't route to a harness.
    // Harness paths (HarnessPrompt, StdinInput→harness) handle their own cleanup
    // inside the spawned harness task.
    let harness_will_handle = matches!(
        cmd,
        ParsedCommand::HarnessPrompt { .. } | ParsedCommand::StdinInput { .. }
    );
    if !harness_will_handle && !msg.attachments.is_empty() {
        cleanup_attachments(&msg.attachments).await;
    }

    // Always bind the foreground session to the current chat context.
    // This ensures delivery tasks know where to send output — critical after
    // restart (reconnected sessions have no chat binding) and for cross-platform use.
    if let Some(fg) = session_mgr.foreground_session() {
        let _ = stream_tx.send(StreamEvent::SessionStarted {
            session: fg.to_string(),
            chat_id: msg.reply_context.chat_id.clone(),
            thread_ts: msg.reply_context.thread_ts.clone(),
        });
    }

    match cmd {
        ParsedCommand::ShellCommand { ref cmd } if blocklist.is_blocked(cmd) => {
            send_error(
                &msg.reply_context,
                "Command blocked by security policy",
                telegram,
                slack,
            )
            .await;
        }
        ParsedCommand::NewSession { name } => match session_mgr.new_session(&name).await {
            Ok(()) => {
                let mut buf = OutputBuffer::new(&name, offline_buffer_max);
                buf.sync_offset(session_mgr.tmux()).await;
                buffers.insert(name.clone(), buf);
                let _ = stream_tx.send(StreamEvent::SessionStarted {
                    session: name.clone(),
                    chat_id: msg.reply_context.chat_id.clone(),
                    thread_ts: msg.reply_context.thread_ts.clone(),
                });
                send_reply(
                    &msg.reply_context,
                    &format!("Session '{}' created", name),
                    telegram,
                    slack,
                )
                .await;
            }
            Err(e) => {
                send_error(&msg.reply_context, &e.to_string(), telegram, slack).await;
            }
        },
        ParsedCommand::Foreground { name } => match session_mgr.fg(&name) {
            Ok(()) => {
                let _ = stream_tx.send(StreamEvent::SessionStarted {
                    session: name.clone(),
                    chat_id: msg.reply_context.chat_id.clone(),
                    thread_ts: msg.reply_context.thread_ts.clone(),
                });
                send_reply(
                    &msg.reply_context,
                    &format!("Session '{}' foregrounded", name),
                    telegram,
                    slack,
                )
                .await;
            }
            Err(e) => {
                send_error(&msg.reply_context, &e.to_string(), telegram, slack).await;
            }
        },
        ParsedCommand::Background => match session_mgr.bg() {
            Ok(Some(name)) => {
                send_reply(
                    &msg.reply_context,
                    &format!("Session '{}' backgrounded", name),
                    telegram,
                    slack,
                )
                .await;
            }
            Ok(None) => {
                send_error(
                    &msg.reply_context,
                    "No foreground session to background",
                    telegram,
                    slack,
                )
                .await;
            }
            Err(e) => {
                send_error(&msg.reply_context, &e.to_string(), telegram, slack).await;
            }
        },
        ParsedCommand::ListSessions => {
            let sessions = session_mgr.list();
            if sessions.is_empty() {
                send_reply(&msg.reply_context, "No active sessions", telegram, slack).await;
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
                send_reply(&msg.reply_context, &lines.join("\n"), telegram, slack).await;
            }
        }
        ParsedCommand::Screen => match session_mgr.foreground_session() {
            Some(fg) => match session_mgr.tmux().capture_pane(fg).await {
                Ok(screen) => {
                    let trimmed = screen
                        .lines()
                        .map(|l| l.trim_end())
                        .collect::<Vec<_>>()
                        .join("\n")
                        .trim()
                        .to_string();
                    if trimmed.is_empty() {
                        send_reply(&msg.reply_context, "(empty screen)", telegram, slack).await;
                    } else {
                        for chunk in split_message(&trimmed, 4000) {
                            send_reply(&msg.reply_context, &chunk, telegram, slack).await;
                        }
                    }
                }
                Err(e) => {
                    send_error(&msg.reply_context, &e.to_string(), telegram, slack).await;
                }
            },
            None => {
                send_error(&msg.reply_context, "No active session", telegram, slack).await;
            }
        },
        ParsedCommand::KillSession { name } => match session_mgr.kill(&name).await {
            Ok(()) => {
                buffers.remove(&name);
                send_reply(
                    &msg.reply_context,
                    &format!("Session '{}' killed", name),
                    telegram,
                    slack,
                )
                .await;
            }
            Err(e) => {
                send_error(&msg.reply_context, &e.to_string(), telegram, slack).await;
            }
        },
        ParsedCommand::HarnessOn { harness: kind } => {
            // Verify the harness is actually registered (stubs are not)
            if !harnesses.contains_key(&kind) {
                send_error(
                    &msg.reply_context,
                    &format!("{} harness is not available", kind.name()),
                    telegram,
                    slack,
                )
                .await;
                return;
            }
            let fg = match session_mgr.foreground_session() {
                Some(fg) => fg.to_string(),
                None => {
                    send_error(&msg.reply_context, "No active session", telegram, slack).await;
                    return;
                }
            };
            // Check if switching from another harness
            if let Some(current) = session_mgr.foreground_harness() {
                if current == kind {
                    send_reply(
                        &msg.reply_context,
                        &format!("{} mode already active", kind.name()),
                        telegram,
                        slack,
                    )
                    .await;
                    return;
                }
                // Check resume support of the CURRENT harness before switching away
                if let Some(h) = harnesses.get(&current) {
                    if !h.supports_resume() {
                        send_error(
                            &msg.reply_context,
                            &format!(
                                "{} is active but doesn't support resume. Run `: {} off` first to avoid losing context.",
                                current.name(),
                                current.name().to_lowercase()
                            ),
                            telegram,
                            slack,
                        )
                        .await;
                        return;
                    }
                }
                send_reply(
                    &msg.reply_context,
                    &format!(
                        "Switching from {} to {} ({} session preserved)",
                        current.name(),
                        kind.name(),
                        current.name()
                    ),
                    telegram,
                    slack,
                )
                .await;
            }
            session_mgr.set_harness(&fg, Some(kind));
            send_reply(
                &msg.reply_context,
                &format!(
                    "{} mode ON. Plain text now goes to {}. Use `: {} off` to switch back.",
                    kind.name(),
                    kind.name(),
                    kind.name().to_lowercase()
                ),
                telegram,
                slack,
            )
            .await;
        }
        ParsedCommand::HarnessOff { harness: kind } => {
            let fg = match session_mgr.foreground_session() {
                Some(fg) => fg.to_string(),
                None => {
                    send_error(&msg.reply_context, "No active session", telegram, slack).await;
                    return;
                }
            };
            // Verify the specified harness is actually the active one
            let current = session_mgr.foreground_harness();
            if current != Some(kind) {
                send_error(
                    &msg.reply_context,
                    &format!(
                        "{} is not active{}",
                        kind.name(),
                        current
                            .map(|c| format!(" (current: {})", c.name()))
                            .unwrap_or_default()
                    ),
                    telegram,
                    slack,
                )
                .await;
                return;
            }
            session_mgr.set_harness(&fg, None);
            send_reply(
                &msg.reply_context,
                &format!(
                    "{} mode OFF. Plain text now goes to the terminal session.",
                    kind.name()
                ),
                telegram,
                slack,
            )
            .await;
        }
        ParsedCommand::HarnessPrompt {
            harness: kind,
            ref prompt,
        } => {
            if blocklist.is_blocked(prompt) {
                cleanup_attachments(&msg.attachments).await;
                send_error(
                    &msg.reply_context,
                    &format!("{} prompt blocked by security policy", kind.name()),
                    telegram,
                    slack,
                )
                .await;
            } else {
                send_harness_prompt(
                    &msg.reply_context,
                    &kind,
                    prompt,
                    &msg.attachments,
                    session_mgr,
                    harnesses,
                    telegram,
                    slack,
                )
                .await;
            }
        }
        ParsedCommand::ShellCommand { cmd } => {
            // Snapshot pane before command so we can diff afterward
            match session_mgr.foreground_session() {
                None => {
                    tracing::warn!("ShellCommand '{}': no foreground session", cmd);
                }
                Some(fg) => {
                    if let Some(buf) = buffers.get_mut(fg) {
                        buf.snapshot_before_command(session_mgr.tmux(), Some(&cmd))
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
            if let Err(e) = session_mgr.execute_in_foreground(&cmd).await {
                send_error(&msg.reply_context, &e.to_string(), telegram, slack).await;
            }
        }
        ParsedCommand::StdinInput { text } => {
            // Single blocklist check regardless of routing
            if blocklist.is_blocked(&text) {
                cleanup_attachments(&msg.attachments).await;
                send_error(
                    &msg.reply_context,
                    "Command blocked by security policy",
                    telegram,
                    slack,
                )
                .await;
                return;
            }
            if let Some(kind) = session_mgr.foreground_harness() {
                // Route to active harness
                send_harness_prompt(
                    &msg.reply_context,
                    &kind,
                    &text,
                    &msg.attachments,
                    session_mgr,
                    harnesses,
                    telegram,
                    slack,
                )
                .await;
            } else {
                // Images are only supported in harness mode
                if !msg.attachments.is_empty() {
                    cleanup_attachments(&msg.attachments).await;
                    send_error(
                        &msg.reply_context,
                        "Images are only supported in harness mode. Use `: claude on` first.",
                        telegram,
                        slack,
                    )
                    .await;
                    return;
                }
                // Route to terminal
                if let Some(fg) = session_mgr.foreground_session() {
                    if let Some(buf) = buffers.get_mut(fg) {
                        buf.snapshot_before_command(session_mgr.tmux(), Some(&text))
                            .await;
                    }
                }
                if let Err(e) = session_mgr.send_stdin_to_foreground(&text).await {
                    send_error(&msg.reply_context, &e.to_string(), telegram, slack).await;
                }
            }
        }
    }
}

/// Send a prompt to a harness (Claude, Gemini, Codex) with real-time streaming to chat.
async fn send_harness_prompt(
    ctx: &ReplyContext,
    kind: &HarnessKind,
    prompt: &str,
    attachments: &[platform::Attachment],
    session_mgr: &SessionManager,
    harnesses: &HashMap<HarnessKind, Box<dyn Harness>>,
    telegram: Option<&dyn ChatPlatform>,
    slack: Option<&dyn ChatPlatform>,
) {
    let session_name = session_mgr
        .foreground_session()
        .unwrap_or("default")
        .to_string();

    let harness = match harnesses.get(kind) {
        Some(h) => h,
        None => {
            send_error(
                ctx,
                &format!("{} harness not available", kind.name()),
                telegram,
                slack,
            )
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
            let (got_output, session_id) = drive_harness(event_rx, ctx, telegram, slack).await;
            if let Some(sid) = session_id {
                harness.set_session_id(&session_name, sid);
            }
            if !got_output {
                send_error(
                    ctx,
                    &format!("{}: no response received", kind.name()),
                    telegram,
                    slack,
                )
                .await;
            }
        }
        Err(e) => {
            send_error(ctx, &format!("{}: {}", kind.name(), e), telegram, slack).await;
        }
    }
}

/// Split a message into chunks of at most `max_len` characters,
/// breaking at newline boundaries when possible.
fn split_message(text: &str, max_len: usize) -> Vec<String> {
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

async fn send_reply(
    ctx: &ReplyContext,
    text: &str,
    telegram: Option<&dyn ChatPlatform>,
    slack: Option<&dyn ChatPlatform>,
) {
    let platform: Option<&dyn ChatPlatform> = match ctx.platform {
        PlatformType::Telegram => telegram,
        PlatformType::Slack => slack,
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

async fn send_error(
    ctx: &ReplyContext,
    error: &str,
    telegram: Option<&dyn ChatPlatform>,
    slack: Option<&dyn ChatPlatform>,
) {
    send_reply(ctx, &format!("Error: {}", error), telegram, slack).await;
}

/// Clean up temp files from attachments that won't reach the harness
/// (which normally handles its own cleanup in the spawned task).
async fn cleanup_attachments(attachments: &[Attachment]) {
    for att in attachments {
        let _ = tokio::fs::remove_file(&att.path).await;
    }
}

fn spawn_delivery_task(platform: Arc<dyn ChatPlatform>, mut rx: broadcast::Receiver<StreamEvent>) {
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

async fn reconcile_startup(
    session_mgr: &mut SessionManager,
    buffers: &mut HashMap<String, OutputBuffer>,
    offline_buffer_max: usize,
) {
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
    match session_mgr.tmux().list_sessions().await {
        Ok(sessions) if !sessions.is_empty() => {
            tracing::info!(
                "Found {} existing tmux session(s), reconnecting...",
                sessions.len()
            );
            for name in sessions {
                match session_mgr.reconnect_session(&name).await {
                    Ok(()) => {
                        let mut buf = OutputBuffer::new(&name, offline_buffer_max);
                        buf.sync_offset(session_mgr.tmux()).await;
                        buffers.insert(name.clone(), buf);
                        tracing::info!("Reconnected session '{}'", name);
                    }
                    Err(e) => {
                        tracing::warn!("Failed to reconnect session '{}': {}", name, e);
                    }
                }
            }
            if let Some(fg) = session_mgr.foreground_session() {
                tracing::info!("Foreground session: '{}'", fg);
            }
        }
        _ => {
            tracing::info!("No existing tmux sessions found");
        }
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
        let text = "a]b".repeat(2000); // ~6000 chars, each emoji is 4 bytes
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
