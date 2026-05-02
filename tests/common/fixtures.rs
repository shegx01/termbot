//! Test fixtures that build configured `App`s and channels for tests.
//!
//! NOTE: The `test_config` / `test_app` here intentionally mirror
//! `make_test_config` / `make_app` in `src/app.rs` (around line 1814).
//! When `App::new` or its required config sections change, BOTH copies
//! must be updated — the integration-test target compiles the lib without
//! `cfg(test)`, so it cannot reuse the in-module helpers directly.

use std::path::Path;
use std::sync::Arc;

use terminus::app::App;
use terminus::buffer::StreamEvent;
use terminus::chat_adapters::ChatPlatform;
use terminus::config::Config;
use terminus::delivery::spawn_delivery_task;
use terminus::state_store::{StateStore, StateUpdate};
use tokio::sync::mpsc;

use super::mocks::MockPlatform;

/// Write a minimal Telegram-only config TOML to `dir/terminus.toml` and load
/// it. The returned config has telegram auth + token populated and an empty
/// blocklist.
pub fn test_config(dir: &Path) -> Config {
    let toml_path = dir.join("terminus.toml");
    std::fs::write(
        &toml_path,
        r#"
[auth]
telegram_user_id = 12345

[telegram]
bot_token = "test_token_that_is_not_real"

[blocklist]
patterns = []
"#,
    )
    .expect("write test config");
    Config::load(&toml_path).expect("load test config")
}

/// Build an `App` for integration tests, returning the app paired with the
/// state-update receiver so tests can observe persistence intent.
///
/// `dir` should be a `tempfile::tempdir()` path that owns the state file
/// for the duration of the test.
pub fn test_app(dir: &Path) -> (App, mpsc::Receiver<StateUpdate>) {
    let config = test_config(dir);
    let state_path = dir.join("terminus-state.json");
    let store = StateStore::load(&state_path).expect("load state store");
    let (state_tx, state_rx) = mpsc::channel::<StateUpdate>(64);
    let app = App::new(&config, store, state_tx).expect("App::new");
    (app, state_rx)
}

/// Wire a `MockPlatform::telegram()` into `app` and spawn the production
/// delivery task. Returns the `Arc<MockPlatform>` for assertion access.
///
/// Centralizes the cfg-gating dance for `App::set_platforms` — every
/// feature combination (`telegram-only`, `slack-only`, etc.) compiles the
/// signature differently, and the per-feature CI matrix would otherwise
/// reject inline call sites that hardcode 5 arguments.
pub fn wire_telegram_mock(app: &mut App) -> Arc<MockPlatform> {
    wire_mock_inner(app, MockPlatform::telegram())
}

/// Same as `wire_telegram_mock` but with a `MockPlatform::telegram_blocking()`
/// — used by the banner-ack-timeout fallback test.
pub fn wire_telegram_blocking_mock(app: &mut App) -> Arc<MockPlatform> {
    wire_mock_inner(app, MockPlatform::telegram_blocking())
}

fn wire_mock_inner(app: &mut App, mock: Arc<MockPlatform>) -> Arc<MockPlatform> {
    let dyn_mock: Arc<dyn ChatPlatform> = mock.clone();
    let stream_rx = app.subscribe_stream();
    let acks = app.pending_banner_acks_handle();
    let prefixes = app.gap_prefix_handle();
    spawn_delivery_task(Arc::clone(&dyn_mock), stream_rx, acks, prefixes);
    set_telegram_only_platform(app, dyn_mock);
    mock
}

/// `App::set_platforms` parameter list shifts under `#[cfg(feature = ...)]`.
/// This helper hides the matrix so test files compile in every feature
/// combination without per-test cfg sprinkles.
fn set_telegram_only_platform(app: &mut App, telegram: Arc<dyn ChatPlatform>) {
    app.set_platforms(
        Some(telegram),
        None,
        #[cfg(feature = "slack")]
        None,
        None,
        #[cfg(feature = "discord")]
        None,
    );
}

/// Subscribe to the stream channel — used by tests that need to assert on
/// raw `StreamEvent` payloads rather than going through delivery.
pub fn subscribe_stream(app: &App) -> tokio::sync::broadcast::Receiver<StreamEvent> {
    app.subscribe_stream()
}
