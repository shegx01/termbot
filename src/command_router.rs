//! `command_router` — routes parsed commands to the appropriate handler.
//! Extracted from `App::handle_command` (a single ~640-line match) so each
//! match arm can live as an independently readable async function.
//!
//! Per-arm handlers take `&mut App` and call into App's already-`pub(crate)`
//! component fields and methods (`session_mgr`, `harness_registry`,
//! `state_persistor`, `wake`, etc.). The router itself is a free-function
//! module — there is no `Router` type — because the call graph fans into
//! ~all of App's interior, and a borrow-bundle struct large enough to
//! capture every field would be more error-prone than direct method
//! calls on `&mut App`.
//!
//! ## Why pass-through `&mut App` rather than a borrow bundle
//!
//! The earlier extractions (`StatePersistor`, `BannerCoordinator`,
//! `HarnessRegistry`, `WakeCoordinator`) used a `*DispatchContext<'a>`
//! struct with 5–9 borrowed fields. The command router would need 17+
//! fields (every component plus chat platforms plus structured-output
//! handles plus broadcast senders plus config primitives). Constructing
//! such a struct via field-level disjoint borrows works, but the
//! ergonomic and review cost — at every call site, in every per-arm
//! handler — exceeds the encapsulation benefit.
//!
//! Pragmatic deviation: the router operates on `&mut App` directly. The
//! goal of the extraction (mechanical reduction of the largest method,
//! independent readability of each arm, isolated testing) is preserved.

use crate::app::{numeric_name_hint, App};
use crate::buffer::{OutputBuffer, StreamEvent};
use crate::chat_adapters::{IncomingMessage, PlatformType};
use crate::command::{HarnessOptions, HarnessSubcommandKind, ParsedCommand};
use crate::delivery::split_message;
use crate::harness::{drive_harness, HarnessContext, HarnessKind};
use crate::harness_registry::PromptDispatchContext;
use crate::session::SessionStatus;
use crate::socket::events::AmbientEvent;
use crate::state_store::StateUpdate;

/// Drive a single inbound message through the command pipeline:
///
/// 1. First-interaction `MarkDirty` guard
/// 2. `ChatForward` ambient event for chat-origin messages
/// 3. Per-platform chat-binding (`Bind*Chat` `StateUpdate`)
/// 4. `ParsedCommand::parse` with chat-safe error reply on failure
/// 5. Multi-line `StdinInput` guard (requires harness mode)
/// 6. Foreground-session bind broadcast (so delivery tasks know the chat)
/// 7. Per-arm dispatch
pub async fn route_command(app: &mut App, msg: &IncomingMessage) {
    // 1. Ensure MarkDirty is sent on the first real user interaction.
    if app.state_persistor.claim_dirty_send() {
        if let Err(e) = app.state_tx.try_send(StateUpdate::MarkDirty) {
            tracing::warn!("state_tx full, update dropped: {}", e);
        }
    }

    // 2. Emit ChatForward ambient event for chat-origin messages (not socket).
    if msg.socket_request_id.is_none() {
        app.emit_ambient(AmbientEvent::ChatForward {
            platform: format!("{:?}", msg.platform),
            user_id: msg.user_id.clone(),
            text: msg.text.clone(),
        });
    }

    // 3. Skip chat binding for socket-origin messages — they use
    //    PlatformType::Telegram as a wire-compat placeholder but should NOT
    //    pollute the wake coordinator's active-chat sets or trigger gap
    //    banners.
    if msg.reply_context.socket_reply_tx.is_none() {
        bind_chat_for_message(app, msg);
    }

    let cmd = match ParsedCommand::parse(&msg.text, app.trigger) {
        Ok(cmd) => cmd,
        Err(e) => {
            app.send_error(&msg.reply_context, &format!("{:#}", e))
                .await;
            return;
        }
    };

    // 5. Multi-line StdinInput guard: the parser allows multi-line StdinInput,
    //    but the dispatcher must check harness state before routing to tmux.
    if let ParsedCommand::StdinInput { ref text } = cmd {
        if (text.contains('\n') || text.contains('\r'))
            && app.session_mgr.foreground_harness().is_none()
        {
            app.send_error(
                &msg.reply_context,
                &format!(
                    "Multi-line input requires harness mode. \
                     Use `{} claude on` to enable, or prefix with `{} claude <prompt>` for one-off.",
                    app.trigger, app.trigger
                ),
            )
            .await;
            return;
        }
    }

    tracing::info!(
        "handle_command: {:?} (fg={:?})",
        cmd,
        app.session_mgr.foreground_session()
    );

    // 6. Always bind the foreground session to the current chat context.
    //    This ensures delivery tasks know where to send output — critical
    //    after restart (reconnected sessions have no chat binding) and for
    //    cross-platform use.
    if let Some(fg) = app.session_mgr.foreground_session() {
        let _ = app.stream_tx.send(StreamEvent::SessionStarted {
            session: fg.to_string(),
            chat_id: msg.reply_context.chat_id.clone(),
            thread_ts: msg.reply_context.thread_ts.clone(),
        });
    }

    // 7. Per-arm dispatch.
    match cmd {
        ParsedCommand::ShellCommand { ref cmd } if app.blocklist.is_blocked(cmd) => {
            app.send_error(&msg.reply_context, "Command blocked by security policy")
                .await;
        }
        ParsedCommand::NewSession { name } => handle_new_session(app, name, msg).await,
        ParsedCommand::Foreground { name } => handle_foreground(app, name, msg).await,
        ParsedCommand::Background => handle_background(app, msg).await,
        ParsedCommand::ListSessions => handle_list_sessions(app, msg).await,
        ParsedCommand::Screen => handle_screen(app, msg).await,
        ParsedCommand::KillSession { name } => handle_kill_session(app, name, msg).await,
        ParsedCommand::HarnessOn {
            harness: kind,
            options,
            initial_prompt,
        } => handle_harness_on(app, kind, options, initial_prompt, msg).await,
        ParsedCommand::HarnessOff { harness: kind } => handle_harness_off(app, kind, msg).await,
        ParsedCommand::HarnessPrompt {
            harness: kind,
            ref prompt,
            ref options,
        } => handle_harness_prompt(app, kind, prompt, options, msg).await,
        ParsedCommand::HarnessSubcommand {
            harness,
            subcommand,
            args,
        } => handle_harness_subcommand(app, harness, subcommand, args, msg).await,
        ParsedCommand::ShellCommand { cmd } => handle_shell_command(app, cmd, msg).await,
        ParsedCommand::StdinInput { text } => handle_stdin_input(app, text, msg).await,
    }
}

// ── Chat binding ─────────────────────────────────────────────────────────────

/// Bind chat IDs for the active platform so the wake coordinator knows
/// which chats to banner. Each `wake.insert_*_chat` returns true iff the
/// chat was newly added — only then do we send the corresponding
/// `StateUpdate` so already-bound chats don't spam the channel.
fn bind_chat_for_message(app: &mut App, msg: &IncomingMessage) {
    match msg.reply_context.platform {
        PlatformType::Telegram => {
            if let Ok(id) = msg.reply_context.chat_id.parse::<i64>() {
                if app.wake.insert_telegram_chat(id) {
                    if let Err(e) = app.state_tx.try_send(StateUpdate::BindTelegramChat(id)) {
                        tracing::warn!("state_tx full, update dropped: {}", e);
                    }
                }
            }
        }
        PlatformType::Slack => {
            let id = msg.reply_context.chat_id.clone();
            if app.wake.insert_slack_chat(id.clone()) {
                if let Err(e) = app.state_tx.try_send(StateUpdate::BindSlackChat(id)) {
                    tracing::warn!("state_tx full, update dropped: {}", e);
                }
            }
        }
        PlatformType::Discord => {
            let id = msg.reply_context.chat_id.clone();
            if app.wake.insert_discord_chat(id.clone()) {
                if let Err(e) = app.state_tx.try_send(StateUpdate::BindDiscordChat(id)) {
                    tracing::warn!("state_tx full, update dropped: {}", e);
                }
            }
        }
    }
}

// ── Per-arm handlers ────────────────────────────────────────────────────────

async fn handle_new_session(app: &mut App, name: String, msg: &IncomingMessage) {
    match app.session_mgr.new_session(&name).await {
        Ok(()) => {
            let mut buf = OutputBuffer::new(&name, app.offline_buffer_max);
            buf.sync_offset(app.session_mgr.tmux()).await;
            app.buffers.insert(name.clone(), buf);
            let _ = app.stream_tx.send(StreamEvent::SessionStarted {
                session: name.clone(),
                chat_id: msg.reply_context.chat_id.clone(),
                thread_ts: msg.reply_context.thread_ts.clone(),
            });
            app.emit_ambient(AmbientEvent::SessionCreated {
                session: name.clone(),
                origin_chat: Some(msg.reply_context.chat_id.clone()),
                created_at: chrono::Utc::now(),
            });
            app.send_reply(&msg.reply_context, &format!("Session '{}' created", name))
                .await;
        }
        Err(e) => {
            // Check if the error is a session-limit error and emit ambient.
            let err_str = format!("{:#}", e);
            if err_str.contains("Maximum session limit") {
                app.emit_ambient(AmbientEvent::SessionLimitReached {
                    attempted: name.clone(),
                    current: app.session_mgr.session_count(),
                    max: app.session_mgr.max_sessions(),
                });
            }
            app.send_error(&msg.reply_context, &err_str).await;
        }
    }
}

async fn handle_foreground(app: &mut App, name: String, msg: &IncomingMessage) {
    match app.session_mgr.fg(&name) {
        Ok(()) => {
            let _ = app.stream_tx.send(StreamEvent::SessionStarted {
                session: name.clone(),
                chat_id: msg.reply_context.chat_id.clone(),
                thread_ts: msg.reply_context.thread_ts.clone(),
            });
            app.send_reply(
                &msg.reply_context,
                &format!("Session '{}' foregrounded", name),
            )
            .await;
        }
        Err(e) => {
            app.send_error(&msg.reply_context, &format!("{:#}", e))
                .await;
        }
    }
}

async fn handle_background(app: &mut App, msg: &IncomingMessage) {
    match app.session_mgr.bg() {
        Ok(Some(name)) => {
            app.send_reply(
                &msg.reply_context,
                &format!("Session '{}' backgrounded", name),
            )
            .await;
        }
        Ok(None) => {
            app.send_error(&msg.reply_context, "No foreground session to background")
                .await;
        }
        Err(e) => {
            app.send_error(&msg.reply_context, &format!("{:#}", e))
                .await;
        }
    }
}

async fn handle_list_sessions(app: &mut App, msg: &IncomingMessage) {
    let sessions = app.session_mgr.list();
    if sessions.is_empty() {
        app.send_reply(&msg.reply_context, "No active sessions")
            .await;
    } else {
        let mut lines = Vec::new();
        for (name, status, created) in &sessions {
            let status_str = match status {
                SessionStatus::Foreground => "[foreground]",
                SessionStatus::Background => "[background]",
            };
            let elapsed = created.elapsed();
            lines.push(format!(
                "  {} {} (uptime: {}s)",
                name,
                status_str,
                elapsed.as_secs()
            ));
        }
        app.send_reply(&msg.reply_context, &lines.join("\n")).await;
    }
}

async fn handle_screen(app: &mut App, msg: &IncomingMessage) {
    match app.session_mgr.foreground_session() {
        Some(fg) => {
            let fg_owned = fg.to_string();
            match app.session_mgr.tmux().capture_pane(&fg_owned).await {
                Ok(screen) => {
                    let trimmed = screen
                        .lines()
                        .map(|l| l.trim_end())
                        .collect::<Vec<_>>()
                        .join("\n")
                        .trim()
                        .to_string();
                    if trimmed.is_empty() {
                        app.send_reply(&msg.reply_context, "(empty screen)").await;
                    } else {
                        for chunk in split_message(&trimmed, 4000) {
                            app.send_reply(&msg.reply_context, &chunk).await;
                        }
                    }
                }
                Err(e) => {
                    app.send_error(&msg.reply_context, &format!("{:#}", e))
                        .await;
                }
            }
        }
        None => {
            app.send_error(&msg.reply_context, "No active session")
                .await;
        }
    }
}

async fn handle_kill_session(app: &mut App, name: String, msg: &IncomingMessage) {
    match app.session_mgr.kill(&name).await {
        Ok(()) => {
            app.buffers.remove(&name);
            app.emit_ambient(AmbientEvent::SessionKilled {
                session: name.clone(),
                reason: "user_request".to_string(),
                killed_at: chrono::Utc::now(),
            });
            app.send_reply(&msg.reply_context, &format!("Session '{}' killed", name))
                .await;
        }
        Err(e) => {
            app.send_error(&msg.reply_context, &format!("{:#}", e))
                .await;
        }
    }
}

/// Handle `: <kind> on [--name|--resume <id>] [<initial_prompt>]`.
///
/// Phases:
/// 1. **Registration & session guards** — verify the harness kind is
///    registered (stubs aren't) and a foreground session exists.
/// 2. **Resume eligibility check** — `reject_if_not_resumable` gates
///    `--name`/`--resume` flags against the harness's `supports_resume`.
///    The dispatch context is built in a scoped block so its borrows
///    end before the subsequent `&mut session_mgr.set_harness` calls.
/// 3. **Named-session resolution** — `--resume` does a strict lookup;
///    `--name` does create-or-resume (upsert) with a `numeric_name_hint`
///    on creation only.
/// 4. **Switch / re-activate** — same-kind re-activation just updates
///    options (and notifies on session-name changes); cross-kind switch
///    requires the current harness to support resume so we don't lose
///    its context.
/// 5. **Activate + initial prompt** — emit the `... mode ON` message
///    and, if an `initial_prompt` was provided, fire it via
///    `app.send_harness_prompt`.
async fn handle_harness_on(
    app: &mut App,
    kind: HarnessKind,
    options: HarnessOptions,
    initial_prompt: Option<String>,
    msg: &IncomingMessage,
) {
    // Verify the harness is actually registered (stubs are not).
    if !app.harness_registry.contains_kind(&kind) {
        app.send_error(
            &msg.reply_context,
            &format!("{} harness is not available", kind.name()),
        )
        .await;
        return;
    }
    let fg = match app.session_mgr.foreground_session() {
        Some(fg) => fg.to_string(),
        None => {
            app.send_error(&msg.reply_context, "No active session")
                .await;
            return;
        }
    };
    // Validate --name/--resume on non-resumable harnesses. Dispatch context
    // built inline so its field-level borrows end after the reject call,
    // freeing `&mut app.session_mgr` for the `set_harness` calls below.
    {
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
        if app
            .harness_registry
            .reject_if_not_resumable(&msg.reply_context, &kind, &options, &dispatch)
            .await
        {
            return;
        }
    }

    // Resolve named session for --name/--resume. We do NOT pre-create an
    // empty-session_id entry here — persistence happens only after the
    // first prompt returns a real session_id. This keeps the state file
    // free of "zombie" entries when the user runs `claude on --name foo`
    // then exits without prompting, and avoids the "exists but has no
    // conversation yet" dead-end on a subsequent `--resume foo`.
    let mut named_notification: Option<String> = None;
    if let Some(ref resume_name) = options.resume {
        match app.harness_registry.lookup_named_session(kind, resume_name) {
            Some(entry) if !entry.session_id.is_empty() => {
                named_notification = Some(format!("Resuming session '{}'", resume_name));
            }
            _ => {
                app.send_error(
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
        let is_resumable = app
            .harness_registry
            .lookup_named_session(kind, name)
            .is_some_and(|e| !e.session_id.is_empty());
        let primary = if is_resumable {
            format!("Resuming existing session '{}'", name)
        } else {
            format!("Created new session '{}'", name)
        };
        // Only hint on creation, not on resume — if the user is resuming a
        // numeric name, they clearly know it exists.
        let hint = (!is_resumable).then(|| numeric_name_hint(name)).flatten();
        named_notification = Some(match hint {
            Some(h) => format!("{}\n{}", primary, h),
            None => primary,
        });
    }

    // Check if switching from another harness or updating options.
    if let Some(current) = app.session_mgr.foreground_harness() {
        if current == kind {
            // Same harness re-activated — check if named session changed.
            let new_session_name = options.name.as_deref().or(options.resume.as_deref());
            let prev_resolved = app
                .session_mgr
                .foreground_named_session_resolved()
                .map(|s| s.to_string());

            app.session_mgr
                .set_harness(&fg, Some(kind), options.clone());

            // Update named_session_resolved if session name changed.
            if new_session_name.map(|s| s.to_string()) != prev_resolved {
                app.session_mgr
                    .set_named_session_resolved(new_session_name.map(|s| s.to_string()));
                if let Some(note) = named_notification {
                    app.send_reply(&msg.reply_context, &note).await;
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
            app.send_reply(&msg.reply_context, &opts_msg).await;
            // Fire the initial prompt, if provided (e.g.
            // `: claude on --resume review please look at the bag`).
            if let Some(ref prompt) = initial_prompt {
                app.send_harness_prompt(
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
        // Check resume support of the CURRENT harness before switching away.
        if let Some(h) = app.harness_registry.harness_for(&current) {
            if !h.supports_resume() {
                app.send_error(
                    &msg.reply_context,
                    &format!(
                        "{} is active but doesn't support resume. Run `{} {} off` first to avoid losing context.",
                        current.name(),
                        app.trigger,
                        current.name().to_lowercase()
                    ),
                )
                .await;
                return;
            }
        }
        app.send_reply(
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
    // Send named session notification before the ON message.
    if let Some(note) = named_notification {
        app.send_reply(&msg.reply_context, &note).await;
    }

    let opts_summary = if options.is_empty() {
        String::new()
    } else {
        format!("\nOptions: {}", options.summary())
    };
    // Track the resolved named session name for notification suppression.
    let resolved_name = options
        .name
        .as_deref()
        .or(options.resume.as_deref())
        .map(|s| s.to_string());
    // Keep a copy for the optional initial prompt, since `set_harness`
    // moves `options` into session state.
    let options_for_prompt = initial_prompt.as_ref().map(|_| options.clone());
    app.session_mgr.set_harness(&fg, Some(kind), options);
    app.session_mgr.set_named_session_resolved(resolved_name);
    app.send_reply(
        &msg.reply_context,
        &format!(
            "{} mode ON. Plain text now goes to {}. Use `{} {} off` to switch back.{}",
            kind.name(),
            kind.name(),
            app.trigger,
            kind.name().to_lowercase(),
            opts_summary
        ),
    )
    .await;
    // Fire the initial prompt, if provided.
    if let (Some(prompt), Some(opts)) = (initial_prompt, options_for_prompt) {
        app.send_harness_prompt(&msg.reply_context, &kind, &prompt, &msg.attachments, &opts)
            .await;
    }
}

async fn handle_harness_off(app: &mut App, kind: HarnessKind, msg: &IncomingMessage) {
    let fg = match app.session_mgr.foreground_session() {
        Some(fg) => fg.to_string(),
        None => {
            app.send_error(&msg.reply_context, "No active session")
                .await;
            return;
        }
    };
    // Verify the specified harness is actually the active one.
    let current = app.session_mgr.foreground_harness();
    if current != Some(kind) {
        app.send_error(
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
    app.session_mgr
        .set_harness(&fg, None, HarnessOptions::default());
    app.send_reply(
        &msg.reply_context,
        &format!(
            "{} mode OFF. Plain text now goes to the terminal session.",
            kind.name()
        ),
    )
    .await;
}

async fn handle_harness_prompt(
    app: &mut App,
    kind: HarnessKind,
    prompt: &str,
    options: &HarnessOptions,
    msg: &IncomingMessage,
) {
    if app.blocklist.is_blocked(prompt) {
        app.send_error(
            &msg.reply_context,
            &format!("{} prompt blocked by security policy", kind.name()),
        )
        .await;
    } else {
        // One-shot prompt (`: claude --schema=foo <prompt>`): use parsed options.
        app.send_harness_prompt(&msg.reply_context, &kind, prompt, &msg.attachments, options)
            .await;
    }
}

async fn handle_harness_subcommand(
    app: &mut App,
    harness: HarnessKind,
    subcommand: HarnessSubcommandKind,
    args: Vec<String>,
    msg: &IncomingMessage,
) {
    let hctx = HarnessContext {
        ctx: &msg.reply_context,
        telegram: app.telegram.as_deref(),
        slack: app.slack.as_deref(),
        discord: app.discord.as_deref(),
        schema_registry: &app.schema_registry,
        delivery_queue: &app.delivery_queue,
        webhook_client: &app.webhook_client,
        stream_tx: &app.stream_tx,
    };
    // Route to the per-harness subcommand runner. All runners emit the
    // same Text+Done / Error+Done shape that `drive_harness` consumes —
    // subcommands don't create sessions and don't fire ambient
    // HarnessStarted/Finished events.
    let setup = match (harness, subcommand) {
        (HarnessKind::Opencode, HarnessSubcommandKind::Opencode(sub)) => {
            app.harness_registry
                .opencode()
                .run_subcommand(sub, args)
                .await
        }
        (HarnessKind::Gemini, HarnessSubcommandKind::Gemini(sub)) => {
            app.harness_registry
                .gemini()
                .run_subcommand(sub, args)
                .await
        }
        (HarnessKind::Codex, HarnessSubcommandKind::Codex(sub)) => {
            app.harness_registry.codex().run_subcommand(sub, args).await
        }
        (h, _) => {
            app.send_error(
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
            app.send_error(
                &msg.reply_context,
                &format!("{} subcommand setup failed: {}", harness.name(), e),
            )
            .await;
        }
    }
}

async fn handle_shell_command(app: &mut App, cmd: String, msg: &IncomingMessage) {
    // Snapshot pane before command so we can diff afterward.
    match app.session_mgr.foreground_session() {
        None => {
            tracing::warn!("ShellCommand '{}': no foreground session", cmd);
        }
        Some(fg) => {
            let fg_owned = fg.to_string();
            if let Some(buf) = app.buffers.get_mut(&fg_owned) {
                buf.snapshot_before_command(app.session_mgr.tmux(), Some(&cmd))
                    .await;
                tracing::debug!("ShellCommand '{}': snapshot taken", cmd);
            } else {
                tracing::warn!(
                    "ShellCommand '{}': no buffer for session '{}', output won't be captured",
                    cmd,
                    fg_owned
                );
            }
        }
    }
    if let Err(e) = app.session_mgr.execute_in_foreground(&cmd).await {
        app.send_error(&msg.reply_context, &format!("{:#}", e))
            .await;
    }
}

async fn handle_stdin_input(app: &mut App, text: String, msg: &IncomingMessage) {
    // Single blocklist check regardless of routing.
    if app.blocklist.is_blocked(&text) {
        app.send_error(&msg.reply_context, "Command blocked by security policy")
            .await;
        return;
    }
    if let Some(kind) = app.session_mgr.foreground_harness() {
        // Route to active harness with stored session options.
        let opts = app.session_mgr.foreground_harness_options();
        app.send_harness_prompt(&msg.reply_context, &kind, &text, &msg.attachments, &opts)
            .await;
    } else {
        // Images are only supported in harness mode.
        if !msg.attachments.is_empty() {
            app.send_error(
                &msg.reply_context,
                &format!(
                    "Images are only supported in harness mode. Use `{} claude on` first.",
                    app.trigger
                ),
            )
            .await;
            return;
        }
        // Route to terminal.
        if let Some(fg) = app.session_mgr.foreground_session() {
            let fg_owned = fg.to_string();
            if let Some(buf) = app.buffers.get_mut(&fg_owned) {
                buf.snapshot_before_command(app.session_mgr.tmux(), Some(&text))
                    .await;
            }
        }
        if let Err(e) = app.session_mgr.send_stdin_to_foreground(&text).await {
            app.send_error(&msg.reply_context, &format!("{:#}", e))
                .await;
        }
    }
}
