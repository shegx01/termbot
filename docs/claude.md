# claude harness — CLI reference

The claude harness wraps Anthropic's Claude Code via the `claude-agent-sdk-rust` crate, which calls the `claude` CLI with `--output-format stream-json`. Each prompt spawns a short-lived subprocess, streams typed events back to chat in real time, and exits cleanly. The harness powers the `: claude` SDK surface; for raw interactive Claude Code (slash commands, skills, plugins) you can also launch the CLI directly inside a tmux session — see [Two ways to use Claude](#two-ways-to-use-claude). For setup and onboarding, see [README.md](../README.md#requirements).

Uses your **Claude subscription** (Pro/Max), not API credits.

## Contents

- [Invocation modes](#invocation-modes)
- [Per-prompt flags](#per-prompt-flags)
- [Examples](#examples)
- [Two ways to use Claude](#two-ways-to-use-claude)
- [Named sessions](#named-sessions)
- [Image input](#image-input)
- [Output file delivery](#output-file-delivery)
- [Tool activity](#tool-activity)
- [Structured output](#structured-output)
- [Timeouts](#timeouts)
- [Troubleshooting](#troubleshooting)

---

## Invocation modes

### One-shot

```
: claude <prompt>
```

Single-turn prompt. Streams structured events (text, tool calls, file outputs) back to chat. No session is persisted unless `--name` is passed.

### Interactive toggle

```
: claude on [flags]
```

Plain text from the user routes to Claude until `: claude off`. All flags work with the `on` form and apply for the lifetime of the interactive session.

```
: claude off
```

Leave interactive mode; subsequent plain text goes back to the foreground tmux session.

In Claude mode, plain text goes to Claude instead of the terminal. Multi-turn — each message continues the same conversation.

---

## Per-prompt flags

All flags work in both one-shot (`: claude --name foo <prompt>`) and on-toggle (`: claude on --name foo`) forms. In interactive mode, any text after the flags is sent as the first prompt.

| Flag | Short | Value | What it does |
|------|-------|-------|--------------|
| `--model <name>` | `-m` | string | Model override (e.g. `sonnet`, `opus`) |
| `--effort <level>` | `-e` | string | Thinking effort: `low`, `medium`, `high`, `max` |
| `--system-prompt <text>` | | string | Replace the default system prompt |
| `--append-system-prompt <text>` | | string | Append to the default system prompt |
| `--add-dir <path>` | `-d` | path (repeatable) | Add a directory for context |
| `--max-turns <n>` | `-t` | integer | Limit agentic turns per prompt |
| `--name <name>` | `-n` | string | Name a Claude session for multi-turn resume (create-or-resume) |
| `--resume <name>` | | string | Strict resume of a named session (errors if not found) |
| `--continue <name>` | | string | Alias for `--resume <name>` |
| `--settings <path>` | | path or inline JSON | Path to a Claude Code settings file or inline JSON |
| `--mcp-config <path>` | | path | Path to an MCP server config file |
| `--permission-mode <mode>` | `-p` | string | Permission mode: `default`, `acceptEdits`, `plan`, `bypassPermissions` (default: `bypassPermissions`) |
| `--schema <name>` | | string | Validated structured output. See [Structured Output](../README.md#structured-output---schema) |

### Notes

- Quote values that contain spaces: `--system-prompt "You are a Rust expert"` or `--system-prompt 'Be concise'`. Smart/curly quotes from mobile keyboards are normalized automatically.
- Paths (`--add-dir`, `--mcp-config`, `--settings`) are relative to terminus's working directory, not the terminal session's. Use absolute paths when in doubt.
- `--name` and `--resume` are mutually exclusive.

### Mobile keyboard normalization

Em-dash (U+2014) in any flag position is normalized to `--` at the top of the harness dispatch block. This applies to all harnesses (claude, opencode, codex, gemini). En-dash (U+2013) is intentionally NOT normalized — it appears in legitimate prose (e.g., page ranges `5–10`).

### Breaking change

`-n` was previously the short flag for `--max-turns`. It now means `--name`. Use `-t` for `--max-turns`.

---

## Examples

```
: claude on --model sonnet                          # use Sonnet model
: claude on --effort high --add-dir ../shared-lib   # deeper thinking + extra context
: claude on -m opus -t 10                           # Opus model, max 10 turns per prompt
: claude on -n auth                                 # interactive mode with named session "auth"
: claude --resume auth fix the login bug            # strict resume of session "auth"
: claude on --system-prompt "Focus on security"     # custom system prompt
: claude on --mcp-config ./mcp.json --settings ./s.json
: claude on -p acceptEdits                          # Claude can edit files but not run shell commands
```

---

## Two ways to use Claude

**SDK mode** (`: claude`) — structured output, real-time tool activity, multi-turn, image input. Best for prompts and Q&A:

```
: claude explain the auth module
: claude on
What does the auth module do?
: claude off
```

**tmux mode** — run Claude Code as a full interactive CLI session. Supports slash commands, skills, plugins, and interactive prompts that the SDK can't handle:

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

This captures exactly what you'd see if you were looking at the terminal — useful when Claude is working on a long task and you want a progress check.

Use tmux mode when you need Claude Code's full interactive features (slash commands, permission prompts, multi-file editing workflows). Use SDK mode when you want quick, clean answers with image support.

---

## Named sessions

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

- Named sessions persist across restarts — the session index (name → session ID + working directory) is saved in `terminus-state.json` under the `claude:<name>` prefix.
- The Claude SDK stores conversation history in `.claude/` — terminus only tracks the mapping.
- On resume, the stored working directory is passed to the SDK for context.
- Sessions are LRU-evicted when the index exceeds `max_named_sessions` (default 50, configurable in the `[harness]` section of `terminus.toml`). The cap is shared across Claude / opencode / gemini / codex.
- Session names follow the same rules as terminal session names (alphanumeric, hyphens, underscores, max 64 chars).

---

## Image input

Send photos (screenshots, diagrams) in Claude mode. Attach a photo with an optional caption and Claude will see it. Photo-only messages default to "What is in this image?".

```
: claude on
[send a screenshot with caption] what's wrong with this error?
[send a diagram]                 # photo-only messages work too
: claude off
```

Images are forwarded to the SDK as `@/path` mentions; Claude processes them as multimodal input. Sending images to the terminal (without an active harness) returns a chat-safe error.

---

## Output file delivery

When Claude creates or writes files during a prompt, terminus automatically delivers qualifying files back to chat:

- **Images** (png, jpg, gif, webp, svg, bmp) — sent as native photo previews
- **Documents** (pdf, csv, xlsx, docx, pptx) — sent as file attachments
- **Text/data** (md, txt, json, yaml, toml, xml, html) — sent as file attachments

Files are detected from Write/Edit tool calls, Bash output redirects (`-o`, `>`, `--output`), and a post-prompt scan of the working directory and `/tmp`.

### Safety

- Only allowlisted extensions (images, documents, data files) are delivered.
- Files must be under the working directory or `/tmp` (path traversal blocked).
- Sensitive filenames are never delivered: `terminus.toml`, `.env`, `credentials*`, `secret*`, `token*`, `password*`, `private_key*`.
- Max 50 MB per file.

---

## Tool activity

The SDK streams typed events for Claude's tool calls. terminus translates these into one chat line per call, in real time:

```
🧠 Thinking
📖 Read src/main.rs
🔎 Grep "TODO"
✏️ Edit src/buffer.rs
💻 Bash cargo test
🤖 Agent "investigate auth module"
```

This is "structured typed output" — no terminal scraping or ANSI parsing. Multi-turn sessions preserve the conversation state via `--resume` (handled internally by the named-session machinery).

---

## Structured output

Claude has full support for `--schema` — both schema validation and webhook delivery with HMAC-SHA256 signing and a retry queue. See [Structured Output (`--schema`)](../README.md#structured-output---schema) in the README for the full pipeline (named schemas in `terminus.toml`, webhook URL + secret, signature format, retry behavior).

```
: claude --schema todos list my open tasks
```

---

## Timeouts

- **5-minute timeout** per prompt — long-running prompts time out with a clear error. Use `--max-turns` to constrain agentic loops.

---

## Troubleshooting

### Claude commands not working

The Claude CLI must be installed and authenticated:

```bash
npm i -g @anthropic-ai/claude-code
claude login
```

Verify with `claude -p "hello"` in your terminal.

### Smart quotes causing shell errors

terminus normalizes curly quotes (e.g. `"…"`, `'…'`) to ASCII automatically before forwarding to the shell or to Claude. If you still see issues, check that you're running the latest build.

### Images not working with Claude

Images are only supported in harness mode. Enter Claude mode first with `: claude on`, then send a photo. Sending images to the terminal (without an active harness) will show an error.

### Session-state persistence failures

Named-session persistence is atomic: in-memory mutations happen only after the state worker accepts the batch (5-second timeout). If the state worker is stalled or its channel is closed, the prompt's chat output gains a `Session-state persistence failed; this prompt's named session won't survive a restart` line so the failure is visible rather than silent.
