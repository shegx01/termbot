# Integration Tests

Terminus has two classes of tests:

1. **Unit tests** — pure, offline, fast. Run by `cargo test`.
2. **Integration tests** — require external binaries or network; gated by `#[ignore]` and env vars.

## Running integration tests

```bash
TERMINUS_HAS_OPENCODE=1 cargo test -- --ignored
```

This flag is additive — it runs ALL `#[ignore]`-marked tests. For a specific test:

```bash
TERMINUS_HAS_OPENCODE=1 cargo test -- --ignored <test_name>
```

## opencode harness — gated tests

**Location:** `src/harness/opencode.rs::tests`

**Preconditions:**
- `opencode` on `PATH`; `opencode --version` succeeds
- opencode must be authenticated (`opencode providers` or `opencode auth login`)
- Default model / agent configured in opencode (check with `opencode run "hi"` in a terminal)
- Port 4096 is **not** required (the CLI-subprocess harness does not bind a port)

**Tests:**
- `ac1_one_shot_haiku_streams_and_completes` — basic prompt streams text and emits Done
- `ac2_interactive_two_prompts_reuse_session` — session resume via captured sessionID
- `ac3_bogus_session_id_surfaces_error_path` — invalid `--session` yields error/Done
- `ac4_tool_use_visibility_with_agent_build` — agent=build triggers `HarnessEvent::ToolUse` with structured input/output
- `subcommand_models_returns_non_empty_output` — `run_subcommand(Models)` returns non-empty output mentioning at least one known provider (requires `opencode-authenticated`)

**Environment variables:**
- `TERMINUS_HAS_OPENCODE=1` — required to run any gated opencode integration test

## Claude harness — not currently gated

Claude integration tests rely on `claude-agent-sdk-rust` subprocess spawning, which works offline for unit tests (the SDK has a test mode). End-to-end tests with a live Claude Code instance are not currently in the tree.

## Known integration gaps (systemic)

These are NOT opencode-specific; they apply to any harness that wants end-to-end coverage:

- **Slack inbound attachments** — not forwarded to harnesses. Only text content is processed.
- **Discord inbound attachments** — same; `msg.content` text only.
- **Telegram inbound attachments** — forwarded as `Attachment { path, mime }`. The opencode harness threads these via `-f <path>`, but end-to-end chat→harness→file-analysis has no automated test yet.
- **State-persist-failure drift** — `state_tx.try_send` drops on full channel; in-memory HashMap state may diverge from on-disk. Cross-harness systemic issue, not opencode-specific. Tracked in `.omc/plans/opencode-harness-followups.md`.
- **Live integration on CI** — no CI job currently has `TERMINUS_HAS_OPENCODE=1` set. Gated tests are developer-local only.

## How to add a new gated integration test

Pattern:

```rust
#[tokio::test]
#[ignore]
async fn my_integration_test() {
    if std::env::var("TERMINUS_HAS_OPENCODE").is_err() {
        eprintln!("skip: TERMINUS_HAS_OPENCODE not set");
        return;
    }
    // ... test body
}
```

Always include the `#[ignore]` attribute AND the env-var guard, so the test is safe to exist in the tree without a live binary.
