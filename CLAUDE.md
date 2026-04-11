# termbot

Single-user Rust bot that controls tmux terminal sessions from Telegram and Slack. Built on tokio async runtime with `tokio::select!` main loop.

## Quick reference

```bash
cargo build                  # build
cargo test                   # run unit tests (18 tests in command + buffer modules)
cargo run                    # requires termbot.toml (copy termbot.example.toml)
```

Config file `termbot.toml` contains API secrets and is gitignored. Never commit it.

## Architecture

Single crate, flat modules. No workspace members.

```
src/
  main.rs           tokio::select! event loop, command dispatch, delivery tasks
  config.rs         TOML config (serde Deserialize)
  command.rs        `: ` prefix command parser + regex blocklist
  session.rs        Foreground/background session state machine
  tmux.rs           tmux CLI wrapper (create, kill, send-keys, capture-pane)
  buffer.rs         Scrollback-offset output capture + streaming
  claude.rs         Claude Code SDK integration (claude-agent-sdk-rust crate)
  platform/
    mod.rs          ChatPlatform trait (async_trait)
    telegram.rs     Telegram adapter (teloxide, long-polling)
    slack.rs        Slack adapter (Socket Mode via tokio-tungstenite)
```

### Core loop (main.rs)

Four `tokio::select!` branches:
1. **cmd_rx** -- incoming messages from platform adapters (mpsc channel)
2. **health_interval** -- 5s timer, detects crashed tmux sessions
3. **poll_interval** -- 250ms timer, reads new output via capture-pane scrollback
4. **ctrl_c** -- graceful shutdown (stops pipe-pane, cleans temp files, leaves tmux sessions alive)

Output events flow through `broadcast::channel<StreamEvent>` to per-platform delivery tasks.

### Output capture

Uses `tmux capture-pane -S -` (full scrollback) with line-offset tracking. Each poll diffs meaningful (non-empty) line count against `lines_seen`. The `is_noise_line` filter strips prompts, command echoes, starship segments, and shell errors.

**Do not use pipe-pane for output capture.** The codebase previously considered it but settled on capture-pane with scrollback offsets. The `output_file_path` and `cleanup_output_file` methods in `TmuxClient` are legacy stubs.

### Claude Code integration

Uses `claude-agent-sdk-rust` crate, not terminal scraping. Claude prompts spawn via `tokio::spawn` with an mpsc event channel. Multi-turn sessions are tracked by session name -> Claude session ID in `ClaudeManager`. Two modes:
- One-shot: `: claude <prompt>`
- Interactive: `: claude on` / `: claude off` toggles plain text routing

### Platform adapters

Both implement `ChatPlatform` (async_trait). Auth is single-user: messages from non-authorized user IDs are silently dropped. Telegram uses manual `getUpdates` long-polling (not webhooks). Slack uses Socket Mode WebSocket with auto-reconnect.

## Conventions

- Error handling: `anyhow::Result` everywhere, `thiserror` for `ParseError` in command.rs
- Logging: `tracing` macros (`info!`, `warn!`, `error!`, `debug!`), initialized with `tracing_subscriber::fmt`
- Async: all tmux operations are async (spawns `tmux` CLI as child process via `tokio::process::Command`)
- Sessions are prefixed `tb-` in tmux (e.g. `tb-build`). Always target by name, never by window/pane index
- Smart quotes from mobile keyboards are normalized to ASCII in `tmux.rs::normalize_quotes`
- Tests live in `#[cfg(test)] mod tests` within each module, not in a separate `tests/` dir

## Key constraints

- **Single-user only.** Auth checks are per-platform (telegram_user_id, slack_user_id in config).
- **tmux must be on PATH.** All session operations shell out to `tmux`.
- **No shared mutable state.** The main loop owns all mutable state directly; platform adapters only send/receive through channels.
- **Rate limiting is per-platform.** `edit_throttle_ms` config controls minimum gap between message edits to stay within Telegram/Slack API limits.
- **Startup reconciliation.** On restart, surviving `tb-*` tmux sessions are auto-reconnected. Chat binding is re-established on first user message (chat IDs are not persisted to disk).

## Testing

Unit tests cover command parsing and blocklist (command.rs) and output buffer line counting / noise filtering (buffer.rs). No integration tests yet -- tmux operations require a live tmux server.

Run with: `cargo test`
