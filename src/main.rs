mod buffer;
mod claude;
mod command;
mod config;
mod platform;
mod session;
mod tmux;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tokio::sync::{broadcast, mpsc};
use tracing;

use buffer::{OutputBuffer, StreamEvent};
use claude::{ClaudeEvent, ClaudeManager, format_tool_event};
use command::{CommandBlocklist, ParsedCommand};
use config::Config;
use platform::slack::SlackPlatform;
use platform::telegram::TelegramAdapter;
use platform::{ChatPlatform, IncomingMessage, PlatformType, ReplyContext};
use session::SessionManager;
use tmux::TmuxClient;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    tracing::info!("Starting termbot...");

    let config = Config::load("termbot.toml")?;
    tracing::info!("Config loaded successfully");

    let blocklist = CommandBlocklist::from_config(&config.blocklist.patterns)?;
    let mut session_mgr = SessionManager::new(TmuxClient::new());
    let mut buffers: HashMap<String, OutputBuffer> = HashMap::new();
    let cwd = std::env::current_dir()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| ".".to_string());
    let claude_mgr = ClaudeManager::new(cwd);
    let mut claude_mode = false; // when true, plain text routes to Claude SDK

    let chunk_size = config.streaming.chunk_size;
    let offline_buffer_max = config.streaming.offline_buffer_max_bytes;

    // Startup reconciliation — reconnect surviving sessions
    reconcile_startup(&mut session_mgr, &mut buffers, chunk_size, offline_buffer_max).await;

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
        spawn_delivery_task(Arc::clone(&adapter) as Arc<dyn ChatPlatform>, stream_tx.subscribe());
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
        spawn_delivery_task(Arc::clone(&adapter) as Arc<dyn ChatPlatform>, stream_tx.subscribe());
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
    tracing::info!("termbot ready — listening on {}", platforms_desc.join(" and "));

    loop {
        tokio::select! {
            msg = cmd_rx.recv() => {
                match msg {
                    Some(msg) => {
                        handle_command(
                            &msg,
                            &blocklist,
                            &mut session_mgr,
                            &mut buffers,
                            &claude_mgr,
                            &mut claude_mode,
                            &stream_tx,
                            telegram.as_deref(),
                            slack.as_deref(),
                            chunk_size,
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
                let tmux = session_mgr.tmux();
                for buffer in buffers.values_mut() {
                    let events = buffer.poll(tmux).await;
                    for event in events {
                        let _ = stream_tx.send(event);
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
    claude_mgr: &ClaudeManager,
    claude_mode: &mut bool,
    stream_tx: &broadcast::Sender<StreamEvent>,
    telegram: Option<&dyn ChatPlatform>,
    slack: Option<&dyn ChatPlatform>,
    chunk_size: usize,
    offline_buffer_max: usize,
) {
    let cmd = match ParsedCommand::parse(&msg.text) {
        Ok(cmd) => cmd,
        Err(e) => {
            send_error(&msg.reply_context, &e.to_string(), telegram, slack).await;
            return;
        }
    };

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
        ParsedCommand::NewSession { name } => {
            match session_mgr.new_session(&name).await {
                Ok(()) => {
                    let mut buf = OutputBuffer::new(&name, chunk_size, offline_buffer_max);
                    buf.sync_offset(session_mgr.tmux()).await;
                    buffers.insert(name.clone(), buf);
                    let _ = stream_tx.send(StreamEvent::SessionStarted {
                        session: name.clone(),
                        chat_id: msg.reply_context.chat_id.clone(),
                        thread_ts: msg.reply_context.thread_ts.clone(),
                    });
                    send_reply(&msg.reply_context, &format!("Session '{}' created", name), telegram, slack).await;
                }
                Err(e) => {
                    send_error(&msg.reply_context, &e.to_string(), telegram, slack).await;
                }
            }
        }
        ParsedCommand::Foreground { name } => match session_mgr.fg(&name) {
            Ok(()) => {
                let _ = stream_tx.send(StreamEvent::SessionStarted {
                    session: name.clone(),
                    chat_id: msg.reply_context.chat_id.clone(),
                    thread_ts: msg.reply_context.thread_ts.clone(),
                });
                send_reply(&msg.reply_context, &format!("Session '{}' foregrounded", name), telegram, slack).await;
            }
            Err(e) => {
                send_error(&msg.reply_context, &e.to_string(), telegram, slack).await;
            }
        },
        ParsedCommand::Background => match session_mgr.bg() {
            Ok(Some(name)) => {
                send_reply(&msg.reply_context, &format!("Session '{}' backgrounded", name), telegram, slack).await;
            }
            Ok(None) => {
                send_error(&msg.reply_context, "No foreground session to background", telegram, slack).await;
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
                    lines.push(format!("  {} {} (uptime: {}s)", name, status_str, elapsed.as_secs()));
                }
                send_reply(&msg.reply_context, &lines.join("\n"), telegram, slack).await;
            }
        }
        ParsedCommand::KillSession { name } => {
            match session_mgr.kill(&name).await {
                Ok(()) => {
                    buffers.remove(&name);
                    send_reply(&msg.reply_context, &format!("Session '{}' killed", name), telegram, slack).await;
                }
                Err(e) => {
                    send_error(&msg.reply_context, &e.to_string(), telegram, slack).await;
                }
            }
        }
        ParsedCommand::ClaudeOn => {
            *claude_mode = true;
            send_reply(
                &msg.reply_context,
                "Claude mode ON. Plain text now goes to Claude Code. Use `: claude off` to switch back to terminal.",
                telegram, slack,
            ).await;
        }
        ParsedCommand::ClaudeOff => {
            *claude_mode = false;
            send_reply(
                &msg.reply_context,
                "Claude mode OFF. Plain text now goes to the terminal session.",
                telegram, slack,
            ).await;
        }
        ParsedCommand::Claude { prompt } => {
            send_claude_prompt(&msg.reply_context, &prompt, session_mgr, claude_mgr, telegram, slack).await;
        }
        ParsedCommand::ShellCommand { cmd } => {
            // Snapshot pane before command so we can diff afterward
            if let Some(fg) = session_mgr.foreground_session() {
                if let Some(buf) = buffers.get_mut(fg) {
                    buf.snapshot_before_command(session_mgr.tmux(), Some(&cmd)).await;
                }
            }
            if let Err(e) = session_mgr.execute_in_foreground(&cmd).await {
                send_error(&msg.reply_context, &e.to_string(), telegram, slack).await;
            }
        }
        ParsedCommand::StdinInput { text } => {
            if *claude_mode {
                send_claude_prompt(&msg.reply_context, &text, session_mgr, claude_mgr, telegram, slack).await;
            } else {
                // Blocklist check on plain text too — it's sent to the shell as a command
                if blocklist.is_blocked(&text) {
                    send_error(&msg.reply_context, "Command blocked by security policy", telegram, slack).await;
                    return;
                }
                if let Some(fg) = session_mgr.foreground_session() {
                    if let Some(buf) = buffers.get_mut(fg) {
                        buf.snapshot_before_command(session_mgr.tmux(), Some(&text)).await;
                    }
                }
                if let Err(e) = session_mgr.send_stdin_to_foreground(&text).await {
                    send_error(&msg.reply_context, &e.to_string(), telegram, slack).await;
                }
            }
        }
    }
}

/// Send a prompt to Claude Code via SDK with real-time streaming to chat.
/// Shows tool activity (Read, Write, Edit, Bash) as it happens,
/// then delivers the final response.
async fn send_claude_prompt(
    ctx: &ReplyContext,
    prompt: &str,
    session_mgr: &SessionManager,
    claude_mgr: &ClaudeManager,
    telegram: Option<&dyn ChatPlatform>,
    slack: Option<&dyn ChatPlatform>,
) {
    let session_name = session_mgr
        .foreground_session()
        .unwrap_or("default")
        .to_string();

    let (event_tx, mut event_rx) = mpsc::channel::<ClaudeEvent>(64);

    // Gather what we need for the spawned task (all owned types, no references)
    let prompt_owned = prompt.to_string();
    let cwd = claude_mgr.cwd.clone();
    let resume_session = claude_mgr.get_session_id(&session_name).await;
    let session_name_clone = session_name.clone();

    // Spawn Claude in a background task
    let prompt_handle = tokio::spawn(async move {
        claude::run_claude_prompt(prompt_owned, cwd, resume_session, event_tx).await
    });

    // Consume events and deliver to chat in real-time
    let mut tool_lines: Vec<String> = Vec::new();
    let mut last_tool_flush = tokio::time::Instant::now();

    while let Some(event) = event_rx.recv().await {
        match event {
            ClaudeEvent::ToolUse { tool, description } => {
                tool_lines.push(format_tool_event(&tool, &description));
                // Batch: send every 3s or every 5 tools
                if tool_lines.len() >= 5
                    || last_tool_flush.elapsed() > std::time::Duration::from_secs(3)
                {
                    send_reply(ctx, &tool_lines.join("\n"), telegram, slack).await;
                    tool_lines.clear();
                    last_tool_flush = tokio::time::Instant::now();
                }
            }
            ClaudeEvent::Text(text) => {
                if !tool_lines.is_empty() {
                    send_reply(ctx, &tool_lines.join("\n"), telegram, slack).await;
                    tool_lines.clear();
                }
                if !text.is_empty() {
                    for chunk in split_message(&text, 4000) {
                        send_reply(ctx, &chunk, telegram, slack).await;
                    }
                }
            }
            ClaudeEvent::Done { .. } => {
                if !tool_lines.is_empty() {
                    send_reply(ctx, &tool_lines.join("\n"), telegram, slack).await;
                }
                break;
            }
            ClaudeEvent::Error(e) => {
                if !tool_lines.is_empty() {
                    send_reply(ctx, &tool_lines.join("\n"), telegram, slack).await;
                }
                send_error(ctx, &format!("Claude: {}", e), telegram, slack).await;
                break;
            }
        }
    }

    // Store the session ID for future resume
    if let Ok(Some(new_session_id)) = prompt_handle.await {
        claude_mgr
            .set_session_id(&session_name_clone, new_session_id)
            .await;
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
        if let Err(e) = p.send_message(text, &ctx.chat_id, ctx.thread_ts.as_deref()).await {
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

fn spawn_delivery_task(
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
                    tracing::warn!("{:?} delivery lagged by {} events", platform.platform_type(), n);
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
        StreamEvent::SessionStarted { session, chat_id, thread_ts } => {
            session_chat_ids.insert(session.clone(), chat_id);
            session_thread_ts.insert(session, thread_ts);
        }
        StreamEvent::NewMessage { session, content } | StreamEvent::EditMessage { session, content } => {
            let chat_id = match session_chat_ids.get(&session) {
                Some(id) => id.clone(),
                None => return,
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
    chunk_size: usize,
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
        }
    }

    // Reconnect to surviving tb-* tmux sessions
    match session_mgr.tmux().list_sessions().await {
        Ok(sessions) if !sessions.is_empty() => {
            tracing::info!("Found {} existing tmux session(s), reconnecting...", sessions.len());
            for name in sessions {
                match session_mgr.reconnect_session(&name).await {
                    Ok(()) => {
                        let mut buf = OutputBuffer::new(&name, chunk_size, offline_buffer_max);
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
