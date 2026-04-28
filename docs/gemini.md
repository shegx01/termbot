# gemini harness — CLI reference

The gemini harness wraps Google's `gemini` CLI (`github.com/google-gemini/gemini-cli`) as a short-lived subprocess per prompt. Each invocation spawns `gemini -o stream-json [flags] <prompt>`, streams newline-delimited JSON events from stdout back to chat in real time, and exits cleanly with no persistent sidecar or port binding. Mirrors the `opencode` harness architectural pattern but uses gemini-cli's own idioms.

## Contents

- [Functionality matrix](#functionality-matrix)
- [Invocation modes](#invocation-modes)
- [Per-prompt flags](#per-prompt-flags)
- [Subcommands (safe, read-only)](#subcommands-safe-read-only)
- [Blocked subcommands](#blocked-subcommands)
- [Event schema](#event-schema)
- [Configuration](#configuration)
- [Error messages](#error-messages)
- [Mobile keyboard normalization](#mobile-keyboard-normalization)
- [Testing](#testing)
- [Known limitations](#known-limitations)

---

## Functionality matrix

Status legend: **Working** = shipped and tested · **Partial** = implemented with a documented gap · **Not shipped** = deliberate non-goal or deferred.

| Feature | Status | Notes |
|---|---|---|
| **Invocation** |||
| One-shot (`: gemini <prompt>`) | Working | Non-interactive; each prompt spawns a fresh `gemini -o stream-json` subprocess |
| Interactive toggle (`: gemini on` / `: gemini off`) | Working | Plain text routes to gemini between the two toggles |
| Gemini chat-safe subcommands | Working | `: gemini sessions` → `gemini --list-sessions`; `: gemini extensions` → `gemini --list-extensions`. Both are read-only flag-based surfaces. All other native subcommands (`mcp`, `skills`, `hooks`, `update`, `extensions <sub>`) remain blocked — see [Blocked subcommands](#blocked-subcommands) |
| **Per-prompt flags** |||
| `--name <x>` (create-or-resume) | Working | Upsert; internally prefixed `gemini:<x>` in session index |
| `--resume <x>` / `--continue <x>` (strict resume) | Working | Errors if the named session isn't in the index |
| Bare `--continue` (continue last) | Working | Translates to `gemini -r latest` |
| `--model <x>` / `-m <x>` | Working | Aliases: `pro`, `flash`, `flash-lite`. Per-prompt wins over `[harness.gemini].model` |
| `--approval-mode <x>` | Working | Values: `default`, `auto_edit`, `yolo`, `plan`. Per-prompt wins over `[harness.gemini].approval_mode` |
| `--schema <name>` | Working (error-redirect) | Returns `"gemini does not support --schema. Try: \`: claude --schema=<name> <prompt>\`"` |
| `--title`, `--share`, `--pure`, `--fork` | Not shipped | Opencode-only; no gemini analog |
| **Event translation (`stream-json` → `HarnessEvent`)** |||
| `init` → capture `session_id` + model | Working | Feeds `HarnessEvent::Done.session_id` |
| `message { content, delta: false/absent }` → atomic `HarnessEvent::Text` | Working | Emits text + assistant-done marker |
| `message { content, delta: true }` → accumulating chunks | Working | Buffer flushes at next non-message event, or stream close |
| `message { role: "user" }` → ignored | Working | Echo suppression; prevents duplicating the user's own input |
| `tool_use` + matching `tool_result` (success) | Working | Coalesced into one `HarnessEvent::ToolUse` via `ToolPairingBuffer` (keyed by `tool_id`) |
| `tool_use` + `tool_result { status: "error" }` | Working | `output: None`, description decorated with the error message |
| `tool_use` without matching result on stream close | Working | Flushed with `output: None` |
| `tool_result` for unknown `tool_id` | Working | Logged at debug, no event emitted |
| `error { severity: "warning" }` | Working | Logged at warn, does NOT terminate the stream |
| `error { severity: "error" }` | Working | Fatal — surfaced as `HarnessEvent::Error`, stream terminates |
| `error { severity: <other> }` | Working | Treated as warning (resilient to future severity levels) |
| `result { status: "success" }` | Working | Terminal — emits `HarnessEvent::Done` |
| `result { status: "error", error.message }` | Working | Fatal — message is surfaced as `HarnessEvent::Error` |
| Unrecognized `type` | Working | First occurrence logs at warn with "version drift"; subsequent at debug |
| Recognized type with required field missing | Working | `SchemaMismatch` → version-drift warning |
| **Session management** |||
| Persistence to `terminus-state.json` under `gemini:<name>` prefix | Working | Shared `StateStore`; prefix prevents cross-harness name collisions |
| LRU eviction at `max_named_sessions` cap | Working | Shared cap across Claude / opencode / gemini |
| **Ambient events** |||
| `HarnessStarted` (pre-spawn) | Working | Carries `harness: "Gemini"`, `run_id` (ulid) |
| `HarnessFinished` with `status: "ok"` / `"error"` | Working | Fires on every exit path including panic |
| **Error surfaces** |||
| `gemini binary not found` | Working | Includes a hint about `[harness.gemini] binary_path` |
| `gemini spawn failed: <err>` | Working | Non-ENOENT OS-level spawn failures |
| `gemini: stdout pipe missing` | Working | Kills the child before returning |
| `gemini: no output for 5 minutes — killing subprocess` | Working | Kills child, emits error |
| `gemini exited with status <n>: <sanitized stderr>` | Working | 64 KiB stderr cap + `sanitize_stderr` (env-var, home-path redaction) + 500-char truncation |
| `--schema` rejection | Working | Chat-safe redirect to claude |
| Blocked subcommands rejection | Working | See [Blocked subcommands](#blocked-subcommands) |
| Attachment rejection | Working | Returns `"gemini: attachments are not yet supported — send text only"` + `Done` |
| **Config (`[harness.gemini]`)** |||
| `binary_path` | Working | Overrides PATH-based `gemini` resolution |
| `model` | Working | Per-prompt `--model` / `-m` overrides this |
| `approval_mode` | Working | Per-prompt `--approval-mode` overrides this |
| `max_named_sessions` (`[harness]`) | Working | Shared with Claude / opencode |
| **Input safety** |||
| Mobile keyboard em-dash (U+2014) normalization | Working | Shared dispatch block; covers `: gemini —name foo hello` |
| Mutual exclusion: `--name` + `--resume` | Working | Returns "Cannot use both --name and --resume/--continue" |
| **Attachments** |||
| Inbound image / file attachments | Not shipped | Rejected with `"gemini: attachments are not yet supported — send text only"` |
| **Testing** |||
| Unit tests (deterministic, no external deps) | Working | 55 tests in `harness::gemini` covering `translate_event` for all 6 event types incl. `tool_result.error` object/string variants, `ToolPairingBuffer` incl. capacity eviction, `sanitize_stderr` env-var (upper/lower-case) + home-path redaction + truncation, `build_argv` flag ordering + precedence, ambient-event shape, attachment rejection, schema-redirect, mutex-poison recovery |
| `ac1` one-shot streams + completes (live-binary) | Working | Gated by `TERMINUS_HAS_GEMINI=1` |
| `ac2` interactive two-prompt session reuse (live-binary) | Working | Gated |
| `ac3` bogus session id surfaces error (live-binary) | Working | Gated |
| `ac4` tool-use visibility with approval=yolo (live-binary) | Working | Gated |
| `ac5` subcommand output (live-binary) | Not shipped | Originally planned from the opencode ACs; skipped because no chat-safe gemini subcommand was shipped. An equivalent non-gated test covers attachment rejection instead |

---

## Invocation modes

### One-shot

```
: gemini <prompt>
```

Spawns `gemini -o stream-json [flags] <prompt>` as a short-lived child process. No session is persisted unless `--name <x>` is passed.

### Interactive toggle

```
: gemini on [flags]
```

Plain text from the user routes to gemini until `: gemini off`. All flags work with the `on` form and apply for the lifetime of the interactive session (each turn still spawns a short-lived subprocess under the hood).

```
: gemini off
```

Leaves interactive mode; subsequent plain text goes back to the terminal.

---

## Per-prompt flags

All flags work in both one-shot (`: gemini --name foo <prompt>`) and on-toggle (`: gemini on --name foo`) forms. Interactive mode sends any text after the flags as the first prompt.

| Flag | Value | What it does | Maps to gemini CLI |
|---|---|---|---|
| `--name <x>` | string | Create-or-resume named session (prefixed `gemini:<x>`). Resumes if found, creates if not | `-r <stored-id>` after upsert |
| `--resume <x>` | string | Strict resume — errors if `gemini:<x>` is not in the session index | `-r <stored-id>` |
| `--continue <x>` | string | Alias for `--resume <x>` | same |
| `--continue` | (bare) | Continue the most recent gemini session | `-r latest` |
| `-m <x>` / `--model <x>` | string | Override model for this prompt; overrides `[harness.gemini] model` | `-m <x>` |
| `--approval-mode <x>` | `default` \| `auto_edit` \| `yolo` \| `plan` | Set gemini-cli's approval mode; overrides `[harness.gemini] approval_mode` | `--approval-mode <x>` |
| `--schema=<name>` | string | **Not supported** — returns an error directing to the claude harness | — |

### Mutual exclusion rules

- `--name <x>` together with `--resume <x>` / `--continue <x>` (named form) → error: `"cannot use both --name and --resume/--continue"`
- `--fork` for gemini → error `"gemini does not support --fork — remove the flag"` (gemini-cli has no fork analog; rejected at parse time)
- Bare `--continue` combined with `--name` / `--resume` → error: `"Cannot use bare --continue with --name or --resume"`

---

## Subcommands (safe, read-only)

Gemini-cli's only chat-safe surfaces are the read-only `--list-*` flags. Terminus exposes them under user-friendly chat keywords:

| Chat keyword | Maps to | What it does |
|---|---|---|
| `: gemini sessions` | `gemini --list-sessions` | List saved sessions for the current project |
| `: gemini extensions` | `gemini --list-extensions` | List installed extensions |

### Bounds

- 30-second wall-clock timeout per subcommand
- stderr capped at 64 KiB before being sanitized for chat display
- Empty stdout at exit 0 → `"gemini <sub>: no results"` rather than silence
- `NO_COLOR=1` is set on the child env so output is plain-text fenced

## Blocked subcommands

Rejected at parse time with a chat-safe error:

> `` `gemini <sub>` is not available from chat — run it in your terminal. Safe chat subcommands: sessions, extensions. ``

| Command | Why blocked | Alternative |
|---|---|---|
| `update` | Self-updates the gemini binary — interactive confirmation, then restart required | Run `gemini update` in terminal, then restart terminus |
| `mcp` | Opens a TTY-bound interactive MCP server manager | Run in terminal |
| `extensions <sub>` (any sub-form) | `extensions add` / `remove` / `enable` / `disable` are interactive | Run in terminal. Bare `: gemini extensions` (chat-safe list) still works |
| `skills` | Interactive skills install / discovery | Run in terminal |
| `hooks` | Interactive hook management | Run in terminal |

---

## Event schema

Ground truth: `packages/core/src/output/types.ts` in `github.com/google-gemini/gemini-cli`.

```text
{"type":"init",        "timestamp":"…", "session_id":"…", "model":"…"}
{"type":"message",     "timestamp":"…", "role":"user|assistant", "content":"…", "delta":true|false}
{"type":"tool_use",    "timestamp":"…", "tool_id":"…", "tool_name":"…", "parameters":{…}}
{"type":"tool_result", "timestamp":"…", "tool_id":"…", "status":"success|error", "output":"…", "error":{"type":"…","message":"…"}}
{"type":"error",       "timestamp":"…", "severity":"warning|error", "message":"…"}
{"type":"result",      "timestamp":"…", "status":"success|error", "error":{"type":"…","message":"…"}, "stats":{…}}
```

**Key divergence from opencode:** `tool_use` and `tool_result` are **separate events linked by `tool_id`** (opencode packs them into one). The harness coalesces them into a single `HarnessEvent::ToolUse` via `ToolPairingBuffer`. Unpaired `tool_use` on stream close is flushed with `output: None`.

**Streaming text:** `message` events with `delta: true` carry a partial chunk in `content`; the harness accumulates them in a per-turn buffer. `delta: false` or absent marks an atomic message and flushes the buffer.

---

## Configuration

The `[harness.gemini]` block in `terminus.toml` is entirely optional. If omitted, terminus inherits gemini-cli's own defaults (auth, default model).

```toml
[harness.gemini]
# Override the gemini binary location (default: resolved via PATH).
# binary_path = "/usr/local/bin/gemini"

# Default model; per-prompt `--model <x>` or `-m <x>` overrides this.
# Aliases: "pro" | "flash" | "flash-lite"
# model = "flash"

# Default approval mode; per-prompt `--approval-mode <x>` overrides this.
# Values: "default" | "auto_edit" | "yolo" | "plan"
# approval_mode = "default"
```

### Runtime requirements

- `gemini` must be on `PATH` (or `binary_path` must point to it) — parallel to the `tmux must be on PATH` constraint
- `gemini` must already be authenticated (via env var or its own config / OAuth flow); terminus does not proxy credentials
- Default model must be configured via gemini-cli's own config, or overridden here / per-prompt

---

## Error messages

| Error | Meaning | User action |
|---|---|---|
| `gemini binary not found: <path> (set [harness.gemini] binary_path or install gemini on PATH)` | Spawn failed with ENOENT | Install gemini or fix `binary_path` |
| `gemini spawn failed: <err>` | Other spawn failure | Check binary permissions and path |
| `gemini: stdout read: <err>` | Stdout pipe read error mid-stream | Internal / IO error; retry the prompt |
| `gemini: stdout pipe missing` | Child's stdout handle failed | Internal error; retry the prompt |
| `gemini: no output for 5 minutes — killing subprocess` | Per-line silence timeout | Retry; possibly a hung model or stuck tool |
| `gemini exited with status <n>: <sanitized stderr>` | Non-zero exit | stderr is sanitized (env-var assignments and `/Users/<name>/` / `/home/<name>/` paths redacted) |
| `gemini: no response content` | Exit 0, zero recognized events | Model returned nothing; check quota / auth |
| `gemini: no recognized events received (version drift — check \`gemini --version\`)` | Exit 0, only unknown event types | Upgrade or downgrade gemini-cli to a compatible version |
| `gemini: event schema mismatch (version drift — check \`gemini --version\`)` | Recognized type but required field missing | Schema changed upstream; report to terminus |
| `gemini result status: <status>` | `result` event with non-success status and no detail | Check auth / quota / model availability |
| `gemini does not support --schema. Try: \`: claude --schema=<name> <prompt>\`` | `--schema` passed to gemini | Use the claude harness for structured output |
| `` `gemini <sub>` is not available from chat — run it in your terminal. Safe chat subcommands: sessions, extensions. `` | Blocked subcommand invoked | Use a safe subcommand or run the native command in terminal |
| `gemini does not support --fork — remove the flag` | `--fork` passed for gemini | gemini-cli has no fork analog; drop the flag |
| `gemini: attachments are not yet supported — send text only` | An attachment was sent to the harness | Send the prompt without the attachment |

---

## Mobile keyboard normalization

Em-dash (U+2014) in any flag position is normalized to `--` at the top of the harness dispatch block, covering all harnesses (claude, opencode, codex, gemini).

```
: gemini —name review hello
```

Parses identically to:

```
: gemini --name review hello
```

En-dash (U+2013) is intentionally NOT normalized — it appears in legitimate prose (e.g. page ranges `5–10`).

---

## Testing

See [docs/integration-tests.md](integration-tests.md) for the general gated-test model.

**Unit tests (always run):** 54 tests in `src/harness/gemini.rs::tests` cover argv construction (including flag-ordering and per-prompt-vs-config precedence), event translation for all six event types (with `tool_result.error` as both structured `{type, message}` and forward-compat string variants), the pairing buffer's lifecycle including `CAP`-based eviction, `sanitize_stderr` redaction (upper- and lower-case env vars, home-path stripping) with truncation-after-redaction ordering, attachment rejection, ambient-event shape, panic formatting, and session-key prefixing.

**Gated live-binary tests** (`#[ignore]` + `TERMINUS_HAS_GEMINI=1`):

- `ac1_one_shot_flash_lite_streams_and_completes`
- `ac2_interactive_two_prompts_reuse_session`
- `ac3_bogus_session_id_surfaces_error_path`
- `ac4_tool_use_visibility_with_approval_yolo`

Run with:

```bash
TERMINUS_HAS_GEMINI=1 cargo test --release -- --ignored --test-threads=1
```

Preconditions: `gemini` on PATH and authenticated.

---

## Known limitations

1. **Limited chat-safe subcommand surface.** Only the read-only `--list-sessions` and `--list-extensions` flags are exposed (as `: gemini sessions` / `: gemini extensions`). Gemini-cli's interactive surfaces (`mcp`, `skills`, `hooks`, `extensions <sub>`, `update`) remain blocked at the parser. There's no `models` listing because gemini-cli doesn't expose one.
2. **Inbound attachments not forwarded.** Image / file attachments sent via chat are rejected with a chat-safe error rather than silently dropped. Gemini-cli has a multimodal input path but terminus does not thread attachments through it yet.
3. **`result` / `stats` token-usage surfacing.** The terminal `result` event carries a `stats` payload (input/output tokens, duration, tool calls). Terminus currently uses `stats` only to diagnose the terminal marker; the numbers are not surfaced to chat. An opencode-style `: gemini stats` equivalent is not shipped.
4. **No per-harness binary-version detection.** If the on-disk `gemini` emits events with a drifted schema, terminus surfaces a generic "version drift — check `gemini --version`" warning rather than fetching and reporting the actual version.
5. **Version-drift / no-response messages are chat `Text`, not `Error`.** Mirrors opencode's behavior. Users see the diagnostic inline with normal output rather than visually marked as an error. Slated as a harness-wide follow-up.
