# terminus

Single-user Rust bot that controls tmux terminal sessions from Telegram, Slack, and Discord. Built on tokio async runtime with `tokio::select!` main loop.

## Quick reference

```bash
cargo build                  # build
cargo test                   # run unit tests (18 tests in command + buffer modules)
cargo run                    # requires terminus.toml (copy terminus.example.toml)
```

Config file `terminus.toml` contains API secrets and is gitignored. Never commit it.

## Architecture

Single crate, flat modules. No workspace members.

```
src/
  main.rs           tokio::select! event loop, command dispatch
  app.rs            App state, handle_gap, mark_clean_shutdown, StateStore owner
  delivery.rs       Per-platform delivery task, StreamEvent handling, gap-banner
                    rendering, inline-prefix fallback; resolves
                    PendingBannerAcks oneshots directly (no main-loop round-trip)
  config.rs         TOML config (serde Deserialize)
  command.rs        `: ` prefix command parser + regex blocklist
  session.rs        Foreground/background session state machine
  tmux.rs           tmux CLI wrapper (create, kill, send-keys, capture-pane)
  buffer.rs         Scrollback-offset output capture + streaming
  state_store.rs    Atomic JSON state file (Telegram offset, chat bindings, wake
                    watermarks) owned exclusively by App; adapters send updates
                    via mpsc::Sender<StateUpdate>
  claude.rs         Claude Code SDK integration (claude-agent-sdk-rust crate)
  chat_adapters/
    mod.rs          ChatPlatform trait (async_trait), IncomingMessage,
                    ReplyContext (with optional socket_reply_tx for socket-
                    origin routing), PlatformType (Telegram/Slack/Discord —
                    NOT modified for socket; socket uses socket_reply_tx)
    telegram.rs     Telegram adapter (teloxide, long-polling, level-triggered
                    watch-channel pause/resume for banner ordering)
    slack.rs        Slack adapter (Socket Mode via tokio-tungstenite)
    discord.rs      Discord adapter (serenity gateway, handler-gate pause/resume)
  socket/
    mod.rs          SocketServer: TcpListener, Bearer token auth at HTTP
                    upgrade, per-connection task spawn, connection-limit
                    enforcement
    connection.rs   Per-connection tokio::select! loop: pending-request FIFO,
                    subscription dispatch, rate limiting, ping/pong, idle
                    timeout, graceful shutdown
    envelope.rs     InboundEnvelope/OutboundEnvelope serde types (terminus/v1
                    wire protocol), ErrorCode, Filter, OutboundFrame
    events.rs       AmbientEvent (7 genuinely-new event types for socket
                    subscription bus), AmbientEventType discriminator
    rate_limit.rs   Per-connection TokenBucket (capacity + refill)
    subscription.rs SubscriptionRegistry with matches_ambient/matches_stream
                    (facet filter: event_types/schemas/sessions, OR within
                    facets, AND across facets)
  power/
    mod.rs          PowerManager trait (async_trait) + cfg-gated submod picks
    types.rs        LidState, PowerSource, PowerEvent, PowerSignal
    policy.rs       Pure desired_inhibit(lid, power, stayawake_on_battery) fn
    gap_detector.rs 1s ticker comparing SystemTime/Instant; emits
                    PowerSignal::GapDetected when divergence > 30s
    supervisor.rs   Polls lid/power, applies policy, calls set_inhibit on
                    transitions (broadcasts PowerEvent for observability)
    macos.rs        #[cfg(target_os = "macos")] — caffeinate -i child + ioreg /
                    pmset -g batt reads (hybrid per ADR)
    linux.rs        #[cfg(target_os = "linux")] — systemd-inhibit child +
                    /proc/acpi/button/lid/* + /sys/class/power_supply/AC* reads
    fake.rs         FakePowerManager test double (OS-agnostic)
```

### Core loop (main.rs)

`tokio::select!` branches in biased priority order:
1. **power_rx** -- gap-detector `PowerSignal::GapDetected` (biased **above** cmd_rx
   so the gap banner is dispatched before any queued platform message races it)
2. **state_rx** -- `StateUpdate` mpsc from adapters and App internals; debounced
   persist (≥10 updates OR ≥5s)
3. **cmd_rx** -- incoming messages from platform adapters
4. **health_interval** -- 5s timer, crashed-session detection + `StateUpdate::Tick`
5. **poll_interval** -- 250ms timer, capture-pane scrollback read
6. **ctrl_c** -- graceful shutdown; calls `App::mark_clean_shutdown()` to flip
   `last_clean_shutdown=true` before cleanup so restarts don't fire false banners

**Banner-ack ordering** (no main-loop round-trip, no deadlock): `handle_gap`
inserts per-chat `oneshot::Sender` handles into the shared
`PendingBannerAcks` (`Arc<tokio::sync::Mutex<HashMap<...>>>`), broadcasts
`StreamEvent::GapBanner`, then awaits the receivers with a 5s timeout. The
delivery task (in `src/delivery.rs`) resolves each oneshot directly on
successful `send_message`. On timeout, the fallback engages: a `GapInfo` is
inserted into the shared `GapPrefixes` map, and the delivery task prepends
`[gap: Xm Ys] ` to the next outbound `NewMessage` for that chat.

Output events flow through `broadcast::channel<StreamEvent>` (capacity 256) to
per-platform delivery tasks. `StreamEvent::GapBanner` is rendered by the
delivery task and acked back via `DeliveryAck::BannerSent`.

### Sleep/wake behavior

Cross-platform (macOS + Linux) per the consensus plan:
- **Idle sleep is prevented** while the lid is open (or the device has no lid)
  and the host is on AC. Set `power.stayawake_on_battery = true` in
  `terminus.toml` to prevent idle sleep on battery too.
- **Closed-lid sleep is never blocked** — this is intentional. macOS clamshell
  and Linux `systemd-inhibit --what=idle:sleep` are both idle-sleep-only.
- **Wake recovery**: a 1s ticker compares `SystemTime` (wall) vs `Instant`
  (monotonic). A >30s divergence signals that the host slept. On wake, the
  Telegram polling loop is paused via a `tokio::sync::watch::channel<bool>`
  (level-triggered — required so a busy poll doesn't miss the signal), a
  `⏸ paused at HH:MM, resumed at HH:MM (gap: Xm Ys), processing N queued
  messages` banner is broadcast per active chat, the banner-sent ack is
  awaited (5s timeout with inline-prefix fallback), then polling resumes.
- **Restart durability**: Telegram offset and chat IDs are persisted to
  `<terminus.toml parent>/terminus-state.json` (or the `power.state_file`
  override). The restart-banner gate requires `last_clean_shutdown == false`
  AND `wall_gap > 30s` so graceful restarts don't trigger spurious banners.
- **Discord pause** uses a handler-gate rather than a poll-gate. Gateway events
  arriving during the pause window are discarded, unlike Telegram's server-side
  update list which queues messages for drain on resume. The pause window is
  bounded by gap-banner dispatch timing (typically <5s) and the `: ` command
  protocol is self-recoverable, so this is an accepted v1 tradeoff.
- Verify the assertion is held with `pmset -g assertions` (macOS) or
  `systemd-inhibit --list` (Linux).

### Output capture

Uses `tmux capture-pane -S -` (full scrollback) with line-offset tracking. Each poll diffs meaningful (non-empty) line count against `lines_seen`. The `is_noise_line` filter strips prompts, command echoes, starship segments, and shell errors.

**Do not use pipe-pane for output capture.** The codebase previously considered it but settled on capture-pane with scrollback offsets. The `output_file_path` and `cleanup_output_file` methods in `TmuxClient` are legacy stubs.

### Claude Code integration

Uses `claude-agent-sdk-rust` crate, not terminal scraping. Claude prompts spawn via `tokio::spawn` with an mpsc event channel. Multi-turn sessions are tracked by session name -> Claude session ID in `ClaudeManager`. Two modes:
- One-shot: `: claude <prompt>`
- Interactive: `: claude on` / `: claude off` toggles plain text routing

### Platform adapters

All three implement `ChatPlatform` (async_trait). Auth is single-user: messages from non-authorized user IDs are silently dropped. Telegram uses manual `getUpdates` long-polling (not webhooks). Slack uses Socket Mode WebSocket with auto-reconnect. Discord uses the serenity crate with gateway intents `DIRECT_MESSAGES | GUILD_MESSAGES | MESSAGE_CONTENT` (privileged). Inbound Discord attachments (images/files sent by the user to the bot) are not currently processed -- only `msg.content` text is forwarded to the main loop. Telegram-parity for inbound attachments is a follow-up.

### WebSocket bidirectional API (`src/socket/`)

Optional, opt-in (`[socket] enabled = true`). Exposes an authenticated WebSocket
endpoint for local programs and remote agents. Transport is plain `ws://`; deploy
behind a reverse proxy for `wss://` TLS termination.

**Architecture:** Per-connection task model. `SocketServer` binds a `TcpListener`,
authenticates via `Authorization: Bearer <token>` at the HTTP upgrade (tokens from
`[[socket.client]]` entries in config), and spawns a per-connection task.

**Integration seams (no structural changes to App/session/harness):**
- Inbound: `mpsc::Sender<IncomingMessage>` (same channel as chat adapters). Socket
  sets `socket_reply_tx` on `ReplyContext` so `App::send_reply` routes responses
  back to the socket instead of to a chat platform.
- Per-request output: `broadcast::channel<StreamEvent>` (existing). The connection
  task subscribes and translates matching events for subscribed clients.
- Ambient events: `broadcast::channel<AmbientEvent>` (new, capacity 512). 7 new
  event types emitted from `App` methods: `SessionCreated`, `SessionKilled`,
  `SessionLimitReached`, `ChatForward`, `HarnessStarted`, `HarnessFinished`,
  `SessionOutput` (translated from `StreamEvent::NewMessage` at socket layer).
- `PlatformType` is NOT modified (preserves golden-tested queue-file wire format).

**Wire protocol:** `terminus/v1`. One JSON envelope per WebSocket text message.
Client-supplied `request_id` for correlation. Per-request lifecycle:
`request → ack → [text_chunk | tool_call | partial_result]* → result | error → end`.
Subscription: `subscribe → subscribed → event*`, filtered by facets
(`event_types` / `schemas` / `sessions`), OR within facets, AND across.

**Rate limiting:** Per-connection token bucket (burst + refill). Max message size,
idle timeout, ping/pong, configurable in `[socket]` table.

**Shutdown ordering:** `main.rs` ctrl_c branch cancels `socket_cancel` token first,
then calls `app.mark_clean_shutdown()`. Socket connections receive `shutting_down`
envelope and drain pending requests within `shutdown_drain_secs`.

## Conventions

- Error handling: `anyhow::Result` everywhere, `thiserror` for `ParseError` in command.rs
- Logging: `tracing` macros (`info!`, `warn!`, `error!`, `debug!`), initialized with `tracing_subscriber::fmt`
- Async: all tmux operations are async (spawns `tmux` CLI as child process via `tokio::process::Command`)
- Sessions are prefixed `term-` in tmux (e.g. `term-build`). Always target by name, never by window/pane index
- Smart quotes from mobile keyboards are normalized to ASCII in `tmux.rs::normalize_quotes`
- Tests live in `#[cfg(test)] mod tests` within each module, not in a separate `tests/` dir

## Key constraints

- **Single-user only.** Auth checks are per-platform (telegram_user_id, slack_user_id, discord_user_id in config).
- **tmux must be on PATH.** All session operations shell out to `tmux`.
- **No shared mutable state.** The main loop owns all mutable state directly; platform adapters only send/receive through channels.
- **Rate limiting is per-platform.** `edit_throttle_ms` config controls minimum gap between message edits to stay within Telegram/Slack API limits.
- **Startup reconciliation.** On restart, surviving `term-*` tmux sessions are auto-reconnected. Chat binding is re-established on first user message (chat IDs are not persisted to disk).

## Testing

Unit tests cover command parsing and blocklist (command.rs) and output buffer line counting / noise filtering (buffer.rs). No integration tests yet -- tmux operations require a live tmux server.

Run with: `cargo test`
