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
  harness/
    mod.rs          Harness trait (async_trait), HarnessEvent, HarnessKind,
                    HarnessOptions; shared session-key builder
    claude.rs       Claude Code SDK integration (claude-agent-sdk-rust crate)
    opencode.rs     opencode CLI-subprocess harness; translate_event, sanitize_stderr
    codex.rs        Codex harness stub
    gemini.rs       Gemini harness stub
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
4. **cancel_rx** -- socket cancel requests; sends `C-c` to foreground tmux pane
5. **health_interval** -- 5s timer, crashed-session detection + `StateUpdate::Tick`
6. **poll_interval** -- 250ms timer, capture-pane scrollback read
7. **ctrl_c** -- graceful shutdown; calls `App::mark_clean_shutdown()` to flip
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

Uses `claude-agent-sdk-rust` crate, not terminal scraping. Claude prompts spawn via `tokio::spawn` with an mpsc event channel. Multi-turn sessions are tracked by session name -> Claude session ID in `ClaudeHarness`. Two modes:
- One-shot: `: claude <prompt>`
- Interactive: `: claude on` / `: claude off` toggles plain text routing

**Named sessions:** Users can explicitly name and resume Claude conversation sessions using CLI flags:
- `--name <name>` / `-n <name>` — **create-or-resume** (upsert). If the session exists, resumes it with a notification. If not, creates a new one.
- `--resume <name>` / `--continue <name>` — **strict resume**. Errors if the session doesn't exist. Catches typos and LRU-evicted sessions.
- Session names follow the same rules as terminal session names (alphanumeric, hyphens, underscores, max 64 chars).
- Both flags work in one-shot (`: claude --name auth fix bug`) and interactive (`: claude on --name auth`) modes.
- `--name` and `--resume` are mutually exclusive.
- Only works with harnesses that support resume (Claude and opencode).

**Session persistence:** Named sessions persist across restarts. The session index (name -> session_id + working directory) is stored in `terminus-state.json` via StateStore. The Claude SDK persists conversation state in `.claude/`. On resume, the stored working directory is passed to the SDK.

**LRU eviction:** Named sessions are capped at `max_named_sessions` (default 50, configurable in `[harness]` section of `terminus.toml`). When the cap is reached, the least-recently-used session is evicted.

**Breaking change:** The `-n` short flag was reassigned from `--max-turns` to `--name`. Use `-t` for `--max-turns` instead.

### opencode integration

Uses the `opencode` CLI directly — each prompt spawns `opencode run --format json`
as a short-lived child process with `kill_on_drop(true)`. Mirrors the
`claude-agent-sdk-rust` subprocess pattern, NOT an HTTP sidecar.

- Config is inherited from opencode's own config (model, agent, provider, auth).
- Session resume uses `--session <ses_id>`. Terminus captures the first
  `sessionID` it sees on stdout and persists the name → id mapping under a
  prefixed key `opencode:<name>` in `terminus-state.json`.
- Ambient events: `HarnessStarted` / `HarnessFinished` at prompt boundaries.
- Tool-use events: terminus translates opencode's atomic tool_use JSON events
  (emitted when opencode uses a tool-enabled agent) into `HarnessEvent::ToolUse`
  with structured `tool`, `description`, `input`, and `output` fields.
- No persistent sidecar, no port binding, no shutdown hook needed.

Optional `[harness.opencode]` overrides (see `terminus.example.toml`):
- `binary_path`: override the opencode CLI location (default: resolved via PATH)
- `model`: pass `-m <value>` to `opencode run` (default: opencode's own default)
- `agent`: pass `--agent <value>` to `opencode run` (default: opencode's own default)

**Supported subcommands from chat:** `models`, `stats`, `sessions` (`session list`/`ls`), `providers` (`auth list`/`ls`), `export <id>`. Full reference: [docs/opencode.md](docs/opencode.md).

**Blocked from chat** (chat-safe errors returned; run in terminal): `acp`, `agent`, `attach`, `auth`, `debug`, `github`, `import`, `login`, `logout`, `mcp`, `serve`, `session`, `tui`, `uninstall`, `upgrade`, `web`. Full details in [docs/opencode.md](docs/opencode.md).

**Per-prompt flags** (`: opencode [flags] <prompt>`):
- `--name <x>` / `--resume <x>` / `--continue <x>` — named session
- `--continue` (no value, followed by another flag or end of flags) — continue opencode's last session (maps to `opencode run --continue`)
- `--model <provider/model>` (alias: `-m`) — model override; overrides `[harness.opencode] model` when passed per-prompt
- `--agent <name>` — agent override (e.g. "build" for tool-use); overrides `[harness.opencode] agent` when passed per-prompt
- `--title <str>` — human-readable session title
- `--share` — ask opencode for a shareable URL
- `--pure` — run without external plugins
- `--fork` — fork the session before continuing (requires `--continue` or `--resume`)

Full flag semantics and mutual exclusion: [docs/opencode.md#per-prompt-flags](docs/opencode.md#per-prompt-flags).

Subcommand output is wrapped in a fenced code block and truncated at 3000
chars. For long outputs, run the CLI in your terminal.

**Known limitations:**
- Cross-harness state-persist-failure (state file only persists one entry per
  key; the `{kind}:{name}` prefix scheme prevents collisions between harnesses
  but the underlying state-store write path is shared). Not opencode-specific.

### Platform adapters

All three implement `ChatPlatform` (async_trait). Auth is single-user: messages from non-authorized user IDs are silently dropped. Telegram uses manual `getUpdates` long-polling (not webhooks). Slack uses Socket Mode WebSocket with auto-reconnect. Discord uses the serenity crate with gateway intents `DIRECT_MESSAGES | GUILD_MESSAGES | MESSAGE_CONTENT` (privileged). Inbound Discord attachments (images/files sent by the user to the bot) are not currently processed -- only `msg.content` text is forwarded to the main loop. Telegram-parity for inbound attachments is a follow-up.

### WebSocket bidirectional API (`src/socket/`)

Optional, opt-in (`[socket] enabled = true`). Exposes an authenticated WebSocket
endpoint for local programs and remote agents. Transport is plain `ws://`; deploy
behind a reverse proxy for `wss://` TLS termination.

**Architecture:** Per-connection task model. `SocketServer` binds a `TcpListener`,
authenticates via `Authorization: Bearer <token>` at the HTTP upgrade (tokens from
`[[socket.client]]` entries in config via `Arc<ArcSwap<Vec<SocketClient>>>`), and
spawns a per-connection task.

**Integration seams (no structural changes to App/session/harness):**
- Inbound: `mpsc::Sender<IncomingMessage>` (same channel as chat adapters). Socket
  sets `socket_reply_tx` on `ReplyContext` so `App::send_reply` routes responses
  back to the socket instead of to a chat platform.
- Cancel: `mpsc::Sender<String>` carries cancel request_ids from socket connections
  to the main loop, which sends `C-c` to the foreground tmux pane.
- Per-request output: `broadcast::channel<StreamEvent>` (existing). The connection
  task subscribes and translates matching events for subscribed clients.
- Ambient events: `broadcast::channel<AmbientEvent>` (new, capacity 512). 7 new
  event types emitted from `App` methods: `SessionCreated`, `SessionKilled`,
  `SessionLimitReached`, `ChatForward`, `HarnessStarted`, `HarnessFinished`,
  `SessionOutput` (translated from `StreamEvent::NewMessage` at socket layer).
- `PlatformType` is NOT modified (preserves golden-tested queue-file wire format).

**Config hot-reload:** `src/socket/config_watcher.rs` uses the `notify` crate to
watch `terminus.toml` for changes, debounces at 500ms, and atomically swaps the
`[[socket.client]]` list via `ArcSwap`. Only client tokens/names are hot-reloaded;
structural config (bind, port, limits) still requires restart. Existing connections
are not disrupted.

**Persistent subscriptions:** `SharedSubscriptionStore` (`Arc<Mutex<HashMap<String,
Vec<(String, Filter)>>>>`) in `SocketServer` saves subscriptions by client_name on
every subscribe/unsubscribe. Clients send `hello` with `restore_subscriptions: true`
to restore saved subscriptions on reconnect. In-memory only — server restart clears.

**Binary frames:** Two-phase protocol: `attachment_meta` JSON envelope declares
upcoming binary frame, then a WebSocket binary frame with the payload. Connection
state machine (`PendingBinary`) enforces one pending at a time, max size
(`max_binary_bytes`, default 10 MiB), and 30s timeout. Binary data is written to
`/tmp/terminus-attachment-{ulid}.{ext}` and forwarded via `IncomingMessage.attachments`.

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
- **opencode must be on PATH** (or `binary_path` set in `[harness.opencode]`). The opencode harness shells out to the CLI.
- **No shared mutable state.** The main loop owns all mutable state directly; platform adapters only send/receive through channels.
- **Rate limiting is per-platform.** `edit_throttle_ms` config controls minimum gap between message edits to stay within Telegram/Slack API limits.
- **Startup reconciliation.** On restart, surviving `term-*` tmux sessions are auto-reconnected. Chat binding is re-established on first user message (chat IDs are not persisted to disk).

## Testing

Unit tests cover command parsing and blocklist (command.rs) and output buffer line counting / noise filtering (buffer.rs). No integration tests yet -- tmux operations require a live tmux server.

Run with: `cargo test`

Integration tests (e.g. opencode end-to-end) are gated by `#[ignore]` + env
vars. See `docs/integration-tests.md` for how to run them.

## Agent orchestration

When delegating work to subagents (via the `Task` / `Agent` tool), **always use
`sonnet` or `haiku` — not `opus`** unless the task genuinely requires deep
architectural reasoning that smaller models demonstrably cannot handle. Pick
the model by scope:

- **`haiku`**: lookups, single-file edits, small bug fixes, doc tweaks,
  grep-and-report tasks
- **`sonnet`** (default): multi-file refactors, feature implementations,
  test suites, plan execution, integration work
- **`opus`**: reserved for genuinely hard problems — cross-system architecture
  reviews, deep debugging of subtle concurrency bugs, consensus-mode
  Planner/Architect/Critic passes. Flag in your prompt why opus is warranted.

Opus is expensive and rate-limited; defaulting to sonnet/haiku keeps cycles
available for the cases that truly need them. If you're unsure, start with
sonnet and escalate only if the output is insufficient.
