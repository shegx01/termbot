# terminus

**Drive your terminal from Telegram, Slack, Discord, or any WebSocket client.**

terminus is a single-user Rust program that bridges tmux sessions to chat
platforms and a programmatic WebSocket API. Anything you'd do at a
terminal — deploy a release, tail prod logs, SSH into a box, scroll back
through output — now works from your phone, a laptop without your dotfiles,
or a script. If you can pipe it through tmux, you can drive it from chat.

```
: new prod
: ssh api-1 'docker logs -f web'      # tail prod logs from your phone
: kubectl rollout status deploy/api   # check a deploy mid-meeting
: cargo build --release               # kick off a long build, walk away
```

Optional AI harnesses (Claude, Codex, Gemini, opencode) sit on top of the
same surface — named sessions, image input, schema-validated output, live
tool-use streaming.

```
: claude on --name auth --model sonnet
What does the auth module do?
[send a screenshot] what's wrong with this error?
```

---

## Install

```bash
curl -sSL https://raw.githubusercontent.com/shegx01/terminus/main/install.sh | bash
```

The installer detects your OS, walks you through configuration, registers a
system service with auto-restart, and starts terminus. Open your chat client
and start typing.

<details>
<summary>What the installer does</summary>

1. Detects OS and architecture (macOS / Linux, x86_64 / ARM64)
2. Checks for tmux and Claude Code CLI
3. Walks you through Telegram / Slack / Discord configuration
4. Downloads the binary to `~/.local/bin/terminus`
5. Writes config to `~/.config/terminus/terminus.toml`
6. Registers a system service (systemd / launchd) with auto-restart
7. Sets up a daily update check
8. Starts terminus

**Flags:** `--quick` (skip help text), `--upgrade` (replace binary, keep config), `--uninstall` (stop service, keep config).

```bash
curl -sSL https://raw.githubusercontent.com/shegx01/terminus/main/install.sh | bash -s -- --upgrade
curl -sSL https://raw.githubusercontent.com/shegx01/terminus/main/install.sh | bash -s -- --uninstall
```
</details>

<details>
<summary>Manual install (pre-built binary or from source)</summary>

Pre-built binaries are on the [Releases](https://github.com/shegx01/terminus/releases/latest) page.

```bash
# Pre-built (macOS Apple Silicon shown)
curl -L -o terminus \
  https://github.com/shegx01/terminus/releases/latest/download/terminus-aarch64-apple-darwin
chmod +x terminus
cp terminus.example.toml terminus.toml   # edit with your tokens
./terminus

# Or build from source (Rust 1.70+)
git clone https://github.com/shegx01/terminus.git
cd terminus
cp terminus.example.toml terminus.toml
cargo build --release
./target/release/terminus

# Single-platform builds (smaller dep graph) — see docs/feature-flags.md
cargo build --release --no-default-features --features telegram
```

Set `TERMINUS_CONFIG=/path/to/config.toml` to use a non-default config location.
</details>

---

## What it does

**Terminal control** — run commands, manage tmux sessions, anything interactive you can do in terminal:

```
: new dev                 # create a session
: cargo build --release   # run a command in the foreground session
: list                    # see all sessions
: bg                      # background the current session
: fg dev                  # switch sessions
: kill dev                # destroy a session
: screen                  # snapshot the terminal screen
```

**Claude Code** — Structured typed output, real-time tool activity, image input, named
sessions:

```
: claude explain this codebase
: claude on --name auth --model sonnet
What does the auth module do?
[send a screenshot] what's wrong with this error?
: claude off
```

Full reference: [docs/claude.md](docs/claude.md)

**Codex** — OpenAI's `codex` CLI from chat (verified against codex-cli
**0.128.0**), with paired tool-use events, named sessions, image attachments,
and structured output:

```
: codex --model gpt-5.5 explain this codebase
: codex on --name auth --sandbox workspace-write
Can you fix the JWT validation bug?
```

Defaults to `gpt-5.5` with `workspace-write` sandbox. Full reference: [docs/codex.md](docs/codex.md)

**Gemini** — Google's `gemini` CLI from chat, with tool-use events and named
sessions:

```
: gemini on --name review --approval-mode yolo
What does the auth module do?
Can you also run the unit tests?
: gemini sessions             # list saved sessions
: gemini extensions           # list installed extensions
```

Full reference: [docs/gemini.md](docs/gemini.md)

**opencode** — prompt opencode from chat with named sessions and CLI
subcommand passthrough:

```
: opencode --agent build refactor this module
: opencode stats --days 7      # token usage + cost
: opencode sessions            # recent session IDs
```

Full reference: [docs/opencode.md](docs/opencode.md)

**Programmatic access** — drive terminus from scripts and agents over
WebSocket:

```bash
websocat ws://127.0.0.1:7645 -H "Authorization: Bearer tk_live_..."
```

```json
> {"type":"request","request_id":"r1","command":": list"}
< {"type":"result","request_id":"r1","value":{"text":"..."},"cancelled":false}
```

Full reference: [docs/socket.md](docs/socket.md)

---

## Support matrix

| Platform  | Inbound text | Inbound attachments | Notes |
|-----------|:------------:|:-------------------:|-------|
| Telegram  |      ✓       |          ✓          | Long-polling |
| Slack     |      ✓       |          ✓          | WebSocket (Slack's Socket Mode) + `conversations.history` wake-recovery |
| Discord   |      ✓       |          ✓          | Gateway + REST catchup; threads in guilds, replies in DMs |
| WebSocket |      ✓       |          ✓          | Binary-frame upload; opt-in |

| Harness  | One-shot | Interactive | Named sessions | Tool events | Schema | Attachments |
|----------|:--------:|:-----------:|:--------------:|:-----------:|:------:|:-----------:|
| claude   |    ✓     |      ✓      |       ✓        |      ✓      |   ✓    |     ✓       |
| codex    |    ✓     |      ✓      |       ✓        |      ✓      |   ✓    | image-only  |
| gemini   |    ✓     |      ✓      |       ✓        |      ✓      |   ✗    |     ✗       |
| opencode |    ✓     |      ✓      |       ✓        |   ✓ (build) |   ✗    |     ✓       |

**Notes:** opencode emits tool events only when run with a tool-enabled agent
(`--agent build`). gemini and opencode have no schema-constrained output
surface — use `: claude --schema=<name>` or `: codex --schema=<name>` for
validated structured output. Codex image-attachment whitelist:
`image/png`, `image/jpeg`, `image/jpg`, `image/webp`. See the per-harness docs
for full event schemas.

---

## Requirements

- **Rust 1.70+** (only for building from source)
- **tmux** on PATH (any `base-index` / `pane-base-index` setting; sessions are targeted by name, never by index)
- **Claude Code CLI** for `: claude` (`npm i -g @anthropic-ai/claude-code`)
- **opencode** / **gemini** / **codex** CLIs on PATH for those harnesses (all optional)
- At least one of: Telegram bot token, Slack bot + app tokens, Discord bot token, or socket API enabled

---

## Configuration

The `[blocklist]` and `[streaming]` sections are shared across all platforms.
Defaults shown in the Telegram example below; other snippets omit them.

### Telegram

```toml
[auth]
telegram_user_id = 123456789

[telegram]
bot_token = "7012345678:AAH..."

[blocklist]
patterns = [
  "rm\\s+-[a-z]*f[a-z]*r[a-z]*\\s+/",
  "sudo\\s+",
  ":\\(\\)\\{\\s*:\\|:\\&\\s*\\};:",
  "mkfs\\.",
  "dd\\s+if=",
]

[streaming]
edit_throttle_ms = 2000
poll_interval_ms = 250
chunk_size = 4000
offline_buffer_max_bytes = 1048576
max_sessions = 10
```

<details>
<summary>How to get a bot token and your user ID</summary>

**Bot token:** message [@BotFather](https://t.me/BotFather), send `/newbot`, follow prompts.
**User ID:** message [@userinfobot](https://t.me/userinfobot) — it replies with your numeric ID.
</details>

### Slack

```toml
[auth]
slack_user_id = "U01ABCDEF"

[slack]
bot_token = "xoxb-..."
app_token = "xapp-..."
channel_id = "C01ABCDEF"
```

<details>
<summary>How to set up a Slack app</summary>

1. [api.slack.com/apps](https://api.slack.com/apps) → create new app ("From scratch")
2. **Socket Mode**: enable, generate app-level token with `connections:write` (`xapp-...`)
3. **OAuth & Permissions**: add bot OAuth scopes `chat:write`, `channels:history`, `channels:read`, `im:history` (add `groups:history`, `groups:read` for private channels). `im:history` is required for DM wake-recovery — omit it and missed DMs vanish silently after sleep.
4. Install to workspace, copy bot token (`xoxb-...`)
5. **Event Subscriptions**: enable, subscribe to `message.channels` (and `message.groups` for private)
6. Invite the bot: `/invite @botname`
7. Channel ID: right-click channel → "View channel details"
8. Member ID: profile → three dots → "Copy member ID"
</details>

### Discord

```toml
[auth]
discord_user_id = 123456789012345678

[discord]
bot_token = "YOUR_BOT_TOKEN_HERE"
# guild_id   = 111222333444555666     # omit for DM-only mode
# channel_id = 666555444333222111
```

<details>
<summary>How to set up a Discord bot</summary>

1. [discord.com/developers/applications](https://discord.com/developers/applications) → create app
2. **Bot section**: reset token, copy
3. **Privileged Gateway Intents**: enable **MESSAGE CONTENT INTENT** (required); also **SERVER MEMBERS INTENT** for guild channels
4. **OAuth2 → URL Generator**: scope `bot`, permissions: View Channels, Send Messages, Attach Files, Read Message History. Open the generated URL to invite the bot.
5. **DM-only mode**: omit `guild_id` and `channel_id`. **Guild mode**: set both.

**Discord IDs** (called "snowflakes" — numeric strings): Settings → Advanced → Developer Mode, then right-click any user/server/channel → Copy ID.
</details>

### Socket API

```toml
[socket]
enabled = true

[[socket.client]]
name = "my-agent"
token = "tk_live_your-32-character-minimum-secret"
```

Opt-in (`enabled = false` by default). Each client authenticates with a named
Bearer token. See [Socket API](#socket-api) below and [docs/socket.md](docs/socket.md)
for the wire protocol.

### Harness overrides

All harness configuration is optional — terminus inherits each CLI's own
config (auth, default model, profiles).

```toml
[harness.claude]
# See docs/claude.md for the full set.

[harness.opencode]
# binary_path = "/usr/local/bin/opencode"
# model = "openrouter/anthropic/claude-haiku-4-5"
# agent = "build"   # use "build" for tool-use-enabled prompts

[harness.gemini]
# binary_path   = "/usr/local/bin/gemini"
# model         = "flash"          # pro | flash | flash-lite
# approval_mode = "default"        # default | auto_edit | yolo | plan

[harness.codex]
# binary_path        = "/opt/homebrew/bin/codex"
# model              = "gpt-5.5"           # gpt-5.4 / gpt-5.4-mini also valid
# sandbox            = "workspace-write"   # read-only | workspace-write | danger-full-access
# profile            = "default"
# ignore_user_config = false
```

Run each CLI's auth flow once before use (`opencode auth login`,
`gemini` OAuth, `codex login`). Per-harness flag references and blocked-from-chat
subcommand lists live in [docs/claude.md](docs/claude.md), [docs/codex.md](docs/codex.md),
[docs/gemini.md](docs/gemini.md), and [docs/opencode.md](docs/opencode.md).

### Sleep/wake (optional)

terminus prevents idle sleep while the lid is open and the host is on AC.
Override in `[power]`:

```toml
[power]
enabled              = true     # set false for CI/headless
stayawake_on_battery = false    # true to inhibit on battery too
# state_file = "/absolute/path/to/terminus-state.json"
```

### Command trigger (optional)

```toml
[commands]
trigger = "!"   # default is `: ` — allowed: `: ! > ; . , @ ~ ^ - + = | % ?`
```

### Multiple platforms

Include any combination of `[telegram]`, `[slack]`, `[discord]`, and
`[socket]`. terminus starts with whatever is configured. Each platform is
also a build-time Cargo feature (default = full set); see
[docs/feature-flags.md](docs/feature-flags.md) to prune the dependency graph.

---

## Commands reference

All commands use the `: ` (colon + space) prefix. Plain text (no prefix) is
sent as stdin to the foreground session — or to the active harness when one
is on.

| Command | What it does |
|---------|--------------|
| `: new <name>` | Create a named terminal session |
| `: fg <name>` | Bring a session to the foreground |
| `: bg` | Background the current session |
| `: list` | Show all sessions and their status |
| `: kill <name>` | Destroy a session |
| `: screen` | Send a snapshot of the terminal screen to chat |
| `: <command>` | Run in the foreground session (e.g. `: ls -la`) |
| `: claude <prompt>` | One-shot Claude prompt with structured response |
| `: claude on [opts]` | Enter interactive Claude mode |
| `: claude off` | Exit Claude mode |
| `: codex <prompt>` | One-shot Codex prompt |
| `: gemini <prompt>` | One-shot Gemini prompt |
| `: opencode <prompt>` | One-shot opencode prompt |

Session names: alphanumeric, hyphens, underscores, max 64 characters.

---

## Named sessions

All four harnesses support named, resumable sessions:

```
: claude --name auth fix the JWT validation
: opencode --name review look at the PR
: codex --resume auth keep going
```

- `--name <x>` — **create-or-resume** (upsert)
- `--resume <x>` / `--continue <x>` — **strict resume** (errors if missing)
- `--name` and `--resume` are mutually exclusive

Names persist across restarts in `terminus-state.json` under per-harness
prefixes (`claude:auth`, `codex:auth`, etc.) and are LRU-evicted at
`max_named_sessions` (default 50, shared across harnesses).

> **Breaking change:** `-n` was previously the short flag for `--max-turns`.
> It now means `--name`. Use `-t` for `--max-turns`.

---

## Structured output

The claude and codex harnesses can emit a JSON response validated against a
schema you define, then POST it to a webhook with HMAC-SHA256 authentication.

| Harness  | Validation | Fenced JSON in chat | Webhook delivery |
|----------|:----------:|:-------------------:|:----------------:|
| claude   |     ✓      |          ✓          |        ✓         |
| codex    |     ✓      |          ✓          | registered names only |
| gemini   |     ✗      |          —          |        —         |
| opencode |     ✗      |          —          |        —         |

Codex `--schema` accepts three forms — registered name (`[schemas.<name>]`),
file path, or inline JSON — but only registered names feed the webhook
delivery pipeline.

```toml
[schemas.todos]
schema = '''
{
  "type": "object",
  "required": ["todos"],
  "properties": {
    "todos": {
      "type": "array",
      "items": {
        "type": "object",
        "required": ["title", "done"],
        "properties": {
          "title": { "type": "string" },
          "done":  { "type": "boolean" }
        }
      }
    }
  }
}
'''

# Optional webhook delivery
webhook            = "https://your-server.example.com/webhooks/todos"
webhook_secret_env = "TODOS_WEBHOOK_SECRET"
```

```sh
export TODOS_WEBHOOK_SECRET="$(openssl rand -hex 32)"
```

```
: claude --schema todos list my open tasks
```

Webhook requests carry four headers:

| Header                  | Value                                              |
|-------------------------|----------------------------------------------------|
| `X-Terminus-Schema`     | Schema name from `[schemas.<name>]`                |
| `X-Terminus-Run-Id`     | ULID — sortable unique identifier per run          |
| `X-Terminus-Timestamp`  | Unix epoch seconds at delivery time                |
| `X-Terminus-Signature`  | `v1=<hmac-sha256-hex>` over `"<timestamp>.<body>"` |

The Stripe-style signature binds the timestamp into the MAC, so neither field
can be altered without invalidating the signature. Transient failures are
retried with exponential backoff up to 60s; jobs survive restarts in
`<queue_dir>/pending/`. Cap retry duration via
`structured_output.max_retry_age_hours`.

Verification snippets (Python, Node.js) and the full retry table:
[docs/claude.md#structured-output](docs/claude.md).

---

## Socket API

Programs, scripts, and agents drive terminus over WebSocket with typed JSON
envelopes, request pipelining, and live event subscriptions.

- **Transport:** plain `ws://` (deploy behind nginx / Caddy for TLS)
- **Auth:** Bearer token at the HTTP upgrade
- **Protocol:** `terminus/v1` — one JSON envelope per WebSocket message

```bash
websocat ws://127.0.0.1:7645 -H "Authorization: Bearer tk_live_your-32-character-minimum-secret"
```

```json
{"type":"request","request_id":"r1","command":": new build"}
```

The server responds with a sequence: `ack` → optional streaming frames →
`result` or `error` → `end`. Multiple requests can be pipelined (FIFO per
connection).

**Subscriptions** filter on `event_types`, `schemas`, and `sessions` (OR
within a facet, AND across facets). Available event types:

| Event type              | Source                                       |
|-------------------------|----------------------------------------------|
| `structured_output`     | `--schema` result (schema, value, run_id)    |
| `webhook_status`        | Webhook delivery attempt outcome             |
| `queue_drained`         | Webhook retry queue drain cycle complete     |
| `session_output`        | tmux capture-pane diff                       |
| `session_created`       | `: new` succeeded                            |
| `session_killed`        | `: kill` succeeded                           |
| `session_started`       | Session foregrounded (`: fg`)                |
| `session_exited`        | Underlying tmux process exited               |
| `session_limit_reached` | `streaming.max_sessions` cap hit             |
| `chat_forward`          | Inbound message from a chat platform         |
| `harness_started`       | AI-harness turn started                      |
| `harness_finished`      | AI-harness turn completed                    |
| `gap_banner`            | Sleep/wake gap detected                      |

**Defaults:** 60-request burst / 20 req/s sustained, 32 pending requests, 16
concurrent connections, 300s idle timeout. Size caps are split: `max_message_bytes`
defaults to 1 MiB (JSON envelope), `max_binary_bytes` defaults to 10 MiB
(binary attachment frames). Rate-limited requests get an `error` with
`code: "rate_limited"` and `retry_after_ms`.

Full wire protocol, all envelope types, error codes, and proxy examples:
[docs/socket.md](docs/socket.md).

---

## Security

**Authentication.** Single-user only on chat platforms — messages from any
user ID other than the configured one are silently dropped (terminus does
not signal its presence to unauthorised senders). Socket clients
authenticate with per-client Bearer tokens (≥32 characters) validated in
constant time; missing or invalid tokens get HTTP 401 before the WebSocket
upgrade.

**Command blocklist.** Both `: ` prefixed commands and plain-text input
(including text routed to a harness) are checked against regex patterns. The
defaults block:

| Pattern | Blocks |
|---------|--------|
| `rm\s+-[a-z]*f[a-z]*r[a-z]*\s+/` | Recursive force-delete from root (any flag order) |
| `sudo\s+` | Privilege escalation |
| `:\(\)\{\s*:\|:\&\s*\};:` | Fork bomb |
| `mkfs\.` | Filesystem format |
| `dd\s+if=` | Raw disk write |

Commands are normalized before matching to defeat common evasion: path
prefixes are stripped (`/usr/bin/sudo` → `sudo`), backslashes removed
(`su\do` → `sudo`), flag order ignored (`rm -fr /` ≡ `rm -rf /`), whitespace
collapsed. Multi-line messages are rejected outright.

Add patterns:

```toml
[blocklist]
patterns = ["shutdown", "reboot"]
```

**Output file safety.** Files delivered from Claude must have an
allowlisted extension (common images, documents, and text/data formats —
full list in [docs/claude.md](docs/claude.md)), be under the working
directory or `/tmp` (path traversal blocked), and be ≤50 MB. Sensitive
filenames are never delivered.

**Smart quote normalization.** Mobile keyboards' curly quotes are normalized
to ASCII automatically so shell commands work.

---

## Sleep/wake recovery

terminus blocks idle sleep while running: `caffeinate -i` on macOS,
`systemd-inhibit --what=idle:sleep` on Linux. Closed-lid sleep is never
blocked.

When the host sleeps anyway (lid close, forced suspend, overnight), terminus
detects the wake via monotonic/wall-clock divergence (>30s) and emits a
one-time banner per chat:

```
⏸ paused at 02:13, resumed at 07:45 (gap: 5h 32m)
```

Adapter polling is paused until each banner is acked, then the backlog
drains: Telegram via server-side queueing, Slack via `conversations.history`
catchup, Discord via REST pagination. Per-channel watermarks are
force-persisted to `terminus-state.json` so multi-hour sleep cycles do not
lose messages. Restart-during-sleep is gated by a compound check
(`wall_gap > 30s` AND previous shutdown was unclean) so build-and-restart
cycles don't fire spurious banners.

Verify the inhibitor:

```sh
pmset -g assertions | grep PreventUserIdleSystem    # macOS
systemd-inhibit --list | grep terminus              # Linux
```

Full mechanism: [docs/slack-parity.md](docs/slack-parity.md), [docs/discord-parity.md](docs/discord-parity.md).

---

## Troubleshooting

<details>
<summary>"No active session"</summary>

Create one: `: new <name>`. After a restart, sessions auto-reconnect — send
`: list` to check.
</details>

<details>
<summary>No output after restart</summary>

Reconnected sessions need a chat binding. Send `: list` or `: fg <name>`
first — this tells terminus which chat to deliver to.
</details>

<details>
<summary>No output appearing at all</summary>

Check tmux is installed (`tmux -V`). Try `: new test` then `: echo hello`.
</details>

<details>
<summary>Telegram rate limit errors</summary>

Increase `streaming.edit_throttle_ms`. Telegram allows ~30 edits/min.
</details>

<details>
<summary>Slack not connecting</summary>

Verify Socket Mode is enabled and the `xapp-` token is correct. Check that
you subscribed to `message.channels`.
</details>

<details>
<summary>Claude / opencode / gemini / codex commands not working</summary>

Each harness needs its CLI on PATH and authenticated:
- **Claude:** `npm i -g @anthropic-ai/claude-code && claude login` — verify `claude -p "hello"`
- **opencode:** install from [opencode.ai](https://opencode.ai), `opencode auth login` — verify `opencode models`
- **gemini:** install from [google-gemini/gemini-cli](https://github.com/google-gemini/gemini-cli), authenticate via OAuth — verify `gemini --help`
- **codex:** `brew install --cask codex` or `npm i -g @openai/codex`, `codex login` — verify `codex --version`

If terminus can't find a binary, set `binary_path` in the relevant
`[harness.<name>]` table. Per-harness troubleshooting: [docs/claude.md](docs/claude.md#troubleshooting), [docs/codex.md](docs/codex.md), [docs/gemini.md](docs/gemini.md), [docs/opencode.md](docs/opencode.md).
</details>

<details>
<summary>"Maximum session limit reached"</summary>

Default is 10 concurrent sessions. Kill unused with `: kill <name>` or raise
`streaming.max_sessions`.
</details>

<details>
<summary>Socket: connection refused / 401 / lagged</summary>

- **Refused:** check `[socket] enabled = true`, at least one
  `[[socket.client]]`, and the bind/port (`ss -tlnp | grep 7645`).
- **401:** verify `Authorization: Bearer <token>` header and that the token
  matches a `[[socket.client]]` entry exactly (≥32 chars).
- **Lagged warnings:** the broadcast buffer overflowed because the client is
  consuming events too slowly. Events were dropped. Increase
  `socket.send_buffer_size` or process events faster. Non-fatal.
</details>

---

## License

MIT
