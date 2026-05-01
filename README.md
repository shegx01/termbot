# terminus

Control your terminal from Telegram, Slack, Discord, or any WebSocket client. Built in Rust.

terminus gives you remote access to terminal sessions and AI harnesses from your phone or from code. Run shell commands, manage tmux sessions, send images, and have multi-turn AI conversations -- through chat platforms or a programmatic WebSocket API.

---

## Contents

- [Support matrix](#support-matrix)
- [Quick start](#quick-start)
- [What can it do?](#what-can-it-do)
- [Requirements](#requirements)
- [Configuration](#configuration)
- [Commands reference](#commands-reference)
- [Named sessions](#named-sessions)
- [Structured Output (--schema)](#structured-output---schema)
- [Socket API](#socket-api)
- [How it works](#how-it-works)
- [Security](#security)
- [Configuration reference](#configuration-reference)
- [Architecture](#architecture)
- [Troubleshooting](#troubleshooting)

---

## Support matrix

**Chat platforms** (inbound + outbound messages):

| Platform  | Text | Inbound attachments | Notes |
|-----------|:----:|:-------------------:|-------|
| Telegram  |  ✓   |          ✓          | Long-polling; most complete integration |
| Slack     |  ✓   |          ✓          | Socket Mode; lossless wake-recovery via `conversations.history` catchup |
| Discord   |  ✓   |          ✗          | Gateway; inbound attachments deferred |
| WebSocket |  ✓   |          ✓          | Binary-frame upload; opt-in |

**AI harnesses** (the `: <name>` prefixed commands):

| Harness  | One-shot | Interactive | Named sessions | Tool-use events | Structured output | Attachments |
|----------|:--------:|:-----------:|:--------------:|:---------------:|:-----------------:|:-----------:|
| claude   |    ✓     |      ✓      |       ✓        |        ✓        |         ✓         |      ✓      |
| opencode |    ✓     |      ✓      |       ✓        |       ✓¹        |         ✗²        |      ✓      |
| gemini   |    ✓     |      ✓      |       ✓        |       ✓³        |         ✗²        |      ✗⁴     |
| codex    |    ✓     |      ✓      |       ✓        |       ✓⁵        |        ✓⁶         |     ✓⁷      |

¹ Tool-use events are forwarded when opencode emits them; tool-enabled agents (e.g. "build") emit tool_use JSON events, others may not. Set `agent = "build"` in `[harness.opencode]` config or pass `--agent build` per-prompt
² Opencode and gemini CLIs have no schema-constrained output surface -- use the claude harness for `--schema` workflows
³ Tool-use events from gemini arrive as separate `tool_use` + `tool_result` frames linked by `tool_id`; terminus coalesces them into a single event with both input and output. Enable tool-using behavior with `--approval-mode yolo` (or `auto_edit` / `plan`)
⁴ Inbound attachments (images / files) are rejected by the gemini harness with a chat-safe error rather than silently dropped; multimodal threading is a follow-up
⁵ Tool-use events from codex arrive as paired `item.started` + `item.completed` frames linked by `item.id` for tool kinds (`command_execution`, `file_change`, `mcp_tool_call`, `web_search`, `plan_update`); `agent_message` items go straight to text. terminus reuses the same `ToolPairingBuffer` as gemini
⁶ Codex `--schema` accepts three forms — registered name (`[schemas.<name>]` in `terminus.toml`), file path, or inline JSON — all resolved to a temp file and passed as `--output-schema <path>`. The registered-name form additionally feeds the webhook-delivery pipeline (HMAC-SHA256, retry queue, full parity with claude). Inline-JSON and file-path forms render as chat-only fenced JSON, no webhook delivery.
⁷ Image-only attachment whitelist (case-insensitive): `image/png`, `image/jpeg`, `image/jpg`, `image/webp`. Non-image MIME types are rejected with a chat-safe error

---

## Quick start

### One-line install (recommended)

The installer downloads the binary, walks you through configuration, installs dependencies, and sets up a system service with auto-restart and update notifications.

```bash
curl -sSL https://raw.githubusercontent.com/shegx01/terminus/main/install.sh | bash
```

That's it. Open Telegram, Slack, or Discord and start typing.

<details>
<summary>What the installer does</summary>

1. Detects your OS and architecture (macOS/Linux, x86_64/ARM64)
2. Checks for tmux (offers to install if missing) and Claude Code CLI (required)
3. Walks you through Telegram/Slack configuration with inline help
4. Downloads the correct binary to `~/.local/bin/terminus`
5. Writes config to `~/.config/terminus/terminus.toml`
6. Registers a system service (systemd on Linux, launchd on macOS) with auto-restart
7. Sets up a daily update check with OS-native notifications
8. Starts terminus

**Flags:**
- `--quick` — skip inline help text during setup
- `--upgrade` — download latest binary, restart service (config untouched)
- `--uninstall` — stop service, remove files (config preserved by default)

```bash
# Upgrade
curl -sSL https://raw.githubusercontent.com/shegx01/terminus/main/install.sh | bash -s -- --upgrade

# Uninstall
curl -sSL https://raw.githubusercontent.com/shegx01/terminus/main/install.sh | bash -s -- --uninstall
```
</details>

### Download a pre-built binary

Pre-built binaries are also available directly on the [Releases](https://github.com/shegx01/terminus/releases/latest) page if you prefer manual setup.

```bash
# Download for your platform (macOS Apple Silicon shown)
curl -L -o terminus \
  https://github.com/shegx01/terminus/releases/latest/download/terminus-aarch64-apple-darwin

chmod +x terminus
cp terminus.example.toml terminus.toml   # edit with your tokens
./terminus
```

### Build from source

```bash
git clone https://github.com/shegx01/terminus.git
cd terminus
cp terminus.example.toml terminus.toml   # edit with your tokens
cargo build --release
./target/release/terminus
```

To use a config file at a custom path, set `TERMINUS_CONFIG`:

```bash
TERMINUS_CONFIG=/path/to/config.toml ./terminus
```

---

## What can it do?

**Terminal control** -- run commands, see output, manage sessions:

```
: new dev                        # create a session
: cargo build --release          # run a command
: list                           # see all sessions
: bg                             # background current session
: fg dev                         # switch to a session
: kill dev                       # destroy a session
: screen                         # snapshot the terminal screen
```

**Claude Code** -- ask questions, refactor code, explore projects:

```
: claude explain this codebase
: claude find all TODO comments and prioritize them
```

**Interactive Claude mode** -- multi-turn conversation from chat:

```
: claude on                      # enter Claude mode
What does the auth module do?    # just type naturally
Can you refactor it?             # conversation continues
Show me the test gaps.           # still the same session
: claude off                     # back to terminal
```

**Claude mode with options** -- customize model, effort, context:

```
: claude on --model sonnet --effort high
: claude on --add-dir ../shared-lib
: claude on -m opus -t 10 --add-dir ../api
```

**Image support** -- send photos to Claude directly from chat:

```
: claude on
[send a screenshot with caption] what's wrong with this error?
[send a diagram]                 # photo-only messages work too
: claude off
```

**OpenCode** -- prompt a different AI via the `opencode` CLI:

```
: opencode explain this codebase
: opencode on --name review
What does the auth module do?
Is this idiomatic Rust?
: opencode off
```

**OpenCode CLI subcommands** -- query opencode directly from chat:

```
: opencode models openrouter        # list configured models
: opencode stats --days 7           # token usage + cost
: opencode sessions                 # recent session IDs
: opencode providers                # configured providers
: opencode export ses_abc...        # dump a session as JSON
```

**Full flag + subcommand reference:** [docs/opencode.md](docs/opencode.md)

**Gemini** -- prompt Google's `gemini` CLI from chat, with tool-use events and named sessions:

```
: gemini explain this codebase
: gemini on --name review --approval-mode yolo
What does the auth module do?
Can you also run the unit tests?
: gemini off
```

Gemini CLI flags are mapped to their own idioms -- `-r latest` for bare `--continue`, `-m <alias>` for model (`pro` / `flash` / `flash-lite`), and `--approval-mode` (`default` / `auto_edit` / `yolo` / `plan`). Gemini's interactive subcommands (`update`, `mcp`, `extensions`, `skills`) are blocked from chat.

**Full flag + event-schema reference:** [docs/gemini.md](docs/gemini.md)

**Codex** -- prompt OpenAI's `codex` CLI from chat (verified against codex-cli **0.128.0**), with paired tool-use events, named sessions, image attachments, and structured output:

```
: codex --model gpt-5.5 explain this codebase
: codex on --name auth --sandbox workspace-write
What does the auth module do?
Can you fix the JWT validation bug?
: codex off
```

Codex always runs with `-s workspace-write --skip-git-repo-check` (codex 0.128 deprecated `--full-auto` in favor of explicit `-s <sandbox>`; `codex exec` is non-interactive by default in 0.128 so no separate "skip approval" flag is needed). Sandbox values: `read-only` / `workspace-write` / `danger-full-access`. Default model is `gpt-5.5` as of codex 0.128 — universally available; `gpt-5.4` and `gpt-5.4-mini` are alternatives. (`gpt-5.3-codex` was removed in 0.128.) Image attachments are forwarded via `-i` (image-only whitelist). Chat-safe subcommands: `: codex sessions`, `: codex apply <task_id>`, and `: codex cloud {list,status,diff,apply,exec}` — see [docs/codex.md](docs/codex.md). Other native subcommands (`login`, `mcp`, `resume`, `fork`, etc.) remain blocked.

**Full flag + event-schema reference:** [docs/codex.md](docs/codex.md)

**Programmatic access** -- drive terminus from scripts, agents, or dashboards via WebSocket:

```bash
# Connect with websocat
websocat ws://127.0.0.1:7645 -H "Authorization: Bearer tk_live_..."

# Send a command and get structured JSON back
> {"type":"request","request_id":"r1","command":": list"}
< {"type":"ack","request_id":"r1","accepted_at":"..."}
< {"type":"result","request_id":"r1","value":{"text":"..."},"cancelled":false}
< {"type":"end","request_id":"r1"}

# Subscribe to live session output
> {"type":"subscribe","subscription_id":"s1","filter":{"event_types":["session_output"]}}
```

---

## Requirements

- **Rust 1.70+** (for building from source)
- **tmux** (for terminal sessions)
- **Claude Code CLI** (for `: claude` commands -- `npm i -g @anthropic-ai/claude-code`)
- **opencode** (optional, for `: opencode` commands -- see [OpenCode integration](#opencode-integration-optional))
- **gemini** (optional, for `: gemini` commands -- see [Gemini integration](#gemini-integration-optional))
- **codex** (optional, for `: codex` commands -- see [Codex integration](#codex-integration-optional))
- At least one of: Telegram bot token, Slack bot + app tokens, Discord bot token, or Socket API enabled

---

## Configuration

The `[blocklist]` and `[streaming]` sections are the same across all platforms. See the Telegram example below for the full defaults -- the other platform snippets omit them for brevity.

### Telegram only

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
<summary>How to get your Telegram bot token and user ID</summary>

**Bot token:**
1. Message [@BotFather](https://t.me/BotFather) on Telegram
2. Send `/newbot`, follow the prompts
3. Copy the token

**Your user ID:**
1. Message [@userinfobot](https://t.me/userinfobot)
2. It replies with your numeric ID

**Permissions:** The default bot token grants everything terminus needs (receive messages, send messages, edit messages). No extra config required.
</details>

### Slack only

```toml
[auth]
slack_user_id = "U01ABCDEF"

[slack]
bot_token = "xoxb-..."
app_token = "xapp-..."
channel_id = "C01ABCDEF"

[blocklist]
patterns = [
  "rm\\s+-[a-z]*f[a-z]*r[a-z]*\\s+/",
  "sudo\\s+",
  ":\\(\\)\\{\\s*:\\|:\\&\\s*\\};:",
]

[streaming]
edit_throttle_ms = 2000
poll_interval_ms = 250
chunk_size = 4000
offline_buffer_max_bytes = 1048576
max_sessions = 10
```

<details>
<summary>How to set up a Slack app</summary>

1. Go to [api.slack.com/apps](https://api.slack.com/apps), create a new app ("From scratch")
2. **Socket Mode**: enable it, generate an app-level token with `connections:write` scope (`xapp-...`)
3. **OAuth & Permissions**: add bot scopes `chat:write`, `channels:history`, `channels:read`
4. Install to workspace, copy bot token (`xoxb-...`)
5. **Event Subscriptions**: enable, subscribe to `message.channels`
6. Invite the bot to your channel: `/invite @botname`
7. Get channel ID: right-click channel > "View channel details" > ID at bottom

**Your member ID:** Click profile picture > "Profile" > three dots > "Copy member ID"

**For private channels**, also add `groups:history` and `groups:read` scopes, and subscribe to `message.groups`.
</details>

### Discord only

```toml
[auth]
discord_user_id = 123456789012345678

[discord]
bot_token = "YOUR_BOT_TOKEN_HERE"
# guild_id = 111222333444555666
# channel_id = 666555444333222111

[blocklist]
patterns = [
  "rm\\s+-[a-z]*f[a-z]*r[a-z]*\\s+/",
  "sudo\\s+",
  ":\\(\\)\\{\\s*:\\|:\\&\\s*\\};:",
]

[streaming]
edit_throttle_ms = 2000
poll_interval_ms = 250
chunk_size = 4000
offline_buffer_max_bytes = 1048576
max_sessions = 10
```

<details>
<summary>How to set up a Discord bot</summary>

1. Go to [discord.com/developers/applications](https://discord.com/developers/applications), create a new application
2. **Bot section**: click "Reset Token" to generate a bot token, copy it
3. **Privileged Gateway Intents**: toggle ON **MESSAGE CONTENT INTENT** (required for reading message text). Also toggle ON **SERVER MEMBERS INTENT** if you plan to use guild channels
4. **Generate an invite URL**: go to OAuth2 > URL Generator. Select the **bot** scope (no `applications.commands` needed). Under Bot Permissions select: **View Channels**, **Send Messages**, **Attach Files**, **Read Message History**. Copy the generated URL and open it in your browser to invite the bot to your server
5. **DM-only mode**: omit `guild_id` and `channel_id` from the config. The bot will only respond to direct messages from the authorized user
6. **Guild channel mode**: set both `guild_id` and `channel_id`. The bot will respond in that channel AND in DMs

**How to get snowflake IDs:**
1. In Discord, go to Settings > Advanced > toggle ON **Developer Mode**
2. Right-click your username > **Copy User ID** (this is `discord_user_id`)
3. Right-click a server > **Copy Server ID** (this is `guild_id`)
4. Right-click a channel > **Copy Channel ID** (this is `channel_id`)
</details>

### Socket API only

```toml
# [auth] and [blocklist] are optional — omit them for socket-only deployments.
# Add [blocklist] if you want to forbid dangerous shell commands sent over the socket.

[socket]
enabled = true

[[socket.client]]
name = "my-agent"
token = "tk_live_your-32-character-minimum-secret"
```

The socket API is opt-in (`enabled = false` by default). Each client authenticates with a named Bearer token. See [Socket API](#socket-api) below for usage and [docs/socket.md](docs/socket.md) for the full wire protocol reference.

### OpenCode integration (optional)

Opencode runs as a subprocess -- terminus spawns `opencode run --format json` per prompt and inherits the user's opencode CLI config (default model, agent, provider, auth). No extra terminus config is required if `opencode` is on PATH and authenticated.

Optional overrides:

```toml
[harness.opencode]
# Override the opencode binary location (default: resolved via PATH)
# binary_path = "/usr/local/bin/opencode"

# Per-run model override (passed as `-m <model>` to opencode run)
# model = "openrouter/anthropic/claude-haiku-4-5"

# Per-run agent override (passed as `--agent <name>`). Use "build" for
# tool-use-enabled agents.
# agent = "build"
```

**Requirements:** `opencode` on PATH (same pattern as `tmux`). Run `opencode auth login` once before using any `: opencode ...` commands.

**Blocked from chat** (run in your terminal instead): `acp`, `agent`, `attach`, `auth`, `debug`, `github`, `import`, `login`, `logout`, `mcp`, `serve`, `session`, `tui`, `uninstall`, `upgrade`, `web`. Terminus returns a clear error if you try these from chat. Note: `session list` / `session ls` and `auth list` / `auth ls` ARE supported as safe read-only aliases.

**`--schema` is not supported.** Opencode's CLI has no schema-constrained output surface; passing `--schema` to `: opencode ...` returns a chat-safe redirect error pointing at the claude or codex harness. Use `: claude --schema=<registered-name> <prompt>` or `: codex --schema=<registered-name> <prompt>` for validated structured output with full webhook delivery. (Codex also accepts `--schema=<inline-or-path>` forms, but those validate the response against the schema without triggering webhook delivery — see footnote ⁶.)

See [docs/opencode.md](docs/opencode.md) for the full CLI reference.

### Gemini integration (optional)

Gemini runs as a subprocess -- terminus spawns `gemini -o stream-json [flags] <prompt>` per prompt and inherits gemini-cli's own defaults (auth, default model). No extra terminus config is required if `gemini` is on PATH and authenticated.

Optional overrides:

```toml
[harness.gemini]
# Override the gemini binary location (default: resolved via PATH)
# binary_path = "/usr/local/bin/gemini"

# Per-run model override. Aliases: "pro" | "flash" | "flash-lite"
# model = "flash"

# Per-run approval mode. Values: "default" | "auto_edit" | "yolo" | "plan"
# approval_mode = "default"
```

**Requirements:** `gemini` on PATH, already authenticated via gemini-cli's own config / OAuth flow.

**Blocked from chat** (all four are interactive or destructive; run in your terminal instead): `update`, `mcp`, `extensions`, `skills`. No chat-safe gemini subcommands are shipped yet -- a `--list-sessions` passthrough is a planned follow-up.

**Per-prompt flags:** `--name`, `--resume`, `--continue` (named or bare), `--model` / `-m`, `--approval-mode`. Opencode-only flags (`--title`, `--share`, `--pure`, `--fork`) are not supported by gemini and return a parse error. Attachments (images / files) are rejected with a chat-safe error -- multimodal threading is a follow-up.

**`--schema` is not supported.** Gemini-cli has no schema-constrained output surface; passing `--schema` to `: gemini ...` returns a chat-safe redirect error pointing at the claude or codex harness. Use `: claude --schema=<registered-name> <prompt>` or `: codex --schema=<registered-name> <prompt>` for validated structured output with full webhook delivery. (Codex also accepts `--schema=<inline-or-path>` forms, but those validate the response against the schema without triggering webhook delivery — see footnote ⁶.)

See [docs/gemini.md](docs/gemini.md) for the full CLI reference, event schema, error table, and functionality matrix.

### Codex integration (optional)

Codex runs as a subprocess -- terminus spawns `codex exec --json --full-auto --skip-git-repo-check [flags] <prompt>` per prompt (or `codex exec resume <thread_id>` for a named-session continuation) and inherits codex's own config (`~/.codex/config.toml`, profiles, ChatGPT/API auth). No extra terminus config is required if `codex` is on PATH and authenticated.

Optional overrides:

```toml
[harness.codex]
# Override the codex binary location (default: resolved via PATH)
# binary_path = "/opt/homebrew/bin/codex"

# Per-run model override. Default in codex 0.128: "gpt-5.5". Other models
# available: "gpt-5.4", "gpt-5.4-mini". ("gpt-5.3-codex" was removed in 0.128.)
# model = "gpt-5.5"

# Per-run profile override (passed as `-p / --profile <name>`). Selects a
# [profiles.<name>] block from ~/.codex/config.toml.
# profile = "default"

# Per-run sandbox policy. Values: "read-only" | "workspace-write" | "danger-full-access"
# sandbox = "workspace-write"

# Defense-in-depth: when true, terminus passes --ignore-user-config so codex
# skips ~/.codex/config.toml entirely. Useful if a future codex profile field
# could re-introduce TTY approval prompts. Default: false (respect user's profile).
# ignore_user_config = false
```

**Requirements:** `codex` on PATH (`brew install --cask codex` or `npm install -g @openai/codex`), authenticated via `codex login` (ChatGPT account or API key).

**Defaults applied unconditionally** by terminus regardless of these fields:
- `-s workspace-write` -- replaces the deprecated-in-0.128 `--full-auto`. `codex exec` is non-interactive by default in 0.128 (no `--ask-for-approval` flag exists on the exec subcommand). User-supplied `--sandbox` / `[harness.codex].sandbox` overrides this default.
- `--skip-git-repo-check` -- terminus's tmux cwd may not always be a git repo.
- `--ephemeral` -- added when no named/resumed session is in play, so one-shot prompts don't pollute codex's session log.
- `Stdio::null()` for child stdin -- `codex exec` reads stdin even when prompt is supplied as an arg (still true in 0.128), blocking forever otherwise.

**Blocked from chat** (interactive, destructive, or v1.1-deferred; run in your terminal instead): `login`, `logout`, `mcp`, `mcp-server`, `app`, `app-server`, `exec-server`, `plugin`, `completion`, `features`, `debug`, `sandbox` (subcommand form, distinct from `--sandbox` flag), `review`, `resume` (use `--resume <name>` flag), `fork`. **Now chat-safe (F6+F7):** `sessions`, `apply <task_id>`, and the `cloud {list,status,diff,apply,exec}` subgroup — see [docs/codex.md](docs/codex.md#chat-safe-subcommands).

**Per-prompt flags:** `--name`, `--resume`, `--continue` (named or bare), `--model` / `-m`, `--sandbox`, `--profile` (no `-p` short alias — collides with claude's `--permission-mode`), `--add-dir` / `-d` (repeatable), `--schema` (three forms — registered name from `[schemas.<name>]`, file path, or inline JSON; all passed as `--output-schema`. Registered names additionally drive webhook delivery with HMAC-SHA256-signed POST + retry queue, full parity with claude). Image attachments (`image/png`, `image/jpeg`, `image/jpg`, `image/webp`) are forwarded via codex's `-i` flag.

See [docs/codex.md](docs/codex.md) for the full CLI reference, verified event schema, error table, and functionality matrix.

### Multiple platforms

Include any combination of `[telegram]`, `[slack]`, `[discord]`, and `[socket]` sections. Any platform can be omitted -- terminus starts with whatever is configured.

### Sleep/wake management (optional)

By default, terminus holds an idle-sleep inhibitor whenever the lid is open (or the device has no lid) and the host is on AC. Drop a `[power]` section into `terminus.toml` to adjust:

```toml
[power]
# Enable the power-management subsystem (default: true).
# Set to false for CI/headless environments with no caffeinate/systemd-inhibit.
enabled = true

# Hold the inhibitor on battery power too (default: false = AC-only).
# Turn on only if you want real-time delivery while unplugged; it will drain
# the battery faster since the host can't idle-sleep.
stayawake_on_battery = false

# Override the state-file location (default: <terminus.toml dir>/terminus-state.json).
# This file stores the Telegram offset, chat bindings, last-seen-wall timestamp,
# and the clean-shutdown flag. It MUST be writable by the terminus process.
# state_file = "/absolute/path/to/terminus-state.json"
```

All fields are optional — an empty `[power]` section (or no section at all) uses the defaults above. Verify the inhibitor is live with:

- macOS: `pmset -g assertions | grep PreventUserIdleSystem`
- Linux: `systemd-inhibit --list | grep terminus`

### Command trigger (optional)

The default command prefix is `: ` (colon + space). Change it in `[commands]`:

```toml
[commands]
trigger = "!"   # Now use `! ls -la` instead of `: ls -la`
```

Allowed characters: `` : ! > ; . , @ ~ ^ - + = | % ? ``.

---

## Commands reference

All commands use the `: ` (colon + space) prefix:

### Session management

| Command | What it does |
|---------|-------------|
| `: new <name>` | Create a named terminal session |
| `: fg <name>` | Bring a session to the foreground |
| `: bg` | Background the current session |
| `: list` | Show all sessions with their status |
| `: kill <name>` | Destroy a session |
| `: screen` | Send a snapshot of the current terminal screen to chat |

Session names can contain letters, numbers, hyphens, and underscores (max 64 characters).

### Shell commands

| Command | What it does |
|---------|-------------|
| `: <command>` | Run in the foreground session (e.g., `: ls -la`) |
| *(plain text)* | Sent as stdin to the foreground session |

### Claude Code

| Command | What it does |
|---------|-------------|
| `: claude <prompt>` | One-shot prompt with structured response |
| `: claude on` | Enter interactive Claude mode |
| `: claude on [options]` | Enter Claude mode with CLI options (see below) |
| `: claude off` | Exit Claude mode, back to terminal |

In Claude mode, plain text goes to Claude instead of the terminal. Multi-turn -- each message continues the same conversation.

You can also send images (photos, screenshots, diagrams) in Claude mode. Attach a photo with an optional caption and Claude will see it. Photo-only messages default to "What is in this image?".

Uses your **Claude subscription** (Pro/Max), not API credits.

#### Claude mode options

Options passed to `: claude on` persist for the entire session (until `: claude off`):

| Option | Short | What it does |
|--------|-------|-------------|
| `--model <name>` | `-m` | Model override (e.g. `sonnet`, `opus`) |
| `--effort <level>` | `-e` | Thinking effort: `low`, `medium`, `high`, `max` |
| `--system-prompt <text>` | | Replace the default system prompt |
| `--append-system-prompt <text>` | | Append to the default system prompt |
| `--add-dir <path>` | `-d` | Add a directory for context (repeatable) |
| `--max-turns <n>` | `-t` | Limit agentic turns per prompt |
| `--name <name>` | `-n` | Name a Claude session for multi-turn resume (create-or-resume) |
| `--resume <name>` / `--continue <name>` | | Strict resume of a named session (error if not found) |
| `--settings <path>` | | Path to a Claude Code settings file or inline JSON |
| `--mcp-config <path>` | | Path to an MCP server config file |
| `--permission-mode <mode>` | `-p` | Permission mode: `default`, `acceptEdits`, `plan`, `bypassPermissions` (default: `bypassPermissions`) |

Quote values that contain spaces: `--system-prompt "You are a Rust expert"` or `--system-prompt 'Be concise'`. Smart/curly quotes from mobile keyboards are normalized automatically.

Paths (`--add-dir`, `--mcp-config`, `--settings`) are relative to terminus's working directory, not the terminal session's. Use absolute paths when in doubt.

Examples:

```
: claude on --model sonnet                          # use Sonnet model
: claude on --effort high --add-dir ../shared-lib   # deeper thinking + extra context
: claude on -m opus -t 10                           # Opus model, max 10 turns per prompt
: claude on -n auth                                 # interactive mode with named session "auth"
: claude --resume auth fix the login bug            # strict resume of session "auth"
: claude on --system-prompt "Focus on security"     # custom system prompt
: claude on --mcp-config ./mcp.json --settings ./s.json
: claude on -p acceptEdits                              # Claude can edit files but not run shell commands
```

### Two ways to use Claude

**SDK mode** (`: claude`) -- structured output, real-time tool activity, multi-turn, image input. Best for prompts and Q&A:

```
: claude explain the auth module
: claude on
What does the auth module do?
: claude off
```

**tmux mode** -- run Claude Code as a full interactive CLI session. Supports slash commands, skills, plugins, and interactive prompts that the SDK can't handle:

```
: new ai                         # create a tmux session
: claude                         # launches Claude Code CLI
```

Now Claude Code is running in the terminal. Use plain text to talk to it:

```
explore this project
/commit                          # Claude Code slash commands
/review-pr 42                    # works because it's a real terminal
```

Switch between Claude and other sessions:

```
: bg                             # background Claude session
: new build
: cargo test                     # run tests in a different session
: fg ai                          # back to Claude
continue where we left off
```

To check what Claude (or any program) is doing in a tmux session, use `: screen`:

```
: screen                         # sends a snapshot of the terminal to chat
```

This captures exactly what you'd see if you were looking at the terminal -- useful when Claude is working on a long task and you want a progress check.

Use tmux mode when you need Claude Code's full interactive features (slash commands, permission prompts, multi-file editing workflows). Use SDK mode when you want quick, clean answers with image support.

### Named sessions

By default, Claude conversation context is tied to the foreground terminal session name. Named sessions decouple this — you can create, name, and resume Claude conversations independently.

**Create or resume a named session (`--name` / `-n`):**

```
: claude --name auth-refactor explain the auth middleware     # creates "auth-refactor"
: claude --name auth-refactor now fix the JWT validation      # resumes "auth-refactor" (notifies you)
: claude on --name auth-refactor                              # interactive mode with "auth-refactor"
: claude on --name auth-refactor explain the auth middleware  # interactive mode AND send first prompt
```

If the session already exists, `--name` resumes it and shows a notification. If it doesn't exist, it creates a new one. With `on`, any text after the flags is sent as the first prompt.

**Strict resume (`--resume` / `--continue`):**

```
: claude --resume auth-refactor add rate limiting             # resumes, errors if not found
: claude --resume typo                                        # → "No session named 'typo'"
: claude on --resume review please look at the PR             # enter interactive mode, resume "review", send prompt
```

Use `--resume` when you know the session exists and want to catch typos. `--continue` is an alias for `--resume`.

**How it works:**
- Named sessions persist across restarts — the session index (name → session ID + working directory) is saved in `terminus-state.json`
- The Claude SDK stores conversation history in `.claude/` — terminus only tracks the mapping
- On resume, the stored working directory is passed to the SDK for context
- Sessions are LRU-evicted when the index exceeds `max_named_sessions` (default 50, configurable in `[harness]` section)
- `--name` and `--resume` are mutually exclusive
- Only works with harnesses that support resume (currently Claude, opencode, and Gemini)

**Breaking change:** `-n` was previously the short flag for `--max-turns`. It now means `--name`. Use `-t` for `--max-turns`.

---

## Structured Output (--schema)

terminus can instruct an AI harness to emit a validated JSON response that matches a JSON Schema you define, then optionally POST it to a webhook endpoint with HMAC-SHA256 authentication.

**Harness support:**

| Harness  | Schema validation | In-chat fenced JSON | Webhook delivery + retry queue |
|----------|:----:|:----:|:----:|
| claude   |  ✓   |  ✓   | ✓ (full pipeline below) |
| codex    |  ✓   |  ✓   | ✓ (registered names only; inline-JSON / file-path forms remain chat-only) |
| opencode |  ✗   |  ✗   | n/a |
| gemini   |  ✗   |  ✗   | n/a |

The codex harness reaches webhook parity with claude when `--schema=<name>` resolves against a `[schemas.<name>]` entry in `terminus.toml`. Inline-JSON and file-path forms (`--schema='{...}'` / `--schema=path/to/schema.json`) remain chat-only — they validate against the schema but never feed the delivery queue, because the security model (HMAC secret env var) is registry-driven.

### Why

- **Type-safe pipeline**: Claude produces JSON that your code can parse directly without post-processing.
- **Webhook delivery**: results flow into your automation stack (Zapier, n8n, custom microservices, databases) without polling.
- **Write-ahead durability**: the job is written to disk before the first network attempt. Transient failures are retried automatically with exponential backoff.

### Setup

1. Define a named schema in `terminus.toml`:

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

# Optional: deliver to a webhook.
webhook = "https://your-server.example.com/webhooks/todos"
webhook_secret_env = "TODOS_WEBHOOK_SECRET"
```

2. Set the HMAC secret (if using a webhook):

```sh
export TODOS_WEBHOOK_SECRET="$(openssl rand -hex 32)"
```

3. Use it:

```
: harness --schema todos list my open tasks
```

Claude's response is rendered as a JSON code block in chat and, if a webhook is configured, POSTed to your endpoint immediately. On transient failure the job is queued for automatic retry.

### Webhook request

```
POST https://your-server.example.com/webhooks/todos
Content-Type: application/json
X-Terminus-Schema: todos
X-Terminus-Run-Id: 01J8...   (ULID, unique per run)
X-Terminus-Timestamp: 1713193920
X-Terminus-Signature: v1=<hmac-sha256-hex>

{"todos":[{"title":"Write tests","done":false}]}
```

The signature covers `"<timestamp>.<raw_body>"` (Stripe-style), which binds the timestamp into the MAC so neither field can be altered without invalidating the signature.

### Verifying in Python

```python
import hmac, hashlib, time

def verify(secret: str, body: bytes, timestamp: str, signature: str) -> bool:
    # Reject replays older than 5 minutes.
    if abs(time.time() - int(timestamp)) > 300:
        return False
    payload = f"{timestamp}.".encode() + body
    expected = "v1=" + hmac.new(secret.encode(), payload, hashlib.sha256).hexdigest()
    return hmac.compare_digest(expected, signature)
```

### Verifying in Node.js

```js
const crypto = require("crypto");

function verify(secret, body, timestamp, signature) {
  // Reject replays older than 5 minutes.
  if (Math.abs(Date.now() / 1000 - parseInt(timestamp)) > 300) return false;
  const payload = `${timestamp}.${body}`;
  const expected = "v1=" + crypto.createHmac("sha256", secret).update(payload).digest("hex");
  const expectedBuf = Buffer.from(expected);
  const signatureBuf = Buffer.from(signature);
  // timingSafeEqual requires equal-length buffers; length mismatch => invalid.
  if (expectedBuf.length !== signatureBuf.length) return false;
  return crypto.timingSafeEqual(expectedBuf, signatureBuf);
}
```

### Retry behaviour

| Attempt | Delay (±20% jitter) |
|---------|---------------------|
| 1       | 1s                  |
| 2       | 2s                  |
| 3       | 4s                  |
| 4       | 8s                  |
| 5       | 16s                 |
| 6       | 32s                 |
| 7+      | 60s (cap)           |

Jobs survive restarts -- they are stored in `<queue_dir>/pending/` and picked up by the retry worker on startup. Set `structured_output.max_retry_age_hours` in `terminus.toml` to cap how long terminus will retry before abandoning a job (0 = forever, the default).

---

## Socket API

The socket API lets programs, scripts, and agents drive terminus over WebSocket. It supports every command available in chat, with typed JSON envelopes, request pipelining, and live event subscriptions.

**Transport:** Plain `ws://` (deploy behind nginx/caddy/Traefik for `wss://` TLS termination).
**Auth:** Bearer token in the `Authorization` header at the HTTP upgrade.
**Protocol:** `terminus/v1` -- one JSON envelope per WebSocket text message.

### Quick start

1. Add to `terminus.toml`:

```toml
[socket]
enabled = true

[[socket.client]]
name = "my-agent"
token = "tk_live_your-32-character-minimum-secret"
```

2. Connect and send a command:

```bash
websocat ws://127.0.0.1:7645 -H "Authorization: Bearer tk_live_your-32-character-minimum-secret"
```

```json
{"type":"request","request_id":"r1","command":": new build"}
```

The server responds with a sequence of typed frames:

```json
{"type":"ack","request_id":"r1","accepted_at":"2026-04-15T12:00:01Z"}
{"type":"result","request_id":"r1","value":{"text":"Session 'build' created"},"cancelled":false}
{"type":"end","request_id":"r1"}
```

### Per-request lifecycle

Every request follows this frame sequence:

```
request → ack → [text_chunk | tool_call | partial_result]* → result | error → end
```

- **`ack`** -- server accepted the request
- **`result`** -- terminal success with the command's output
- **`error`** -- terminal failure with an error code
- **`end`** -- sentinel marking the response is complete

Multiple requests can be pipelined without waiting for results (FIFO per connection).

### Subscriptions

Subscribe to ambient events (session lifecycle, structured output, chat messages) with facet filters:

```json
{"type":"subscribe","subscription_id":"s1","filter":{
  "event_types":["session_output","structured_output"],
  "sessions":["build"]
}}
```

Events arrive as they happen:

```json
{"type":"event","subscription_id":"s1","event":{
  "type":"session_output","session":"build","chunk":"cargo test\n   Compiling..."
}}
```

Filter facets: `event_types`, `schemas`, `sessions`. OR within a facet, AND across facets. Empty filter = receive everything. Up to 8 named subscriptions per connection.

<details>
<summary>Available event types</summary>

| Event type | What it delivers |
|---|---|
| `structured_output` | Claude `--schema` results (schema, value, run_id) |
| `webhook_status` | Webhook delivery attempts (Delivered/Abandoned/Error) |
| `queue_drained` | Webhook queue drain cycle complete |
| `session_output` | Terminal output from tmux sessions |
| `session_created` | New session created (`: new`) |
| `session_killed` | Session destroyed (`: kill`) |
| `session_limit_reached` | Max session cap hit |
| `session_started` | Session foregrounded (`: fg`) |
| `session_exited` | Session process exited |
| `chat_forward` | Message received from a chat platform |
| `harness_started` | Claude turn started |
| `harness_finished` | Claude turn completed |
| `gap_banner` | Sleep/wake gap detected |

</details>

### Rate limiting and backpressure

Each client has a per-client token bucket (survives reconnects):

| Control | Default |
|---|---|
| Burst capacity | 60 requests |
| Sustained rate | 20 requests/sec |
| Max pending requests | 32 per connection |
| Max connections | 16 total |
| Idle timeout | 300s |
| Max message size | 1 MiB |

Rate-limited requests receive an `error` with `code: "rate_limited"` and a machine-readable `retry_after_ms` field.

### Proxy setup for TLS

terminus listens on plain `ws://`. For production, terminate TLS at a reverse proxy:

**Caddy** (automatic TLS):
```caddyfile
terminus.example.com {
    reverse_proxy 127.0.0.1:7645
}
```

**nginx:**
```nginx
location / {
    proxy_pass http://127.0.0.1:7645;
    proxy_http_version 1.1;
    proxy_set_header Upgrade $http_upgrade;
    proxy_set_header Connection "upgrade";
    proxy_read_timeout 3600s;
}
```

### Full reference

See [docs/socket.md](docs/socket.md) for the complete wire protocol specification, all envelope types, error codes, and configuration details.

---

## How it works

### Terminal output

terminus uses `tmux capture-pane` to read the rendered terminal screen -- no raw byte streaming, no ANSI escape stripping. Output is diffed against the previous snapshot so only new content is delivered.

- Output arrives as new messages in Telegram (no edit-in-place accumulation)
- Long output is automatically split at ~4000 chars (within Telegram's 4096 limit)
- Slack output goes into per-session threads for clean separation

### Claude Code integration

The `: claude` command uses the `claude-agent-sdk-rust` crate, which calls the Claude CLI with `--output-format stream-json`. This gives:

- **Structured typed output** -- no terminal scraping
- **Real-time tool activity** -- see what Claude is doing as it works:
  ```
  🧠 Thinking
  📖 Read src/main.rs
  🔎 Grep "TODO"
  ✏️ Edit src/buffer.rs
  💻 Bash cargo test
  🤖 Agent "investigate auth module"
  ```
- **Multi-turn sessions** -- conversation state preserved via `--resume`
- **Image input** -- send photos from Telegram/Slack, Claude receives them as `@/path` mentions
- **File output** -- Claude-created files (images, PDFs, CSVs, etc.) are automatically delivered back to chat
- **5-minute timeout** -- long-running prompts time out with a clear error

### Output file delivery

When Claude creates or writes files during a prompt, terminus automatically delivers qualifying files back to chat:

- **Images** (png, jpg, gif, webp, svg, bmp) -- sent as native photo previews
- **Documents** (pdf, csv, xlsx, docx, pptx) -- sent as file attachments
- **Text/data** (md, txt, json, yaml, toml, xml, html) -- sent as file attachments

Files are detected from Write/Edit tool calls, Bash output redirects (`-o`, `>`, `--output`), and a post-prompt scan of the working directory and `/tmp`. Sensitive files (`terminus.toml`, `.env`, `credentials*`, `secret*`, `token*`, `password*`, `private_key*`) are never delivered. Max file size: 50 MB.

### Session persistence

When terminus shuts down (Ctrl+C), tmux sessions keep running. On restart, terminus automatically reconnects to surviving `term-*` sessions. Send any command (e.g., `: list`) to re-bind the chat delivery.

---

## Security

### Authentication

**Chat platforms:** Single-user only. Messages from any user ID other than the configured one are **silently ignored** -- the bot does not reveal its existence to unauthorized users.

**Socket API:** Per-client Bearer tokens (`Authorization: Bearer <token>`) validated at the HTTP upgrade with constant-time comparison. Missing or invalid tokens receive HTTP 401 before a WebSocket is established. Tokens must be at least 32 characters; the `Debug` impl redacts them from logs. Token comparison is timing-safe to prevent side-channel enumeration.

### Command blocklist

Dangerous commands are blocked by regex patterns in `terminus.toml`. Both `: ` prefixed commands AND plain text input (including text routed to Claude) are checked. The defaults block:

| Pattern | Blocks |
|---------|--------|
| `rm\s+-[a-z]*f[a-z]*r[a-z]*\s+/` | Recursive force-delete from root (any flag order) |
| `sudo\s+` | Privilege escalation |
| `:\(\)\{\s*:\|:\&\s*\};:` | Fork bomb |
| `mkfs\.` | Filesystem format |
| `dd\s+if=` | Raw disk write |

### Evasion prevention

Commands are normalized before matching to prevent common evasion techniques:

- **Path prefix stripping** -- `/usr/bin/sudo reboot` is caught as `sudo reboot`
- **Backslash removal** -- `su\do reboot` is caught as `sudo reboot`
- **Flag reordering** -- `rm -fr /`, `rm -rf /`, and `rm -r -f /` all normalize to the same form
- **Space collapsing** -- extra whitespace between tokens is collapsed

Multi-line messages are rejected to prevent newline injection bypasses.

Add your own patterns:

```toml
[blocklist]
patterns = [
  "rm\\s+-[a-z]*f[a-z]*r[a-z]*\\s+/",
  "sudo\\s+",
  "shutdown",
  "reboot",
]
```

### Output file safety

Files delivered from Claude are restricted:

- Only allowlisted extensions (images, documents, data files)
- Files must be under the working directory or `/tmp` (path traversal blocked)
- Sensitive filenames are never delivered
- Max 50 MB per file

### Smart quote normalization

Mobile keyboards often replace `"straight quotes"` with `"curly quotes"`. terminus automatically normalizes these so shell commands work correctly.

---

## Configuration reference

| Key | Type | Description |
|-----|------|-------------|
| `auth.telegram_user_id` | integer | Your Telegram numeric user ID |
| `auth.slack_user_id` | string | Your Slack member ID |
| `auth.discord_user_id` | integer | Your Discord user snowflake |
| `telegram.bot_token` | string | Telegram Bot API token |
| `slack.bot_token` | string | Slack bot token (`xoxb-`) |
| `slack.app_token` | string | Slack app token for Socket Mode (`xapp-`) |
| `slack.channel_id` | string | Slack channel to operate in |
| `discord.bot_token` | string | Discord bot token |
| `discord.guild_id` | integer | Discord server snowflake (optional; omit for DM-only) |
| `discord.channel_id` | integer | Discord channel snowflake (optional; requires `guild_id`) |
| `blocklist.patterns` | string[] | Regex patterns to block |
| `streaming.edit_throttle_ms` | integer | Min ms between message edits (default: 2000) |
| `streaming.poll_interval_ms` | integer | Terminal output poll interval (default: 250) |
| `streaming.chunk_size` | integer | Max chars per message (default: 4000) |
| `streaming.offline_buffer_max_bytes` | integer | Max offline buffer (default: 1048576) |
| `streaming.max_sessions` | integer | Max concurrent terminal sessions (default: 10) |
| `commands.trigger` | char | Command prefix character (default: `:`). Must be one of `: ! > ; . , @ ~ ^ - + = \| % ?` |
| `power.enabled` | bool | Enable the power-management subsystem (default: true). Set false for CI / headless |
| `power.stayawake_on_battery` | bool | Prevent idle sleep on battery power too (default: false; AC-only) |
| `power.state_file` | string | Override for the persisted-state JSON file (default: adjacent to `terminus.toml`) |
| `socket.enabled` | bool | Enable WebSocket API (default: false) |
| `socket.bind` | string | Bind address (default: `127.0.0.1`; use `0.0.0.0` for containers) |
| `socket.port` | integer | Listener port (default: 7645) |
| `socket.max_connections` | integer | Max concurrent WebSocket connections (default: 16) |
| `socket.max_subscriptions_per_connection` | integer | Named subscriptions per connection (default: 8) |
| `socket.max_pending_requests` | integer | Pipelined request queue depth (default: 32) |
| `socket.rate_limit_per_second` | float | Token bucket refill rate (default: 20.0) |
| `socket.rate_limit_burst` | float | Token bucket capacity (default: 60.0) |
| `socket.max_message_bytes` | integer | Max inbound message size (default: 1048576) |
| `socket.ping_interval_secs` | integer | Server ping interval (default: 30) |
| `socket.pong_timeout_secs` | integer | Close if pong not received (default: 10) |
| `socket.idle_timeout_secs` | integer | Close if no inbound activity (default: 300) |
| `socket.shutdown_drain_secs` | integer | Grace period on shutdown (default: 30) |
| `socket.client[].name` | string | Client display name (unique) |
| `socket.client[].token` | string | Bearer token (min 32 characters) |

Override the config file path with `TERMINUS_CONFIG=/path/to/file.toml`.

### Sleep/wake behavior

terminus holds a platform-appropriate idle-sleep assertion while the lid is
open (or the device has no lid) and the host is on AC. On macOS this is a
supervised `caffeinate -i` child; on Linux it's `systemd-inhibit --what=idle:sleep`.
Closed-lid sleep is **never** blocked — macOS clamshell and Linux
lid-close suspend continue to work normally.

When the host does sleep (lid close, forced suspend, overnight), terminus
detects the gap on wake via monotonic/wall-clock divergence (>30s) and emits
a one-time banner per active chat:

```
⏸ paused at 02:13, resumed at 07:45 (gap: 5h 32m)
```

Adapter polling/handling is paused until each banner is confirmed delivered
(per-chat oneshot ack, 5s timeout); then the backlog drains. Telegram queues
updates server-side and drains them in `update_id` order on resume. Slack
pauses its Socket Mode WebSocket loop and, after the banner is acked, fetches
every message sent during sleep via `conversations.history` since a per-channel
`last_seen_ts` watermark; the watermark is force-persisted to
`terminus-state.json` so even a multi-hour sleep does not lose messages, and a
ring-buffered dedup window prevents double-delivery against any in-flight
Socket Mode reconnect. Discord uses a handler-gate (gateway events during the
pause window are discarded -- the pause is typically <5s and the `: ` command
protocol is self-recoverable). If a banner fails to deliver within the timeout
(e.g., rate-limit or network blip), terminus falls back to prepending
`[gap: Xm Ys] ` inline to the first outbound message for that chat so the gap
is never silently hidden.

The Telegram offset and chat bindings persist atomically to
`terminus-state.json` (adjacent to `terminus.toml` by default). A restart
during sleep still delivers a banner and drains the backlog, guarded by a
compound gate: wall-gap > 30s **AND** the previous shutdown was unclean
(so `cargo build`-and-restart cycles don't fire spurious banners).

Verify the assertion with:

- macOS: `pmset -g assertions | grep PreventUserIdleSystem`
- Linux: `systemd-inhibit --list | grep terminus`

---

## Architecture

```
src/
  main.rs              Core event loop (tokio::select!, biased priority order)
  app.rs               Application state, command dispatch, sleep/wake handling,
                       StateStore owner
  delivery.rs          Per-platform delivery tasks, gap-banner rendering,
                       inline-prefix fallback, StreamEvent dispatch
  config.rs            TOML config with validation
  command.rs           Command parser + blocklist + evasion normalization
  session.rs           Session manager (foreground/background state machine)
  tmux.rs              tmux CLI wrapper (capture-pane, send-keys, smart quotes)
  buffer.rs            Output diffing via scrollback line tracking; StreamEvent enum
  state_store.rs       Atomic JSON persistence (Telegram offset, chat bindings,
                       last_seen_wall, last_clean_shutdown) — owned by App only
  power/               Cross-platform sleep-inhibit + gap detection
    mod.rs             PowerManager async-trait + submod registration
    types.rs           LidState, PowerSource, PowerEvent, PowerSignal
    policy.rs          Pure desired_inhibit() policy function
    gap_detector.rs    SystemTime vs Instant divergence → PowerSignal::GapDetected
    supervisor.rs      Periodic lid/power poller; applies policy, calls set_inhibit
    macos.rs           caffeinate -i child + ioreg/pmset reads (hybrid ADR)
    linux.rs           systemd-inhibit child + sysfs/procfs reads
    fake.rs            Test double for OS-agnostic unit tests
  harness/
    mod.rs             Harness trait, event types, streaming driver
    claude.rs          Claude Code SDK integration (streaming, images, file delivery)
    opencode.rs        OpenCode CLI subprocess harness (JSON event stream, multi-step)
    gemini.rs          Gemini CLI subprocess harness (stream-json, shared tool-pairing buffer)
    codex.rs           Codex CLI subprocess harness (NDJSON --json events, paired
                       item.started/item.completed via shared ToolPairingBuffer,
                       --full-auto + --skip-git-repo-check unconditional)
  chat_adapters/
    mod.rs             ChatPlatform trait + Attachment type
    telegram.rs        Telegram adapter (teloxide, long-polling)
    slack.rs           Slack adapter (Socket Mode, tokio-tungstenite)
    discord.rs         Discord adapter (serenity, gateway + handler-gate pause)
  socket/
    mod.rs             WebSocket server (TcpListener, Bearer auth, per-connection spawn)
    connection.rs      Per-connection task (select! loop, pipelining, subscriptions)
    envelope.rs        Wire protocol types (terminus/v1 JSON envelopes)
    events.rs          Ambient event types for subscription bus
    rate_limit.rs      Per-client token bucket rate limiter
    subscription.rs    Subscription registry with facet filter matching
```

```
Telegram/Slack/Discord          WebSocket clients
    |                               |
    v                               v
cmd_tx (mpsc) ──────────> tokio::select! core loop (main.rs)
                            ├── handle_command() (app.rs)
                            │   ├── tmux send-keys (shell commands)
                            │   └── harness driver (Claude SDK stream-json)
                            │       ├── tool events ──> chat / socket
                            │       ├── text response ──> chat / socket
                            │       └── output files ──> chat (photos/documents)
                            ├── health_check (5s) ──> tmux has-session
                            ├── poll_tick (250ms) ──> tmux capture-pane
                            └── ctrl_c ──> socket drain ──> cleanup
                                   |
                            broadcast::channel (StreamEvent + AmbientEvent)
                                   |
                            ├── Telegram delivery task
                            ├── Slack delivery task
                            ├── Discord delivery task
                            └── Socket per-connection tasks (subscription filtering)
```

The harness system is extensible via the `Harness` trait. Claude, OpenCode, Gemini, and Codex are all fully implemented. Each harness supports streaming events (`ToolUse`, `Text`, `File`, `Done`, `Error`) and optional multi-turn session resume. The gemini and codex harnesses share a `ToolPairingBuffer` (in `harness/mod.rs`) that coalesces paired tool-start + tool-result events keyed by id; the claude SDK and opencode CLI emit atomic tool events and don't need pairing.

---

## Troubleshooting

<details>
<summary>"No active session"</summary>

Create a session first: `: new <name>`. If you just restarted, sessions auto-reconnect -- send `: list` to check.
</details>

<details>
<summary>No output after restart</summary>

Reconnected sessions need a chat binding. Send `: list` or `: fg <name>` first -- this tells terminus which chat to deliver to.
</details>

<details>
<summary>No output appearing at all</summary>

Check that tmux is installed (`tmux -V`). Try `: new test` then `: echo hello`.
</details>

<details>
<summary>Telegram rate limit errors</summary>

Increase `streaming.edit_throttle_ms` in the config. Telegram allows ~30 edits/min.
</details>

<details>
<summary>Slack not connecting</summary>

Verify Socket Mode is enabled and that the `xapp-` token is correct. Check that you subscribed to `message.channels` events.
</details>

<details>
<summary>Claude commands not working</summary>

The Claude CLI must be installed and authenticated: `npm i -g @anthropic-ai/claude-code && claude login`. Verify with `claude -p "hello"` in your terminal.
</details>

<details>
<summary>Smart quotes causing shell errors</summary>

terminus normalizes curly quotes automatically. If you still see issues, check that you're running the latest build.
</details>

<details>
<summary>Images not working with Claude</summary>

Images are only supported in harness mode. Enter Claude mode first with `: claude on`, then send a photo. Sending images to the terminal (without an active harness) will show an error.
</details>

<details>
<summary>OpenCode commands not working</summary>

Opencode must be installed and authenticated: install from [opencode.ai](https://opencode.ai) and run `opencode auth login`. Verify with `opencode models` in your terminal. If terminus can't find it, set `binary_path` in `[harness.opencode]`. Blocked subcommands (auth login/logout, serve, web, etc.) must be run in your terminal, not via chat.
</details>

<details>
<summary>Gemini commands not working</summary>

Gemini CLI must be installed from [github.com/google-gemini/gemini-cli](https://github.com/google-gemini/gemini-cli) and already authenticated (via its own OAuth / config flow -- terminus does not proxy credentials). Verify with `gemini --help` in your terminal. If terminus can't find it, set `binary_path` in `[harness.gemini]`. Interactive gemini subcommands (`update`, `mcp`, `extensions`, `skills`) are blocked from chat -- run them in your terminal. Use `--approval-mode yolo` (per-prompt or in config) if you want gemini to execute tools without prompting.
</details>

<details>
<summary>Codex commands not working</summary>

Codex CLI must be installed (`brew install --cask codex` or `npm install -g @openai/codex`) and authenticated via `codex login` (ChatGPT account or API key). Verify with `codex --version` and `codex login status` in your terminal. If terminus can't find it, set `binary_path` in `[harness.codex]`. Verified against codex-cli **0.128.0** -- newer versions may emit different event schemas (terminus logs a "version drift" warning on unrecognized event types).
</details>

<details>
<summary>Codex: "model not supported"</summary>

Codex 0.128's default model is `gpt-5.5` and it is universally available, including on ChatGPT-account auth. If you set a `model = "..."` override that codex doesn't recognize, the call will surface codex's own error. Valid choices include `gpt-5.5` (default), `gpt-5.4`, `gpt-5.4-mini`. (`gpt-5.3-codex` was removed in 0.128 — if you have it pinned in `[harness.codex] model`, switch to `gpt-5.5` or remove the field to inherit the codex default.)
</details>

<details>
<summary>Codex: blocked subcommand error</summary>

All native codex subcommands are blocked from chat in v1 (`login`, `logout`, `mcp`, `cloud`, `apply`, `plugin`, `debug`, `app`, `sessions`, `resume` (bare), `fork`, etc.). Each returns a targeted chat-safe error pointing you at the right path. For named-session resume use the `--resume <name>` flag (e.g. `: codex --resume auth keep going`), not the `: codex resume` subcommand. Run management subcommands in your terminal directly.
</details>

<details>
<summary>"Maximum session limit reached"</summary>

The default limit is 10 concurrent sessions. Kill unused sessions with `: kill <name>` or increase `streaming.max_sessions` in the config.
</details>

<details>
<summary>Socket: connection refused</summary>

Check `[socket] enabled = true` in `terminus.toml`, verify at least one `[[socket.client]]` is configured, and confirm the bind address and port (`ss -tlnp | grep 7645`).
</details>

<details>
<summary>Socket: 401 Unauthorized</summary>

Verify the `Authorization: Bearer <token>` header is present and the token matches a `[[socket.client]]` entry exactly (minimum 32 characters).
</details>

<details>
<summary>Socket: "lagged" warnings</summary>

The broadcast buffer overflowed because the client is consuming events too slowly. Events were dropped. Increase `socket.send_buffer_size` or process events faster. This is non-fatal; the connection continues.
</details>

---

## tmux compatibility

terminus works with any `base-index` or `pane-base-index` setting. Sessions are targeted by name (`term-build`), never by numeric index.

---

## License

MIT
