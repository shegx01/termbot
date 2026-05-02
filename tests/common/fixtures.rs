//! Test fixtures that build configured `App`s and channels for tests.

use std::path::Path;

use tokio::sync::mpsc;

use terminus::app::App;
use terminus::config::Config;
use terminus::state_store::{StateStore, StateUpdate};

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
