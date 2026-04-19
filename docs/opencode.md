# opencode harness — CLI reference

The opencode harness wraps the `opencode` CLI as a short-lived subprocess per prompt. Each invocation spawns `opencode run --format json`, streams JSON events from stdout back to chat in real time, and exits cleanly with no persistent sidecar or port binding. For setup and onboarding, see [README.md](../README.md#opencode-integration-optional).

## Contents

- [Invocation modes](#invocation-modes)
- [Per-prompt flags](#per-prompt-flags)
- [Subcommands (safe, read-only)](#subcommands-safe-read-only)
- [Blocked subcommands](#blocked-subcommands)
- [Configuration](#configuration)
- [Error messages](#error-messages)
- [Mobile keyboard normalization](#mobile-keyboard-normalization)
- [Testing](#testing)

---

## Invocation modes

### One-shot

```
: opencode <prompt>
```

Spawns `opencode run --format json`, streams stdout back to chat. No session is persisted unless `--name` is passed.

### Interactive toggle

```
: opencode on [flags]
```

Plain text from the user routes to opencode until `: opencode off`. All flags work with the `on` form and apply for the lifetime of the interactive session.

```
: opencode off
```

Leave interactive mode; subsequent plain text goes back to the terminal.

### Subcommand

```
: opencode <subcommand> [args]
```

Executes an opencode CLI subcommand and surfaces stdout to chat. See [Subcommands](#subcommands-safe-read-only) below.

---

## Per-prompt flags

All flags work in both one-shot (`: opencode --name foo <prompt>`) and on-toggle (`: opencode on --name foo`) forms. In interactive mode, any text after the flags is sent as the first prompt.

| Flag | Value | What it does | Maps to opencode CLI |
|---|---|---|---|
| `--name <x>` | string | Create-or-resume named session (internally prefixed `opencode:<x>`). Resumes if found, creates if not | `--session <stored-id>` (after upsert) |
| `--resume <x>` | string | Strict resume — errors if session `opencode:<x>` is not found. Catches typos and LRU-evicted sessions | `--session <stored-id>` |
| `--continue <x>` | string | Alias for `--resume <x>` | same |
| `--continue` | (bare) | Continue opencode's last session (no name stored in terminus) | `--continue` |
| `-m <x>` / `--model <x>` | string | Override model for this prompt; overrides `[harness.opencode] model` | `-m <x>` |
| `--agent <x>` | string | Override agent for this prompt. Use `build` for tool-use-enabled prompts | `--agent <x>` |
| `--title <x>` | string | Human-readable session title | `--title <x>` |
| `--share` | (bare) | Ask opencode for a shareable URL for this run | `--share` |
| `--pure` | (bare) | Disable external plugins for this run | `--pure` |
| `--fork` | (bare) | Fork the session before continuing. Requires `--continue` or `--resume` | `--fork` |
| `-t <n>` / `--max-turns <n>` | integer | Cap the turn count | `-t <n>` |
| `--schema=<name>` | string | **Not supported.** Returns an error directing to the claude harness | — |

### Mutual exclusion rules

- `--name <x>` together with `--resume <x>` / `--continue <x>` (named form) → error: `"cannot use both --name and --resume/--continue"`
- `--fork` without `--continue` (bare or named) or `--resume` → error: `"--fork requires --continue or --resume"`
- Bare `--continue` (no value) combined with a trailing `--name` or `--resume` → error

---

## Subcommands (safe, read-only)

All subcommands spawn `opencode <sub> [args]` with `NO_COLOR=1`, capture stdout, strip ANSI escapes, and deliver via the same pipeline as prompts. Output is chunked automatically for Telegram's 4096-character limit.

| Chat syntax | Native opencode form | What it does |
|---|---|---|
| `: opencode models [provider]` | `opencode models [provider]` | List configured models; optional provider filter (e.g., `openrouter`) |
| `: opencode stats [--days N] [--tools] [--models] [--project]` | `opencode stats ...` | Token usage and cost |
| `: opencode sessions [--max-count N]` | `opencode session list [--max-count N]` | Recent session IDs |
| `: opencode session list` | `opencode session list` | Same (native form accepted) |
| `: opencode session ls` | `opencode session list` | Same (native alias) |
| `: opencode providers` | `opencode auth list` | List configured providers |
| `: opencode auth list` | `opencode auth list` | Same (native form) |
| `: opencode auth ls` | `opencode auth list` | Same (native alias) |
| `: opencode export <sessionID>` | `opencode export <sessionID>` | Dump session as JSON (`<sessionID>` is required; rejected at parse time if omitted) |

### Bounds

- 30-second wall-clock timeout per subcommand (not per prompt)
- stderr capped at 64 KiB before being sanitized for chat display
- If stdout is empty at exit 0, surfaces `"opencode <sub> <args>: no results"` rather than silence

---

## Blocked subcommands

These are rejected at parse time with a chat-safe error message:

> `opencode <sub> is not available from chat — run it in your terminal. Safe chat subcommands: models, stats, sessions, providers, export.`

| Command | Why blocked | Alternative |
|---|---|---|
| `uninstall` | Destructive | Run `opencode uninstall` in terminal |
| `upgrade` | Version change requires terminus restart | Run in terminal, then restart terminus |
| `auth` (bare) | Ambiguous (login/logout/list) | Use `: opencode providers` or `: opencode auth list` |
| `login` / `logout` | Interactive credential flow | Run `opencode auth login` in terminal |
| `session` (bare) | Ambiguous | Use `: opencode sessions` or `: opencode session list` |
| `serve` / `web` / `acp` | Daemon processes | Run in terminal if needed; terminus manages its own opencode spawns |
| `attach` | Interactive TTY | Run in terminal |
| `import` | File-path semantics ambiguous over chat | Run in terminal |
| `mcp` | Interactive config flow | Run in terminal |
| `agent` | Interactive wizard for `agent create` | Use `--agent <name>` flag for agent selection |
| `github` | Interactive OAuth flow | Run in terminal |
| `debug` | Debug-only; noisy output | Run in terminal |
| `tui` | Default TUI mode | Incompatible with chat interface |

---

## Configuration

The `[harness.opencode]` block in `terminus.toml` is entirely optional. If omitted, terminus inherits opencode's own CLI config (default model, agent, provider, auth credentials).

```toml
[harness.opencode]
# Override the opencode binary location (default: resolved via PATH).
# binary_path = "/usr/local/bin/opencode"

# Default model; per-prompt `--model <x>` or `-m <x>` overrides this.
# Example: "openrouter/anthropic/claude-haiku-4-5".
# model = "..."

# Default agent; per-prompt `--agent <x>` overrides this.
# Use "build" for tool-use-enabled workflows.
# agent = "..."
```

### Runtime requirements

- `opencode` must be on `PATH` (or `binary_path` must point to it) — parallel to the `tmux must be on PATH` constraint
- `opencode auth login` must have been run once; terminus does not proxy credentials
- Default model must be configured via opencode's own auth or agent flow

---

## Error messages

| Error | Meaning | User action |
|---|---|---|
| `opencode binary not found: <path> (set [harness.opencode] binary_path or install opencode on PATH)` | Spawn failed with ENOENT | Install opencode or fix `binary_path` in config |
| `opencode spawn failed: <err>` | Other spawn failure | Check binary permissions and path |
| `opencode: stdout pipe missing` | Child's stdout handle failed | Internal error; retry the prompt |
| `opencode: no output for 5 minutes — killing subprocess` | Per-line silence timeout | Retry; possibly a long-running model response |
| `opencode exited with status <n>: <sanitized stderr>` | Non-zero exit | stderr is sanitized (env-var assignments and `/Users/<name>/` / `/home/<name>/` paths are redacted); check `RUST_LOG=debug` for raw stderr |
| `opencode: no response content` | Exit 0, zero recognized events | Model returned nothing; check provider quota |
| `opencode: no recognized events received (version drift — check opencode --version)` | Exit 0, only unknown events | Upgrade or downgrade opencode to a compatible version |
| `opencode: event schema mismatch (version drift — check opencode --version)` | Recognized event type but required field missing | opencode's JSON shape changed; report to terminus |
| `opencode does not support --schema. Try: : claude --schema=<name> <prompt>` | `--schema` flag passed to opencode harness | Use the claude harness for structured output |
| `opencode <sub> is not available from chat — run it in your terminal. Safe chat subcommands: models, stats, sessions, providers, export.` | Blocked subcommand invoked | Use a safe subcommand or run the native command in terminal |

---

## Mobile keyboard normalization

Em-dash (U+2014) in any flag position is normalized to `--` at the top of the harness dispatch block. This applies to all harnesses (claude, opencode, codex, gemini).

```
: opencode —name review hello
```

Parses identically to:

```
: opencode --name review hello
```

En-dash (U+2013) is intentionally NOT normalized — it appears in legitimate prose (e.g., page ranges `5–10`).

---

## Testing

See [docs/integration-tests.md](integration-tests.md) for how to run gated opencode integration tests.

Five `#[ignore]` tests exist in `src/harness/opencode.rs::tests`:

- `ac1_one_shot_haiku_streams_and_completes`
- `ac2_interactive_two_prompts_reuse_session`
- `ac3_bogus_session_id_surfaces_error_path`
- `ac4_tool_use_visibility_with_agent_build`
- `subcommand_models_returns_non_empty_output`

Run with:

```bash
TERMINUS_HAS_OPENCODE=1 cargo test --release -- --ignored --test-threads=1
```

Preconditions: `opencode` on PATH and authenticated.
