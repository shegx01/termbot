# codex harness — CLI reference

The codex harness wraps OpenAI's `codex` CLI (`github.com/openai/codex`, verified against codex-cli **0.125.0**) as a short-lived subprocess per prompt. Each invocation spawns `codex exec --json --full-auto --skip-git-repo-check [flags] <prompt>`, streams newline-delimited JSON events from stdout back to chat in real time, and exits cleanly with no persistent sidecar or port binding. Mirrors the `gemini` harness pattern (paired tool-use events via the shared `ToolPairingBuffer`) with codex-specific argv construction.

## Contents

- [Functionality matrix](#functionality-matrix)
- [Invocation modes](#invocation-modes)
- [Per-prompt flags](#per-prompt-flags)
- [Blocked subcommands](#blocked-subcommands)
- [Event schema](#event-schema)
- [Configuration](#configuration)
- [Error messages](#error-messages)
- [Mobile keyboard normalization](#mobile-keyboard-normalization)
- [Testing](#testing)
- [Known limitations](#known-limitations)

---

## Functionality matrix

Status legend: **Working** = shipped and tested · **Partial** = implemented with a documented gap · **Not shipped** = deliberate non-goal or deferred to a follow-up.

| Feature | Status | Notes |
|---|---|---|
| **Invocation** |||
| One-shot (`: codex <prompt>`) | Working | Non-interactive; each prompt spawns a fresh `codex exec --json` subprocess |
| Interactive toggle (`: codex on` / `: codex off`) | Working | Plain text routes to codex between the two toggles |
| Codex subcommand passthrough (`: codex sessions` / `resume` / `cloud` etc.) | Not shipped | All codex 0.125.0 native subcommands are blocked at the parser; revisit in v1.1 once `cloud`/`apply` lifecycle is designed |
| **Per-prompt flags** |||
| `--name <x>` (create-or-resume) | Working | Upsert; internally prefixed `codex:<x>` in the session index |
| `--resume <x>` / `--continue <x>` (strict resume) | Working | Errors if the named session isn't in the index |
| Bare `--continue` (continue last) | Working | Translates to `codex exec resume --json … --last <prompt>` |
| `--model <x>` / `-m <x>` | Working | Per-prompt wins over `[harness.codex].model`. Note: ChatGPT-account auth rejects some models (e.g. `gpt-5.5` → API-only); use `gpt-5.4` or `gpt-5.3-codex` |
| `--sandbox <x>` | Working | Values: `read-only`, `workspace-write`, `danger-full-access`. Per-prompt wins over `[harness.codex].sandbox` |
| `--profile <x>` | Working | No `-p` short alias (collides with Claude's `--permission-mode`). Per-prompt wins over `[harness.codex].profile` |
| `--approval-mode on-request` | Working (rejected) | Codex 0.125.0 has no `--ask-for-approval` flag at all; terminus passes `--full-auto` unconditionally. Passing `--approval-mode on-request` returns a chat-safe error explaining the deadlock risk |
| `--schema <name>` | Working | Inline JSON or file path. Inline JSON written to a temp file, passed via `codex exec --output-schema <path>`. Validated response is rendered as text (no separate `StructuredOutput` channel) |
| `--fork` | Working (rejected) | Codex's `fork` is a separate interactive subcommand; not exposed in non-interactive mode. Returns "codex does not support --fork in non-interactive mode" |
| `--title`, `--share`, `--pure`, `--agent` | Not shipped | Opencode-only; no codex analog (silently ignored) |
| **Always-on flags** |||
| `--full-auto` | Working | Sandboxed automatic execution; replaces the removed `--ask-for-approval` knob |
| `--skip-git-repo-check` | Working | Tmux cwd may not always be a git repo |
| `--ephemeral` | Working | Passed when no named/resumed session is active so one-shot prompts don't pollute codex's session log |
| `--ignore-user-config` | Working (opt-in) | Set via `[harness.codex].ignore_user_config = true` for defense-in-depth against user profiles re-introducing approval prompts |
| **Event translation (`--json` NDJSON → `HarnessEvent`)** |||
| `thread.started` → capture `thread_id` | Working | Feeds `HarnessEvent::Done.session_id` (terminus translates `thread_id` ↔ `session_id` at the boundary) |
| `turn.started` → marker | Working | Recognized but emits no chat event |
| `item.completed { type: "agent_message", text }` → `HarnessEvent::Text` | Working | Multiple `item.completed` per turn are forwarded in order; read loop continues until `turn.completed` |
| `item.completed { type: "reasoning" }` | Working | Silently dropped in v1 (revisit if users ask) |
| `item.started` for tool kinds (`command_execution`, `file_change`, `mcp_tool_call`, `web_search`, `plan_update`) | Working | Buffered in shared `ToolPairingBuffer` keyed by `item.id` |
| `item.completed` for tool kinds → coalesced `HarnessEvent::ToolUse` | Working | `output` from `aggregated_output`, success from `exit_code == 0` |
| `item.completed { exit_code != 0 }` → tool-use error | Working | `output: None`, description decorated with `exit_code=<n>` |
| Unpaired `item.started` on stream close | Working | Flushed with `output: None` |
| `error { message }` | Working | Non-terminal — surfaced as `HarnessEvent::Error("codex: <msg>")`, read loop continues |
| `turn.failed { error.message }` | Working | Terminal — surfaced as `HarnessEvent::Error("codex: <msg>")`, stream terminates |
| `turn.completed` | Working | Terminal success — emits `HarnessEvent::Done { session_id: <thread_id> }` |
| Unrecognized `type` | Working | First occurrence logs at warn with "version drift"; subsequent at debug |
| **Session management** |||
| Persistence to `terminus-state.json` under `codex:<name>` prefix | Working | Shared `StateStore`; prefix prevents cross-harness name collisions |
| LRU eviction at `max_named_sessions` cap | Working | Shared cap across Claude / opencode / gemini / codex |
| **Ambient events** |||
| `HarnessStarted` (pre-spawn) | Working | Carries `harness: "Codex"`, `run_id` (ulid) |
| `HarnessFinished` with `status: "ok"` / `"error"` | Working | Fires on every exit path including panic |
| **Attachments** |||
| Image attachments forwarded via `-i <path>` | Working | MIME whitelist: `image/png`, `image/jpeg`, `image/jpg`, `image/webp`. Repeatable for multiple images |
| Non-image attachments | Working (rejected) | Returns `"codex: only image attachments supported (got <mime> — allowed: …)"` |
| **Error surfaces** |||
| `codex binary not found` | Working | Includes a hint about `[harness.codex] binary_path` and install commands |
| `codex spawn failed: <err>` | Working | Non-ENOENT OS-level spawn failures |
| `codex: stdout pipe missing` | Working | Kills the child before returning |
| `codex: no output for 5 minutes — killing subprocess` | Working | Kills child, emits error |
| `codex exited with status <n>: <sanitized stderr>` | Working | 64 KiB stderr cap + `sanitize_stderr` (filters benign `failed to record rollout items` / `Reading additional input from stdin` lines, redacts env vars and home paths, truncates to 500 chars) |
| Blocked subcommands rejection | Working | See [Blocked subcommands](#blocked-subcommands) |
| **Config (`[harness.codex]`)** |||
| `binary_path` | Working | Overrides PATH-based `codex` resolution |
| `model` | Working | Per-prompt `--model` / `-m` overrides this |
| `profile` | Working | Per-prompt `--profile` overrides this |
| `sandbox` | Working | Per-prompt `--sandbox` overrides this |
| `ignore_user_config` | Working | When `true`, terminus passes `--ignore-user-config` so codex skips `~/.codex/config.toml` (defense-in-depth) |
| `max_named_sessions` (`[harness]`) | Working | Shared with Claude / opencode / gemini |
| **Input safety** |||
| Mobile keyboard em-dash (U+2014) normalization | Working | Shared dispatch block; covers `: codex —name foo hello` |
| Mutual exclusion: `--name` + `--resume` | Working | Returns "Cannot use both --name and --resume/--continue" |
| Mutual exclusion: `--name` + `--continue` | Working | Same error |
| **Stdin discipline** |||
| `Stdio::null()` for child stdin | Working | Codex 0.125.0 reads stdin even when prompt is supplied as arg; `null` prevents indefinite blocking |
| **Testing** |||
| Unit tests (deterministic, no external deps) | Working | 44 tests in `harness::codex` covering `build_argv` for fresh/resume/continue with all flag combinations, `translate_event` for every event type, `ToolPairingBuffer` integration, MIME-whitelist enforcement, schema temp-file lifecycle, `sanitize_stderr` for codex-specific noise + env-vars + home paths, panic-message formatting |
| Command parser tests | Working | 20 tests in `command::tests::parse_codex_*` covering basic prompt, named session, resume, continue, sandbox/profile/model overrides, every blocked subcommand, on-request rejection, fork rejection (flag + on form), em-dash normalization, on/off toggles, summary/is_empty regression |
| Cross-harness regression: gemini buffer relocation | Working | All gemini tests pass with the relocated `ToolPairingBuffer` from `crate::harness` |

---

## Invocation modes

### One-shot

```
: codex <prompt>
```

Spawns `codex exec --json --full-auto --skip-git-repo-check [flags] <prompt>` as a short-lived child process. No session is persisted unless `--name <x>` is passed; otherwise `--ephemeral` is added so codex doesn't write a session log entry.

### Interactive toggle

```
: codex on [flags]
```

Plain text from the user routes to codex until `: codex off`. All flags work with the `on` form and apply for the lifetime of the interactive session (each turn still spawns a short-lived subprocess under the hood).

```
: codex off
```

Leaves interactive mode; subsequent plain text goes back to the terminal.

---

## Per-prompt flags

All flags work in both one-shot (`: codex --name foo <prompt>`) and on-toggle (`: codex on --name foo`) forms. Interactive mode sends any text after the flags as the first prompt.

| Flag | Value | What it does | Maps to codex CLI |
|---|---|---|---|
| `--name <x>` | session name | Create-or-resume by name | (terminus-internal) |
| `--resume <x>` / `--continue <x>` | session name | Strict resume; errors if name unknown | `exec resume <thread_id>` |
| `--continue` (bare) | (none) | Continue most recent codex session | `exec resume --last` |
| `--model <x>` / `-m <x>` | e.g. `gpt-5.4`, `gpt-5.3-codex` | Override model | `-m <model>` |
| `--sandbox <x>` | `read-only` \| `workspace-write` \| `danger-full-access` | Override sandbox policy | `-s <sandbox>` |
| `--profile <x>` | profile name | Select named profile from `~/.codex/config.toml` | `-p <profile>` |
| `--schema <x>` | file path or inline JSON | Validate response shape | `--output-schema <path>` (inline written to temp) |
| `--approval-mode on-request` | (rejected) | Returns chat-safe deadlock error | n/a — codex 0.125.0 has no equivalent flag |
| `--fork` | (rejected) | Returns "not supported in non-interactive mode" | n/a |

### Mutual exclusion

- `--name` and `--resume`/`--continue <x>` are mutually exclusive.
- `--name` and bare `--continue` are mutually exclusive.

---

## Blocked subcommands

Codex 0.125.0 ships many top-level subcommands. Only `exec` (the prompt entrypoint) is wired into terminus. Every other subcommand returns a chat-safe error so users can't accidentally invoke an interactive surface from chat.

| Subcommand | Chat-safe error |
|---|---|
| `login` / `logout` | "codex auth must be run from your terminal" |
| `mcp` / `mcp-server` | "MCP management is not exposed to chat; run from terminal" |
| `app` / `app-server` / `exec-server` | "codex desktop/server modes are not exposed to chat" |
| `cloud` / `apply` | "cloud surface deferred to v1.1; run from terminal" |
| `resume` (bare subcommand) | "use `: codex --resume <name>` for named-session resume; the bare `resume` subcommand is not exposed to chat in v1" |
| `sessions` | "session listing is not exposed to chat in v1; run `codex exec resume --all` from your terminal" |
| `fork` | "session forking is not exposed to chat in v1" |
| `plugin` / `completion` / `features` / `debug` / `sandbox` / `review` | "not exposed to chat in v1; run from terminal" |

---

## Event schema

Codex emits newline-delimited JSON events on stdout when invoked with `--json`. Verified against codex-cli 0.125.0 via direct spike runs (`.omc/research/codex-events-fresh.ndjson`, `codex-events-tooluse.ndjson`).

```json
{"type":"thread.started","thread_id":"019dcf4d-aaaa-7777-bbbb-cccccccccccc"}
{"type":"turn.started"}
{"type":"item.completed","item":{"id":"item_0","type":"agent_message","text":"…"}}
{"type":"item.started","item":{"id":"item_1","type":"command_execution",
  "command":"…","aggregated_output":"","exit_code":null,"status":"in_progress"}}
{"type":"item.completed","item":{"id":"item_1","type":"command_execution",
  "command":"…","aggregated_output":"…","exit_code":0,"status":"completed"}}
{"type":"turn.completed","usage":{"input_tokens":…,"cached_input_tokens":…,
  "output_tokens":…,"reasoning_output_tokens":…}}
```

Key rules:

- `agent_message` arrives ONLY in `item.completed` (no started/completed pair for text).
- Tool-kind items (`command_execution`, `file_change`, `mcp_tool_call`, `web_search`, `plan_update`) DO pair `item.started` + `item.completed` linked by `item.id`.
- Multiple `item.completed` events may arrive per turn — the read loop continues until `turn.completed` (success) or `turn.failed` (failure) arrives.
- `error` events are non-terminal; `turn.failed` is terminal.

---

## Configuration

```toml
[harness]
max_named_sessions = 50          # shared with Claude / opencode / gemini / codex

[harness.codex]
binary_path = "/opt/homebrew/bin/codex"   # default: resolved via PATH
model = "gpt-5.4"                          # default: codex's own default
profile = "default"                        # default: codex's own active profile
sandbox = "workspace-write"                # default: codex's own default
ignore_user_config = false                 # opt-in: skip ~/.codex/config.toml
```

All fields are optional. When omitted, terminus resolves `codex` from PATH and inherits the user's own config.

### Defaults applied unconditionally by terminus (NOT configurable):

- `--full-auto` — replaces the removed `--ask-for-approval` knob; codex 0.125.0 has no other way to skip TTY approval prompts.
- `--skip-git-repo-check` — terminus's tmux cwd may not always be a git repo.
- `--ephemeral` — added when no named/resumed session is active.
- `Stdio::null()` for child stdin — codex 0.125.0 reads stdin even when prompt is supplied as an arg, blocking forever without this.

### `--cd / -C <path>` is intentionally NOT passed

Terminus already sets the child process's working directory via `tokio::process::Command::current_dir(cwd)`. Passing `--cd` redundantly risks divergence if codex resolves `--cd` differently from the OS-level `current_dir`.

---

## Error messages

All error strings the harness can return to chat (not exhaustive — common categories):

| Category | Example |
|---|---|
| Binary | `"codex binary not found: codex (set [harness.codex] binary_path or install codex on PATH; e.g. \`brew install --cask codex\` or \`npm install -g @openai/codex\`)"` |
| Spawn | `"codex spawn failed: <io error>"` |
| stdout pipe | `"codex: stdout pipe missing"` |
| Idle timeout | `"codex: no output for 5 minutes — killing subprocess"` |
| Non-zero exit | `"codex exited with status 1: <sanitized stderr>"` |
| Approval mode | `"--approval-mode on-request would deadlock the harness (no TTY for approval prompts) — terminus always passes --full-auto instead"` |
| Sandbox | `"Invalid --sandbox '<x>' — expected read-only, workspace-write, or danger-full-access"` |
| Attachment | `"codex: only image attachments supported (got <mime> — allowed: image/png, image/jpeg, image/jpg, image/webp)"` |
| Schema | `"codex: --schema is neither a file path nor valid JSON: <parse error>"` |
| Schema empty | `"codex: --schema value is empty"` |
| Fork | `"codex does not support --fork in non-interactive mode (use \`codex fork\` from your terminal). Remove the flag."` |
| Turn failed | `"codex: <nested error.message>"` (from `turn.failed.error.message`) |

---

## Mobile keyboard normalization

The smart-quote / em-dash normalization shared with Claude / opencode / gemini also applies to codex. So `: codex —name auth fix login` parses correctly even when iOS / Android substitutes `—` for `--`.

---

## Testing

Run unit tests:

```
cargo test --lib harness::codex
cargo test --lib command::tests::parse_codex
```

The codex harness has 44 deterministic unit tests + 20 command-parser tests, none of which require the codex binary to be installed. Manual smoke testing (with codex on PATH and authenticated via `codex login`) is the recommended last step:

1. `cargo run` (with a valid `terminus.toml`)
2. From your authorized chat platform, send `: codex what is 2+2`
3. Expect a `Text` event with the answer and a `Done` event
4. Send `: codex --name test-session write a hello.txt`
5. Verify the file is created in cwd and `terminus-state.json` gains a `codex:test-session` entry
6. Restart terminus, send `: codex --resume test-session what did you do?`
7. Verify codex resumes the previous thread

---

## Known limitations

- **Cloud surface deferred to v1.1.** `codex cloud` and `codex apply` are blocked at the parser; the long-running, two-step submit-then-apply lifecycle doesn't fit terminus's short-lived-subprocess pattern. Revisit once a cloud lifecycle design is spec'd.
- **No subcommand passthrough in v1.** `: codex sessions` (list), `: codex resume` (interactive picker), `: codex models` (list) are all blocked. Named-session resume still works via the `--resume <name>` flag, which is independent of codex's `resume` subcommand.
- **`reasoning` items dropped silently.** When codex emits `item.completed { type: "reasoning" }`, terminus drops it without surfacing to chat. Surfacing reasoning would create noisy chat output; revisit if users ask.
- **Token usage from `turn.completed.usage` not surfaced to chat.** Parity with gemini, where `result` event stats are also unsurfaced today.
- **Image-only attachment whitelist.** `image/png`, `image/jpeg`, `image/jpg`, `image/webp` only. HEIC, PDF, video, etc. are rejected with chat-safe errors. Multimodal expansion deferred.
- **No structured-output rendering as anything richer than text.** `--schema` validates the response and returns it as `HarnessEvent::Text` (gemini-parity). The dedicated `HarnessEvent::StructuredOutput` channel is reserved for the Claude SDK's webhook-delivery pipeline.
- **`gpt-5.5` rejected with ChatGPT auth.** Codex's default model on a ChatGPT account run won't accept `gpt-5.5` (API-only). Use `gpt-5.4` or `gpt-5.3-codex` via `--model` or `[harness.codex].model`.
- **Stderr ERROR lines are benign.** The `failed to record rollout items: thread <id> not found` line is filtered automatically by `sanitize_stderr` and does not propagate to chat. Same for `Reading additional input from stdin...` (only appears if stdin redirection regresses).
- **Cross-harness session-name persist failure (terminus-wide).** State file persists one entry per key; the `codex:` prefix prevents collisions with other harnesses, but a write-failure mode shared with opencode/gemini is documented in CLAUDE.md as a known limitation. Not codex-specific.
