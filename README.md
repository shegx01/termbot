# termbot

Control your terminal from Telegram or Slack. Built in Rust.

termbot lets you manage tmux sessions on your machine from a chat app. Send commands, stream output in real time, and interact with tools like Claude Code -- all from your phone.

## How it works

```
You (Telegram/Slack)          termbot              tmux
    ": ls -la"       ──────>  parse + route  ────> send-keys
                     <──────  stream output  <──── pipe-pane
    "drwxr-xr-x ..."
```

- `: ` prefix for commands and shell input
- Plain text (no prefix) forwarded as stdin to the active session
- One foreground session at a time, others run in the background
- Output streams live via message edits (short output) or chunked messages (long output)
- Telegram: flat chat. Slack: one thread per session.

## Requirements

- Rust 1.70+
- tmux (installed and on PATH)
- At least one of: Telegram Bot token, Slack Bot + App tokens

## Installation

```bash
git clone <this-repo>
cd termbot
cargo build --release
```

The binary is at `target/release/termbot`.

## Configuration

Copy the example config and fill in your tokens:

```bash
cp termbot.example.toml termbot.toml
```

`termbot.toml` is git-ignored since it contains API secrets.

### Telegram only

```toml
[auth]
telegram_user_id = 123456789  # your Telegram numeric user ID

[telegram]
bot_token = "7012345678:AAH..."

[blocklist]
patterns = [
  "rm\\s+-rf\\s+/",
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
```

**Getting your Telegram bot token:**

1. Message [@BotFather](https://t.me/BotFather) on Telegram
2. Send `/newbot` and follow the prompts
3. Copy the token it gives you

**Telegram bot permissions:**

No special permissions need to be toggled. The default bot token grants everything termbot needs:

| Capability | Used for | Granted by default |
|---|---|---|
| Receive text messages | Reading your commands | Yes |
| Send messages | Delivering output | Yes |
| Edit own messages | Streaming output (edit-in-place) | Yes |
| Delete own messages | Not currently used, but available | Yes |

If you disabled **Group Privacy** mode via BotFather (`/setprivacy` > Disable), the bot can read messages in groups. For termbot this is unnecessary -- use a direct 1-on-1 chat with the bot.

**Getting your Telegram user ID:**

1. Message [@userinfobot](https://t.me/userinfobot) on Telegram
2. It replies with your numeric ID

### Slack only

```toml
[auth]
slack_user_id = "U01ABCDEF"  # your Slack member ID

[slack]
bot_token = "xoxb-..."
app_token = "xapp-..."
channel_id = "C01ABCDEF"

[blocklist]
patterns = [
  "rm\\s+-rf\\s+/",
  "sudo\\s+",
  ":\\(\\)\\{\\s*:\\|:\\&\\s*\\};:",
]

[streaming]
edit_throttle_ms = 2000
poll_interval_ms = 250
chunk_size = 4000
offline_buffer_max_bytes = 1048576
```

**Setting up a Slack app:**

1. Go to [api.slack.com/apps](https://api.slack.com/apps) and create a new app (choose "From scratch")
2. Under **Socket Mode**, enable it and generate an app-level token with the `connections:write` scope (`xapp-...`)
3. Under **OAuth & Permissions**, add the required bot token scopes (see table below)
4. Install the app to your workspace and copy the bot token (`xoxb-...`)
5. Under **Event Subscriptions**, enable events and subscribe to the required events (see table below)
6. Invite the bot to your channel (`/invite @botname`)
7. Get the channel ID: right-click the channel name > "View channel details" > copy the ID at the bottom

**Slack bot token scopes** (OAuth & Permissions > Bot Token Scopes):

| Scope | Used for | Required |
|---|---|---|
| `chat:write` | Sending messages and streaming output | Yes |
| `chat:write.customize` | Not used -- skip unless you want custom bot display names | No |
| `channels:history` | Reading messages in public channels the bot is in | Yes |
| `channels:read` | Listing channels (used internally by Socket Mode) | Yes |
| `groups:history` | Reading messages in private channels | Only if you use a private channel |
| `groups:read` | Listing private channels | Only if you use a private channel |
| `im:history` | Reading direct messages | Only if you DM the bot directly |
| `im:read` | Listing DM conversations | Only if you DM the bot directly |

**Minimum for a public channel:** `chat:write` + `channels:history` + `channels:read`

**Slack app-level token scope** (Socket Mode > App-Level Token):

| Scope | Used for | Required |
|---|---|---|
| `connections:write` | Opening the WebSocket connection for Socket Mode | Yes |

**Slack event subscriptions** (Event Subscriptions > Subscribe to bot events):

| Event | Used for | Required |
|---|---|---|
| `message.channels` | Receiving messages in public channels | Yes |
| `message.groups` | Receiving messages in private channels | Only if you use a private channel |
| `message.im` | Receiving direct messages | Only if you DM the bot directly |

**Getting your Slack member ID:**

Click your profile picture in Slack > "Profile" > three dots menu > "Copy member ID"

### Both platforms

Include both `[telegram]` and `[slack]` sections along with both IDs in `[auth]`:

```toml
[auth]
telegram_user_id = 123456789
slack_user_id = "U01ABCDEF"

[telegram]
bot_token = "7012345678:AAH..."

[slack]
bot_token = "xoxb-..."
app_token = "xapp-..."
channel_id = "C01ABCDEF"

[blocklist]
patterns = [
  "rm\\s+-rf\\s+/",
  "sudo\\s+",
  ":\\(\\)\\{\\s*:\\|:\\&\\s*\\};:",
]

[streaming]
edit_throttle_ms = 2000
poll_interval_ms = 250
chunk_size = 4000
offline_buffer_max_bytes = 1048576
```

## Usage

Start the bot:

```bash
cargo run
# or
./target/release/termbot
```

Then send messages from your configured chat app.

### Commands

All commands use the `: ` (colon + space) prefix:

| Command | Description |
|---------|-------------|
| `: new <name>` | Create a named terminal session |
| `: fg <name>` | Bring a session to the foreground |
| `: bg` | Background the current session |
| `: list` | Show all sessions with status |
| `: kill <name>` | Destroy a session |
| `: claude <prompt>` | One-shot Claude Code prompt (structured, no terminal scraping) |
| `: claude on` | Enter interactive Claude mode (plain text goes to Claude) |
| `: claude off` | Exit Claude mode (plain text goes back to terminal) |
| `: <command>` | Run a shell command in the foreground session |
| *(plain text)* | Send as stdin to the foreground session (or to Claude in Claude mode) |

### Example: Run a build

```
: new build
: cargo build --release
```

Output streams into chat as it happens.

### Example: One-shot Claude prompt

```
: claude explain the main.rs file
: claude what are the open TODOs in this project?
```

Clean structured response delivered directly — no terminal scraping.

### Example: Interactive Claude mode

```
: claude on
```

Now just type naturally:

```
What does the buffer module do?
Can you refactor it for clarity?
Show me the test coverage gaps.
```

Each message continues the same Claude conversation (multi-turn via session resume). Switch back to terminal mode:

```
: claude off
```

### Example: Use Claude Code interactively

```
: new claude
: claude
```

Wait for Claude Code to start, then just type naturally (no prefix needed):

```
What files are in this project?
```

Claude's responses stream back in real time. Switch away and back:

```
: bg
: new other-task
: ls -la
: fg claude
Can you explain the main.rs file?
```

### Example: Multiple sessions

```
: new server
: cargo run
: bg
: new logs
: tail -f /var/log/syslog
: bg
: list
```

Output:

```
  server [background] (uptime: 120s)
  logs [background] (uptime: 45s)
```

Foreground any session with `: fg server`.

### Example: Restart and reconnect

If termbot is restarted while tmux sessions are still running, it automatically reconnects to them on startup:

```
2026-04-11T12:30:00Z  INFO termbot: Found 2 existing tmux session(s), reconnecting...
2026-04-11T12:30:00Z  INFO termbot: Reconnected session 'server'
2026-04-11T12:30:00Z  INFO termbot: Reconnected session 'logs'
2026-04-11T12:30:00Z  INFO termbot: Foreground session: 'server'
```

The first reconnected session becomes the foreground. After restart, send any command to re-establish the chat binding:

```
: list
```

This tells termbot which chat to deliver output to. Then commands work normally:

```
: fg logs
: echo "I'm back"
```

Without sending a command first, reconnected sessions capture output but have nowhere to deliver it (termbot doesn't persist chat IDs across restarts).

## Streaming behavior

- **Short output** (<4000 chars): a single message is sent and edited in-place as output arrives
- **Long output** (>4000 chars): the message gets "[continued below...]" appended, then new messages are sent for each chunk
- **Edit throttle**: messages are edited at most once every 2 seconds (configurable) to stay within Telegram/Slack rate limits
- **Offline buffering**: if the bot loses network connectivity, output accumulates in memory and replays as a catch-up message when the connection returns
- **Slack threads**: each session gets its own thread for clean separation

## Security

termbot is a single-user tool. Only messages from the configured user ID are processed; all others are silently ignored.

### Command blocklist

Dangerous commands are blocked by regex patterns in `termbot.toml`. The default patterns block:

- `rm -rf /` (recursive delete from root)
- `sudo` (privilege escalation)
- `:(){ :|:& };:` (fork bomb -- escaped as `:\\(\\)\\{\\s*:\\|:\\&\\s*\\};:` in regex)
- `mkfs.` (filesystem format)
- `dd if=` (raw disk write)

Add your own patterns:

```toml
[blocklist]
patterns = [
  "rm\\s+-rf\\s+/",
  "sudo\\s+",
  "shutdown",
  "reboot",
  "my-custom-pattern",
]
```

Blocked commands return an error message and are never sent to the terminal.

## Configuration reference

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `auth.telegram_user_id` | integer | -- | Your Telegram numeric user ID |
| `auth.slack_user_id` | string | -- | Your Slack member ID |
| `telegram.bot_token` | string | -- | Telegram Bot API token |
| `slack.bot_token` | string | -- | Slack bot token (xoxb-) |
| `slack.app_token` | string | -- | Slack app token for Socket Mode (xapp-) |
| `slack.channel_id` | string | -- | Slack channel to operate in |
| `blocklist.patterns` | string[] | -- | Regex patterns to block |
| `streaming.edit_throttle_ms` | integer | 2000 | Minimum ms between message edits |
| `streaming.poll_interval_ms` | integer | 250 | How often to check for new output |
| `streaming.chunk_size` | integer | 4000 | Max chars per message before chunking |
| `streaming.offline_buffer_max_bytes` | integer | 1048576 | Max offline buffer size (1MB) |

## Architecture

```
src/
  main.rs          Core event loop (tokio::select!)
  config.rs        TOML config loading and validation
  command.rs       Command parser + blocklist
  session.rs       Session manager (foreground/background state machine)
  tmux.rs          tmux CLI wrapper (pipe-pane lifecycle)
  buffer.rs        Output buffer and streaming pipeline
  platform/
    mod.rs         ChatPlatform trait
    telegram.rs    Telegram adapter (teloxide)
    slack.rs       Slack adapter (Socket Mode via tokio-tungstenite)
```

The core loop uses `tokio::select!` over four branches:

1. **Incoming commands** from chat platforms (via mpsc channel)
2. **Health check** timer (detects crashed tmux sessions every 5s)
3. **Output poll** timer (reads new output from pipe-pane files every 250ms)
4. **SIGINT/SIGTERM** for graceful shutdown

Output events are broadcast to all active platform adapters via `tokio::sync::broadcast`.

## Graceful shutdown and restart

Press Ctrl+C or send SIGTERM. termbot will:

- Stop all pipe-pane connections
- Clean up temporary output files
- Leave tmux sessions running (they survive the restart)

### Restart behavior

On next startup, termbot automatically:

1. Cleans stale output files from previous runs
2. Scans for surviving `tb-*` tmux sessions
3. Reconnects to each one (re-attaches pipe-pane for output capture)
4. Sets the first found session as the foreground

**Important:** After restart, you must send at least one command (e.g., `: list` or `: fg <name>`) to bind the session to your chat. Until then, output is captured but has no chat destination for delivery. This is because termbot does not persist chat IDs to disk -- the binding is re-established on the first message you send.

## Troubleshooting

**"No active session"** -- You need to create a session first with `: new <name>`. If you just restarted termbot and had sessions running, they should auto-reconnect. Send `: list` to check, then `: fg <name>` to foreground one.

**No output after restart** -- Reconnected sessions need a chat binding. Send `: list` or `: fg <name>` first -- this tells termbot which chat to deliver output to.

**No output appearing** -- Check that tmux is installed (`tmux -V`) and that `/tmp/termbot-*.out` files are being created.

**Rate limit errors** -- Increase `streaming.edit_throttle_ms` in the config. Telegram allows ~30 edits/min.

**Slack not connecting** -- Verify Socket Mode is enabled for your app and that the `xapp-` token is correct. Check that you subscribed to `message.channels` events.

**Telegram "Function or operation not implemented"** -- This was a TLS backend issue on some systems. termbot ships with `rustls` (pure Rust TLS) to avoid it. If you see this error on a custom build, make sure you are not switching the TLS features back to `native-tls` in `Cargo.toml`.

**tmux "can't find window 0"** -- Your tmux config likely has `set -g base-index 1`. termbot targets sessions by name without hardcoding a window index, so this should not happen on current versions. If you see it, update to the latest termbot.

## tmux compatibility

termbot works with any `base-index` or `pane-base-index` setting. Sessions are always targeted by name (e.g. `tb-build`), never by numeric window or pane index.

## License

MIT
