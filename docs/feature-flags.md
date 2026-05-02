# Cargo feature flags

terminus is split into opt-in Cargo features so single-platform builds don't
compile unused dependencies. The default build still includes everything;
deployment-specific binaries can prune their dependency graph by passing
`--no-default-features --features <X>`.

## Available features

| Feature | Default? | Optional dep enabled | What it does |
|---|---|---|---|
| `telegram` | ✓ | `teloxide` | Compiles the Telegram chat adapter (`src/chat_adapters/telegram.rs`) and wires it into `App::set_platforms`. |
| `slack` | ✓ | `tokio-tungstenite` | Compiles the Slack adapter (`src/chat_adapters/slack.rs`), the typed `slack_platform` Arc, and the Slack catchup branch in `App::handle_gap`. |
| `discord` | ✓ | `serenity` | Compiles the Discord adapter (`src/chat_adapters/discord.rs`), the typed `discord_platform` Arc, and the Discord catchup branch in `App::handle_gap`. |
| `socket` | ✓ | `tokio-tungstenite` | Compiles the bidirectional WebSocket API (`src/socket/{mod,connection,config_watcher,envelope,rate_limit,subscription}.rs`). The `events` submodule (`AmbientEvent` type) is always-compiled because the harnesses fan out to it as a no-op when no socket subscribers are attached. |
| `integration-tests` | — | `wiremock` | Enables the wiremock-backed structured-output integration tests under `src/structured_output/webhook/integration_tests.rs`. |

`tokio-tungstenite` is shared between `slack` and `socket` — Cargo unifies the
optional dep when both features are active. There is intentionally no explicit
dependency between `slack` and `socket`; they enable the same crate
independently.

## Build matrix

| Command | Compiles | Use case |
|---|---|---|
| `cargo build` | All features | Default development build, full functionality. |
| `cargo build --release` | All features | Production single-binary with every platform enabled. |
| `cargo build --no-default-features --features telegram` | Telegram only | Telegram-only operator; binary is ~70 fewer crates. |
| `cargo build --no-default-features --features slack` | Slack only | Slack-only operator. |
| `cargo build --no-default-features --features discord` | Discord only | Discord-only operator. |
| `cargo build --no-default-features --features socket` | Socket API only | No chat platforms, only the bidirectional WebSocket interface. |
| `cargo build --no-default-features --features "telegram,socket"` | Telegram + WebSocket | Common shape: bot operator who also wants programmatic API access. |
| `cargo build --no-default-features` | Pure shared types | Used by CI to verify no required deps leaked into the always-on path. The resulting binary will fail at startup (`Config::validate` requires at least one platform) — this combo exists for compile-time validation only. |
| `cargo build --all-features` | Everything including dev features | Same as default plus `integration-tests`. |

## Functionality matrix

What ships with each feature combination?

| Capability | telegram | slack | discord | socket | empty |
|---|---|---|---|---|---|
| Process Telegram messages | ✓ | — | — | — | — |
| Process Slack messages | — | ✓ | — | — | — |
| Process Discord messages | — | — | ✓ | — | — |
| WebSocket bidirectional API | — | — | — | ✓ | — |
| Wake-recovery banner emission | always (lid/wake monitoring is unconditional) | | | | |
| Slack catchup on wake | — | ✓ | — | — | — |
| Discord catchup on wake | — | — | ✓ | — | — |
| `tmux` session management | always (orthogonal to features) | | | | |
| All four AI harnesses (claude/opencode/gemini/codex) | always | | | | |
| `PlatformType` enum, `IncomingMessage`, `ReplyContext` | always (wire-format stability) | | | | |
| State persistence (`StateStore`) | always | | | | |
| `AmbientEvent` bus (no-op fan-out without socket) | always | | | | |

## Runtime behaviour for misconfigured features

If `terminus.toml` contains `[telegram]` (or any other platform section) but
the binary was built without the matching feature, `Config::validate` emits a
WARN-level log line and continues:

```
WARN [telegram] section is configured but the 'telegram' feature was NOT
     compiled into this binary. Telegram messages will be ignored. Rebuild
     with --features telegram to enable Telegram routing.
```

The binary still starts (so a shared-config-across-binaries deployment isn't
broken), but the warning is loud enough to surface in the operator's logs.
The same applies to `slack`, `discord`, and `socket` features.

If NO platform is enabled at runtime AND socket isn't enabled, validation
fails hard with `bail!("At least one platform must be configured...")`.

## CI matrix

`.github/workflows/ci.yml` enforces the build matrix on every PR via two jobs:

- **`check`** — runs `cargo fmt --check`, `cargo clippy -D warnings`,
  `cargo test`, and `cargo audit` against the default feature set
  (all platforms enabled). This catches most regressions; the per-feature
  matrix below catches the rest.
- **`build-matrix`** — five parallel legs, one per feature combo
  (`telegram-only`, `slack-only`, `discord-only`, `socket-only`, `empty`).
  Non-empty legs run `cargo test --no-run` so that `#[cfg(test)]` code
  compiles too; the empty leg runs `cargo build`. Per-leg
  `Swatinem/rust-cache@v2` keys keep incremental artifacts isolated.
  `fail-fast: false` so a regression in one leg doesn't suppress signal
  from the others.

## Adding a new platform — contributor checklist

If you need to add a fifth platform (say, Mastodon), the following sites
need updates. Use existing platforms as the reference implementation.

1. **`Cargo.toml`** — add a feature entry and gate the dependency:
   ```toml
   [features]
   default = ["telegram", "slack", "discord", "mastodon", "socket"]
   mastodon = ["dep:megalodon"]  # or whatever crate

   [dependencies]
   megalodon = { version = "...", optional = true }
   ```
2. **`src/chat_adapters/mod.rs`** — gate the new submodule and any re-export:
   ```rust
   #[cfg(feature = "mastodon")]
   pub mod mastodon;
   ```
   Add the new variant to the `PlatformType` enum (this stays unconditional —
   wire-format stability) and to `PlatformMessageId`.
3. **`src/chat_adapters/mastodon.rs`** — implement the `ChatPlatform` trait.
4. **`src/main.rs`** — gate import + adapter construction block + the
   `app.set_platforms` call site:
   ```rust
   #[cfg(feature = "mastodon")]
   use chat_adapters::mastodon::MastodonAdapter;

   #[cfg(feature = "mastodon")]
   let mastodon: Option<Arc<dyn ChatPlatform>> = if config.mastodon_enabled() {
       /* ... build adapter ... */
   } else { None };
   #[cfg(not(feature = "mastodon"))]
   let mastodon: Option<Arc<dyn ChatPlatform>> = None;
   ```
5. **`src/app.rs`** — add the field, initialize it in `App::new`, gate any
   typed-Arc accessor (only needed if catchup is required), gate the
   `set_platforms` parameter, and gate any catchup branch in `handle_gap`.
6. **`src/config.rs`** — add `MastodonConfig`, `mastodon_enabled()`, and
   the cfg-gated WARN in `validate()` for "configured but not compiled in".
7. **`.github/workflows/ci.yml`** — add a `mastodon-only` matrix leg.
8. **`docs/feature-flags.md`** (this file) — add a row to the table above.

If your platform doesn't need a typed-Arc accessor for catchup (e.g. it has
no equivalent of Slack's `conversations.history` REST endpoint), step 5 is
shorter — only the dyn `Arc<dyn ChatPlatform>` field and `set_platforms`
parameter are needed.

## Why `default` ships every feature

A single-user bot operator usually wants all platforms available for testing
and switching between them. Defaulting to the full set means `cargo build`
just works for development and production. Operators who care about binary
size or supply-chain surface can opt out per-platform without learning
anything new about Cargo.

The trade-off: a CI change that breaks a non-default feature combo only
surfaces in the `build-matrix` job, never in `cargo test` with default
features. The build-matrix is the load-bearing safeguard for that gap.
