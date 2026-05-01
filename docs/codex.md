# codex harness — CLI reference

The codex harness wraps OpenAI's `codex` CLI (`github.com/openai/codex`, verified against codex-cli **0.128.0**) as a short-lived subprocess per prompt. Each invocation spawns `codex exec --json -s workspace-write --skip-git-repo-check [flags] <prompt>`, streams newline-delimited JSON events from stdout back to chat in real time, and exits cleanly with no persistent sidecar or port binding. Mirrors the `gemini` harness pattern (paired tool-use events via the shared `ToolPairingBuffer`) with codex-specific argv construction.

## Contents

- [Functionality matrix](#functionality-matrix)
- [Invocation modes](#invocation-modes)
- [Per-prompt flags](#per-prompt-flags)
- [Blocked subcommands](#blocked-subcommands)
- [Event schema](#event-schema)
- [Configuration](#configuration)
- [Structured output and webhooks](#structured-output-and-webhooks)
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
| Codex subcommand passthrough (`: codex sessions` / `apply` / `cloud {list,status,diff,apply,exec}`) | Working | F6+F7: chat-safe single-shot subprocess; 30s timeout, 3000-char truncated. See [Chat-safe subcommands](#chat-safe-subcommands). Other native subcommands (`login`, `mcp`, `resume`, `fork`, `app`, …) remain blocked |
| **Per-prompt flags** |||
| `--name <x>` (create-or-resume) | Working | Upsert; internally prefixed `codex:<x>` in the session index |
| `--resume <x>` / `--continue <x>` (strict resume) | Working | Errors if the named session isn't in the index |
| Bare `--continue` (continue last) | Working | Translates to `codex exec resume --json … --last <prompt>` |
| `--model <x>` / `-m <x>` | Working | Per-prompt wins over `[harness.codex].model`. Default in codex 0.128 is `gpt-5.5`; `gpt-5.4` and `gpt-5.4-mini` are also available. (`gpt-5.3-codex` was removed in 0.128.) |
| `--sandbox <x>` | Working | Values: `read-only`, `workspace-write`, `danger-full-access`. Per-prompt wins over `[harness.codex].sandbox`. When neither is set, terminus emits `-s workspace-write` as the default (replaces the deprecated-in-0.128 `--full-auto`) |
| `--profile <x>` | Working | No `-p` short alias (collides with Claude's `--permission-mode`). Per-prompt wins over `[harness.codex].profile` |
| `--add-dir <DIR>` / `-d <DIR>` (repeatable) | Working | Forwarded to `codex exec --add-dir <DIR>` to extend the writable sandbox beyond the current `cwd`. Each occurrence appends one flag/value pair, in argv order. |
| `--approval-mode <x>` | Cross-harness reject | Gemini-only flag. `: codex --approval-mode …` returns "`--approval-mode` is only supported by gemini" at parse time (codex 0.128 has no equivalent flag — approvals live behind permission profiles, not a CLI knob) |
| `--schema <name>` | Working | Three forms: (a) registered name from `[schemas.<name>]` in `terminus.toml`; (b) absolute or cwd-relative file path; (c) inline JSON. All three resolve to a temp file passed via `codex exec --output-schema <path>`. The registered-name form additionally drives the webhook-delivery pipeline (see [Structured output and webhooks](#structured-output-and-webhooks)) |
| `--fork` | Working (rejected) | Codex's `fork` is a separate interactive subcommand; not exposed in non-interactive mode. Returns "codex does not support --fork in non-interactive mode" |
| `--title`, `--share`, `--pure`, `--agent` | Working (rejected) | Opencode-only; using one with `: codex` returns a cross-harness redirect error (`"--<flag> is only supported by opencode (you used `: codex`)"`) |
| **Always-on flags** |||
| `-s <sandbox>` | Working | Defaults to `workspace-write` when no per-prompt or config value is set (replaces the deprecated `--full-auto` from 0.128). User-supplied `--sandbox` / `[harness.codex].sandbox` overrides |
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
| `Stdio::null()` for child stdin | Working | `codex exec` reads stdin even when prompt is supplied as arg (still true in 0.128 — codex prints "Reading additional input from stdin..."); `null` prevents indefinite blocking |
| **Testing** |||
| Unit tests (deterministic, no external deps) | Working | 44 tests in `harness::codex` covering `build_argv` for fresh/resume/continue with all flag combinations, `translate_event` for every event type, `ToolPairingBuffer` integration, MIME-whitelist enforcement, schema temp-file lifecycle, `sanitize_stderr` for codex-specific noise + env-vars + home paths, panic-message formatting |
| Command parser tests | Working | `command::tests::parse_codex_*` covers basic prompt, named session, resume, continue, sandbox/profile/model overrides, blocked subcommands, on-request rejection, fork rejection (flag + on form), em-dash normalization, on/off toggles, summary/is_empty regression, the F6+F7 chat-safe subcommand routing surface (sessions / apply / cloud / cloud-{list,status,diff,apply,exec}), and F10 cross-harness flag rejection. Run `cargo test --lib command::tests::parse_codex` for the current list. |
| Cross-harness regression: gemini buffer relocation | Working | All gemini tests pass with the relocated `ToolPairingBuffer` from `crate::harness` |

---

## Invocation modes

### One-shot

```
: codex <prompt>
```

Spawns `codex exec --json -s workspace-write --skip-git-repo-check [flags] <prompt>` as a short-lived child process. No session is persisted unless `--name <x>` is passed; otherwise `--ephemeral` is added so codex doesn't write a session log entry. The `-s workspace-write` default replaces the deprecated `--full-auto` from codex 0.128 — user-supplied `--sandbox` or `[harness.codex].sandbox` overrides it.

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
| `--model <x>` / `-m <x>` | e.g. `gpt-5.5` (default in 0.128), `gpt-5.4`, `gpt-5.4-mini` | Override model | `-m <model>` |
| `--sandbox <x>` | `read-only` \| `workspace-write` \| `danger-full-access` | Override sandbox policy | `-s <sandbox>` |
| `--profile <x>` | profile name | Select named profile from `~/.codex/config.toml` | `-p <profile>` |
| `--schema <x>` | registered name, file path, or inline JSON | Validate response shape; registered names also drive webhook delivery | `--output-schema <path>` (resolved value written to temp) |
| `--approval-mode on-request` | (rejected) | Cross-harness reject: gemini-only flag (`: gemini` path also rejects `on-request` with the deadlock-explanation wording) | n/a — codex 0.128 has no equivalent CLI flag; approvals live behind permission profiles |
| `--fork` | (rejected) | Returns "not supported in non-interactive mode" | n/a |

### Mutual exclusion

- `--name` and `--resume`/`--continue <x>` are mutually exclusive.
- `--name` and bare `--continue` are mutually exclusive.

---

## Chat-safe subcommands

The following codex subcommands (verified against codex 0.128) are exposed in chat. Each is a single-shot subprocess (30s timeout, output truncated at 3000 chars inside a fenced code block):

| Chat invocation | Codex CLI mapping | Notes |
|---|---|---|
| `: codex sessions` | `codex resume --all` | Read-only listing of saved sessions for this project. |
| `: codex apply <task_id>` | `codex apply <task_id>` | Top-level shortcut: applies a Codex Cloud task diff to the working tree as `git apply`. |
| `: codex cloud list` | `codex cloud list` | List Codex Cloud tasks. Extra args (`--limit N`, `--cursor X`, `--env <id>`) are forwarded. |
| `: codex cloud status <task_id>` | `codex cloud status <task_id>` | Show task status. |
| `: codex cloud diff <task_id>` | `codex cloud diff <task_id>` | Show the unified diff for a task. |
| `: codex cloud apply <task_id>` | `codex cloud apply <task_id>` | Apply task diff locally. |
| `: codex cloud exec --env <env_id> <query>` | `codex cloud exec --env <env_id> <query>` | Submit a new cloud task. `--env` is required upstream; codex surfaces the usage error if omitted. |

Bare `: codex cloud` (no recognized sub-word) returns a help message listing the valid forms above.

## Blocked subcommands

Every other codex 0.128 subcommand returns a chat-safe error so users can't accidentally invoke an interactive surface from chat:

| Subcommand | Chat-safe error |
|---|---|
| `login` / `logout` | "codex auth must be run from your terminal" |
| `mcp` / `mcp-server` | "MCP management is not exposed to chat; run from terminal" |
| `app` / `app-server` / `exec-server` | "codex desktop/server modes are not exposed to chat" |
| `resume` (bare subcommand) | "use `: codex --resume <name>` for named-session resume; the bare `resume` subcommand is not exposed to chat in v1" |
| `fork` | "session forking is not exposed to chat in v1" |
| `plugin` / `completion` / `features` / `debug` / `sandbox` / `review` | "not exposed to chat in v1; run from terminal" |

---

## Event schema

Codex emits newline-delimited JSON events on stdout when invoked with `--json`. Event schema verified against codex-cli 0.125.0 spike (`.omc/research/codex-events-fresh.ndjson`, `codex-events-tooluse.ndjson`); re-verified compatible against 0.128.

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
model = "gpt-5.5"                          # default: codex's own default (gpt-5.5 in 0.128)
profile = "default"                        # default: codex's own active profile
sandbox = "workspace-write"                # default: codex's own default
ignore_user_config = false                 # opt-in: skip ~/.codex/config.toml
```

All fields are optional. When omitted, terminus resolves `codex` from PATH and inherits the user's own config.

### Defaults applied unconditionally by terminus (NOT configurable):

- `-s workspace-write` — applied when no per-prompt or config sandbox value is set. Replaces the deprecated-in-0.128 `--full-auto` (codex prints a deprecation warning if `--full-auto` is used). `codex exec` is non-interactive by default in 0.128, so no separate "skip TTY approval" flag is needed.
- `--skip-git-repo-check` — terminus's tmux cwd may not always be a git repo.
- `--ephemeral` — added when no named/resumed session is active.
- `Stdio::null()` for child stdin — `codex exec` reads stdin even when prompt is supplied as an arg (still true in 0.128), blocking forever without this.

### `--cd / -C <path>` is intentionally NOT passed

Terminus already sets the child process's working directory via `tokio::process::Command::current_dir(cwd)`. Passing `--cd` redundantly risks divergence if codex resolves `--cd` differently from the OS-level `current_dir`.

### Schema registry entries

Schema registry entries (`[schemas.<name>]` with `schema`, `webhook`, `webhook_secret_env` keys) are documented in [Structured output and webhooks](#structured-output-and-webhooks) — they're not codex-specific and are validated at startup against all schema-supporting harnesses (claude + codex).

---

## Structured output and webhooks

`--schema <x>` resolves in priority order — **(1) registered name → (2) file path → (3) inline JSON**. Only the registered-name form drives the webhook-delivery pipeline; the other two are chat-only validation surfaces.

| `--schema=…` form | Behavior | Webhook delivery? |
|---|---|---|
| **`<name>`** matching `[schemas.<name>]` in `terminus.toml` | Schema value serialized to a temp file, passed as `--output-schema`. On `turn.completed` the agent_message text is parsed as JSON and emitted as `HarnessEvent::StructuredOutput { schema, value, run_id }` | Yes — when the entry has `webhook` + `webhook_secret_env` set; HMAC-SHA256-signed POST with retry queue |
| **File path** (relative-to-cwd or absolute) | Read, validated as UTF-8 + JSON, copied to a temp, passed as `--output-schema`. Validated response renders as `HarnessEvent::Text` | No — chat-only |
| **Inline JSON** | Written to a temp, passed as `--output-schema`. Validated response renders as `HarnessEvent::Text` | No — chat-only |

The registered-name form takes priority over inline JSON even when the literal string also happens to parse as JSON. This is the security-relevant ordering: a malicious user can't bypass the registry by passing inline JSON with the same shape.

If `--schema=<name>` matches a registered entry but the response is not valid JSON (model deviated despite `--output-schema`), terminus emits `HarnessEvent::Error("codex: --schema='<name>' but response was not valid JSON: …")` followed by `HarnessEvent::Text(<raw text>)` so the content isn't lost. No webhook delivery is attempted.

If `--schema=<name>` matches a registered entry but the entry has NO webhook configured, the harness falls back to the chat-only `HarnessEvent::Text` path — the user gets validation but no webhook fan-out.

Behavioural parity with the claude harness: identical event variant (`HarnessEvent::StructuredOutput`), identical webhook plumbing (`drive_harness` queues the job, `WebhookClient` POSTs with HMAC, retry worker handles transient failures), identical run-id format (ULID).

Configure schemas in `terminus.toml`:

```toml
[schemas.orders_v1]
schema = '''
{
  "type": "object",
  "required": ["order_id", "items"],
  "properties": {
    "order_id": { "type": "string" },
    "items":    { "type": "array", "items": { "type": "string" } }
  }
}
'''
webhook = "https://your-server.example.com/webhooks/orders"
webhook_secret_env = "ORDERS_WEBHOOK_SECRET"
```

Then invoke from chat:

```
: codex --schema=orders_v1 list the latest 5 orders as JSON
```

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
| Approval mode (gemini) | `"--approval-mode on-request would deadlock the harness (no TTY for approval prompts) — pick default, auto_edit, yolo, or plan instead"` (this path is gemini-only after F10; `: codex --approval-mode …` is rejected up-front as a cross-harness flag) |
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

- **Cloud surface is single-shot only.** `: codex cloud {list,status,diff,apply,exec}` and `: codex apply <task_id>` are wired (F7) — each maps to one short-lived subprocess. The submit-then-apply lifecycle is two independent CLI calls; terminus does not retain task state between them. Use `: codex cloud list` / `cloud status <task_id>` to track tasks.
- **`cloud exec` timeout caveat.** All chat-safe codex subcommands share a 30s wall-clock timeout. `cloud exec` submits a task to a remote environment and the network round-trip can occasionally approach that bound. If `cloud exec` returns "timed out after 30s", the task's submission status is **unknown** — the request may have reached the cloud successfully even though terminus didn't see the task ID. Confirm with `: codex cloud list` before re-submitting; rerunning a successful submission will create a duplicate task.
- **Interactive subcommands stay blocked.** `: codex resume` (picker UI), `: codex fork` (picker UI), `: codex review` (interactive review flow), `: codex models` (no top-level surface in codex 0.128) — all blocked at the parser. Named-session resume still works via the `--resume <name>` flag, independent of codex's `resume` subcommand.
- **`reasoning` items dropped silently.** When codex emits `item.completed { type: "reasoning" }`, terminus drops it without surfacing to chat. Surfacing reasoning would create noisy chat output; revisit if users ask.
- **Token usage from `turn.completed.usage` not surfaced to chat.** Parity with gemini, where `result` event stats are also unsurfaced today.
- **Image-only attachment whitelist.** `image/png`, `image/jpeg`, `image/jpg`, `image/webp` only. HEIC, PDF, video, etc. are rejected with chat-safe errors. Multimodal expansion deferred.
- **Inline-JSON and file-path `--schema` forms render as `HarnessEvent::Text` only.** They never feed the webhook pipeline — that is reserved for registered names from `[schemas.<name>]` so the HMAC secret env var lookup is registry-driven (see [Structured output and webhooks](#structured-output-and-webhooks)).
- **Default model is `gpt-5.5`** as of codex 0.128 (priority 0 in the bundled manifest at `codex-rs/models-manager/models.json`). It is universally available — including on ChatGPT-account auth — and replaces `gpt-5.3-codex` (which was removed in 0.128). `gpt-5.4` and `gpt-5.4-mini` remain available for `--model` overrides.
- **Stderr ERROR lines are benign.** The `failed to record rollout items: thread <id> not found` line is filtered automatically by `sanitize_stderr` and does not propagate to chat. Same for `Reading additional input from stdin...` (only appears if stdin redirection regresses).
- **Cross-harness session-name persist failure (terminus-wide).** State file persists one entry per key; the `codex:` prefix prevents collisions with other harnesses, but a write-failure mode shared with opencode/gemini is documented in CLAUDE.md as a known limitation. Not codex-specific.
