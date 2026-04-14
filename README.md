# termbot

Control your terminal from Telegram, Slack, or Discord. Built in Rust.

termbot gives you remote access to terminal sessions and Claude Code from your phone. Run shell commands, manage tmux sessions, send images, and have multi-turn AI conversations -- all through Telegram, Slack, or Discord.

---

## Quick start

### One-line install (recommended)

The installer downloads the binary, walks you through configuration, installs dependencies, and sets up a system service with auto-restart and update notifications.

```bash
curl -sSL https://raw.githubusercontent.com/shegx01/termbot/main/install.sh | bash
```

That's it. Open Telegram, Slack, or Discord and start typing.

<details>
<summary>What the installer does</summary>

1. Detects your OS and architecture (macOS/Linux, x86_64/ARM64)
2. Checks for tmux (offers to install if missing) and Claude Code CLI (required)
3. Walks you through Telegram/Slack configuration with inline help
4. Downloads the correct binary to `~/.local/bin/termbot`
5. Writes config to `~/.config/termbot/termbot.toml`
6. Registers a system service (systemd on Linux, launchd on macOS) with auto-restart
7. Sets up a daily update check with OS-native notifications
8. Starts termbot

**Flags:**
- `--quick` — skip inline help text during setup
- `--upgrade` — download latest binary, restart service (config untouched)
- `--uninstall` — stop service, remove files (config preserved by default)

```bash
# Upgrade
curl -sSL https://raw.githubusercontent.com/shegx01/termbot/main/install.sh | bash -s -- --upgrade

# Uninstall
curl -sSL https://raw.githubusercontent.com/shegx01/termbot/main/install.sh | bash -s -- --uninstall
```
</details>

### Download a pre-built binary

Pre-built binaries are also available directly on the [Releases](https://github.com/shegx01/termbot/releases/latest) page if you prefer manual setup.

```bash
# Download for your platform (macOS Apple Silicon shown)
curl -L -o termbot \
  https://github.com/shegx01/termbot/releases/latest/download/termbot-aarch64-apple-darwin

chmod +x termbot
cp termbot.example.toml termbot.toml   # edit with your tokens
./termbot
```

### Build from source

```bash
git clone https://github.com/shegx01/termbot.git
cd termbot
cp termbot.example.toml termbot.toml   # edit with your tokens
cargo build --release
./target/release/termbot
```

To use a config file at a custom path, set `TERMBOT_CONFIG`:

```bash
TERMBOT_CONFIG=/path/to/config.toml ./termbot
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
: claude on -m opus -n 10 --add-dir ../api
```

**Image support** -- send photos to Claude directly from chat:

```
: claude on
[send a screenshot with caption] what's wrong with this error?
[send a diagram]                 # photo-only messages work too
: claude off
```

---

## Requirements

- **Rust 1.70+** (for building)
- **tmux** (for terminal sessions)
- **Claude Code CLI** (for `: claude` commands -- `npm i -g @anthropic-ai/claude-code`)
- At least one of: Telegram bot token, Slack bot + app tokens, Discord bot token

---

## Configuration

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

**Permissions:** The default bot token grants everything termbot needs (receive messages, send messages, edit messages). No extra config required.
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

### Multiple platforms

Include any combination of `[telegram]`, `[slack]`, and `[discord]` sections with their corresponding IDs in `[auth]`. Any platform can be omitted -- termbot starts with whatever is configured.

### Sleep/wake management (optional)

By default, termbot holds an idle-sleep inhibitor whenever the lid is open (or the device has no lid) and the host is on AC. Drop a `[power]` section into `termbot.toml` to adjust:

```toml
[power]
# Enable the power-management subsystem (default: true).
# Set to false for CI/headless environments with no caffeinate/systemd-inhibit.
enabled = true

# Hold the inhibitor on battery power too (default: false = AC-only).
# Turn on only if you want real-time delivery while unplugged; it will drain
# the battery faster since the host can't idle-sleep.
stayawake_on_battery = false

# Override the state-file location (default: <termbot.toml dir>/termbot-state.json).
# This file stores the Telegram offset, chat bindings, last-seen-wall timestamp,
# and the clean-shutdown flag. It MUST be writable by the termbot process.
# state_file = "/absolute/path/to/termbot-state.json"
```

All fields are optional — an empty `[power]` section (or no section at all) uses the defaults above. Verify the inhibitor is live with:

- macOS: `pmset -g assertions | grep PreventUserIdleSystem`
- Linux: `systemd-inhibit --list | grep termbot`

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
| `--max-turns <n>` | `-n` | Limit agentic turns per prompt |
| `--settings <path>` | | Path to a Claude Code settings file or inline JSON |
| `--mcp-config <path>` | | Path to an MCP server config file |
| `--permission-mode <mode>` | `-p` | Permission mode: `default`, `acceptEdits`, `plan`, `bypassPermissions` (default: `bypassPermissions`) |

Quote values that contain spaces: `--system-prompt "You are a Rust expert"` or `--system-prompt 'Be concise'`. Smart/curly quotes from mobile keyboards are normalized automatically.

Paths (`--add-dir`, `--mcp-config`, `--settings`) are relative to termbot's working directory, not the terminal session's. Use absolute paths when in doubt.

Examples:

```
: claude on --model sonnet                          # use Sonnet model
: claude on --effort high --add-dir ../shared-lib   # deeper thinking + extra context
: claude on -m opus -n 10                           # Opus model, max 10 turns per prompt
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

---

## How it works

### Terminal output

termbot uses `tmux capture-pane` to read the rendered terminal screen -- no raw byte streaming, no ANSI escape stripping. Output is diffed against the previous snapshot so only new content is delivered.

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

When Claude creates or writes files during a prompt, termbot automatically delivers qualifying files back to chat:

- **Images** (png, jpg, gif, webp, svg, bmp) -- sent as native photo previews
- **Documents** (pdf, csv, xlsx, docx, pptx) -- sent as file attachments
- **Text/data** (md, txt, json, yaml, toml, xml, html) -- sent as file attachments

Files are detected from Write/Edit tool calls, Bash output redirects (`-o`, `>`, `--output`), and a post-prompt scan of the working directory and `/tmp`. Sensitive files (`termbot.toml`, `.env`, `credentials*`, `secret*`, `token*`, `password*`, `private_key*`) are never delivered. Max file size: 50 MB.

### Session persistence

When termbot shuts down (Ctrl+C), tmux sessions keep running. On restart, termbot automatically reconnects to surviving `tb-*` sessions. Send any command (e.g., `: list`) to re-bind the chat delivery.

---

## Security

### Authentication

Single-user only. Messages from any user ID other than the configured one are **silently ignored** -- the bot does not reveal its existence to unauthorized users.

### Command blocklist

Dangerous commands are blocked by regex patterns in `termbot.toml`. Both `: ` prefixed commands AND plain text input (including text routed to Claude) are checked. The defaults block:

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

Mobile keyboards often replace `"straight quotes"` with `"curly quotes"`. termbot automatically normalizes these so shell commands work correctly.

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
| `power.state_file` | string | Override for the persisted-state JSON file (default: adjacent to `termbot.toml`) |

Override the config file path with `TERMBOT_CONFIG=/path/to/file.toml`.

### Sleep/wake behavior

termbot holds a platform-appropriate idle-sleep assertion while the lid is
open (or the device has no lid) and the host is on AC. On macOS this is a
supervised `caffeinate -i` child; on Linux it's `systemd-inhibit --what=idle:sleep`.
Closed-lid sleep is **never** blocked — macOS clamshell and Linux
lid-close suspend continue to work normally.

When the host does sleep (lid close, forced suspend, overnight), termbot
detects the gap on wake via monotonic/wall-clock divergence (>30s) and emits
a one-time banner per active chat:

```
⏸ paused at 02:13, resumed at 07:45 (gap: 5h 32m)
```

Adapter polling/handling is paused until each banner is confirmed delivered
(per-chat oneshot ack, 5s timeout); then the backlog drains. Telegram queues
updates server-side and drains them in `update_id` order on resume. Discord
uses a handler-gate (gateway events during the pause window are discarded --
the pause is typically <5s and the `: ` command protocol is self-recoverable).
If a banner fails to deliver within the timeout (e.g., rate-limit or network
blip), termbot falls back to prepending `[gap: Xm Ys] ` inline to the first
outbound message for that chat so the gap is never silently hidden.

The Telegram offset and chat bindings persist atomically to
`termbot-state.json` (adjacent to `termbot.toml` by default). A restart
during sleep still delivers a banner and drains the backlog, guarded by a
compound gate: wall-gap > 30s **AND** the previous shutdown was unclean
(so `cargo build`-and-restart cycles don't fire spurious banners).

Verify the assertion with:

- macOS: `pmset -g assertions | grep PreventUserIdleSystem`
- Linux: `systemd-inhibit --list | grep termbot`

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
    gemini.rs          Gemini harness (planned)
    codex.rs           Codex harness (planned)
  chat_adapters/
    mod.rs             ChatPlatform trait + Attachment type
    telegram.rs        Telegram adapter (teloxide, long-polling)
    slack.rs           Slack adapter (Socket Mode, tokio-tungstenite)
    discord.rs         Discord adapter (serenity, gateway + handler-gate pause)
```

```
Telegram/Slack/Discord
    |
    v
cmd_tx (mpsc) ──> tokio::select! core loop (main.rs)
                    ├── handle_command() (app.rs)
                    │   ├── tmux send-keys (shell commands)
                    │   └── harness driver (Claude SDK stream-json)
                    │       ├── tool events ──> chat
                    │       ├── text response ──> chat
                    │       └── output files ──> chat (photos/documents)
                    ├── health_check (5s) ──> tmux has-session
                    ├── poll_tick (250ms) ──> tmux capture-pane
                    └── ctrl_c ──> cleanup (detach, sessions survive)
                           |
                    broadcast::channel
                           |
                    ├── Telegram delivery task
                    ├── Slack delivery task
                    └── Discord delivery task
```

The harness system is extensible via the `Harness` trait. Currently only Claude is implemented; Gemini and Codex have stubs ready for future integration. Each harness supports streaming events (`ToolUse`, `Text`, `File`, `Done`, `Error`) and optional multi-turn session resume.

---

## Troubleshooting

<details>
<summary>"No active session"</summary>

Create a session first: `: new <name>`. If you just restarted, sessions auto-reconnect -- send `: list` to check.
</details>

<details>
<summary>No output after restart</summary>

Reconnected sessions need a chat binding. Send `: list` or `: fg <name>` first -- this tells termbot which chat to deliver to.
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

termbot normalizes curly quotes automatically. If you still see issues, check that you're running the latest build.
</details>

<details>
<summary>Images not working with Claude</summary>

Images are only supported in harness mode. Enter Claude mode first with `: claude on`, then send a photo. Sending images to the terminal (without an active harness) will show an error.
</details>

<details>
<summary>"Maximum session limit reached"</summary>

The default limit is 10 concurrent sessions. Kill unused sessions with `: kill <name>` or increase `streaming.max_sessions` in the config.
</details>

---

## tmux compatibility

termbot works with any `base-index` or `pane-base-index` setting. Sessions are targeted by name (`tb-build`), never by numeric index.

---

## License

MIT
