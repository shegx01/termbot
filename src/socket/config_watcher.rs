//! Config hot-reload watcher for socket client tokens.
//!
//! Watches `terminus.toml` for file-system changes and reloads the client
//! list when the file is modified. Only client tokens and names are reloaded;
//! bind address, port, and other structural config still require a restart.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::mpsc;

use crate::config::{Config, SocketClient};

/// Shared, atomically-swappable client list.
pub type SharedClientList = Arc<ArcSwap<Vec<SocketClient>>>;

/// Spawn a background task that watches `config_path` for changes and
/// reloads socket client entries into `shared_clients`.
///
/// Only `[[socket.client]]` entries are reloaded. Changes to bind, port,
/// max_connections, etc. are logged as warnings (require restart).
pub fn spawn_config_watcher(
    config_path: PathBuf,
    shared_clients: SharedClientList,
) -> Option<tokio::task::JoinHandle<()>> {
    let (fs_tx, mut fs_rx) = mpsc::channel::<()>(4);

    // Set up filesystem watcher — filter by config filename to avoid
    // spurious reloads from sibling file changes in the same directory.
    let watch_path = config_path.clone();
    let config_filename = watch_path
        .file_name()
        .map(|f| f.to_os_string())
        .unwrap_or_default();
    let mut watcher = match RecommendedWatcher::new(
        move |res: Result<notify::Event, notify::Error>| {
            if let Ok(event) = res {
                if event.kind.is_modify() || event.kind.is_create() {
                    // Only trigger for our config file, not sibling files
                    let matches = event
                        .paths
                        .iter()
                        .any(|p| p.file_name().map(|f| f == config_filename).unwrap_or(false));
                    if matches {
                        let _ = fs_tx.try_send(());
                    }
                }
            }
        },
        notify::Config::default(),
    ) {
        Ok(w) => w,
        Err(e) => {
            tracing::warn!(
                "Failed to create config file watcher: {} — hot-reload disabled",
                e
            );
            return None;
        }
    };

    // Watch the parent directory (handles atomic rename/replace by editors)
    let watch_dir = watch_path.parent().unwrap_or(watch_path.as_ref());
    if let Err(e) = watcher.watch(watch_dir.as_ref(), RecursiveMode::NonRecursive) {
        tracing::warn!(
            "Failed to watch config directory: {} — hot-reload disabled",
            e
        );
        return None;
    }

    let handle = tokio::spawn(async move {
        // Keep watcher alive for the lifetime of this task
        let _watcher = watcher;

        while fs_rx.recv().await.is_some() {
            // Debounce: sleep 500ms to collect rapid-fire events (editor
            // write + rename), then drain and reload the final state.
            // This prevents the old drain-then-process pattern that could
            // permanently swallow valid changes arriving during reload.
            tokio::time::sleep(Duration::from_millis(500)).await;

            // Drain any events that arrived during the sleep window
            while fs_rx.try_recv().is_ok() {}

            tracing::info!("Config file changed, attempting hot-reload of socket clients...");

            match Config::load(&config_path) {
                Ok(new_config) => {
                    let new_clients = new_config.socket.clients;
                    let old_clients = shared_clients.load();

                    // Log what changed
                    let old_names: std::collections::HashSet<&str> =
                        old_clients.iter().map(|c| c.name.as_str()).collect();
                    let new_names: std::collections::HashSet<&str> =
                        new_clients.iter().map(|c| c.name.as_str()).collect();

                    for name in new_names.difference(&old_names) {
                        tracing::info!("Config hot-reload: added client '{}'", name);
                    }
                    for name in old_names.difference(&new_names) {
                        tracing::info!("Config hot-reload: removed client '{}'", name);
                    }
                    for name in old_names.intersection(&new_names) {
                        let old_token = old_clients
                            .iter()
                            .find(|c| c.name == *name)
                            .map(|c| &c.token);
                        let new_token = new_clients
                            .iter()
                            .find(|c| c.name == *name)
                            .map(|c| &c.token);
                        if old_token != new_token {
                            tracing::info!(
                                "Config hot-reload: rotated token for client '{}'",
                                name
                            );
                        }
                    }

                    shared_clients.store(Arc::new(new_clients));
                    tracing::info!("Socket client list hot-reloaded successfully");
                }
                Err(e) => {
                    tracing::warn!("Config hot-reload failed (keeping old config): {}", e);
                }
            }
        }
    });

    Some(handle)
}
