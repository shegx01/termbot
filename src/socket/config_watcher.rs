//! Config hot-reload watcher for socket client tokens.
//!
//! Watches `terminus.toml` for file-system changes and reloads the client
//! list when the file is modified. Only client tokens and names are reloaded;
//! bind address, port, and other structural config still require a restart.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::config::{Config, SocketClient};

use super::SharedSubscriptionStore;

/// Shared, atomically-swappable client list.
pub type SharedClientList = Arc<ArcSwap<Vec<SocketClient>>>;

/// Spawn a background task that watches `config_path` for changes and
/// reloads socket client entries into `shared_clients`.
///
/// Only `[[socket.client]]` entries are reloaded. Changes to bind, port,
/// max_connections, etc. are logged as warnings (require restart).
///
/// Watches the config FILE directly (not its parent directory) to avoid
/// the kqueue backend opening FDs for every sibling entry.  After an
/// editor atomic-save (rename), the kqueue watch is lost; we re-establish
/// it after each reload so subsequent edits are still detected.
///
/// `cancel` is a `CancellationToken` that terminates the watcher task
/// cooperatively on shutdown, releasing the kqueue/inotify file-descriptor.
pub(crate) fn spawn_config_watcher(
    config_path: PathBuf,
    shared_clients: SharedClientList,
    shared_subs: SharedSubscriptionStore,
    cancel: CancellationToken,
) -> Option<tokio::task::JoinHandle<()>> {
    let (fs_tx, mut fs_rx) = mpsc::channel::<()>(4);

    let mut watcher = match RecommendedWatcher::new(
        move |res: Result<notify::Event, notify::Error>| {
            if let Ok(event) = res {
                // Any vnode event on the config file (modify, rename, delete)
                // triggers a reload attempt.
                let dominated =
                    event.kind.is_modify() || event.kind.is_create() || event.kind.is_remove();
                if dominated {
                    let _ = fs_tx.try_send(());
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

    // Watch the config file directly — avoids kqueue opening FDs for every
    // entry in the parent directory (which caused FD exhaustion when the
    // parent was the project root containing target/, .git/, etc.).
    if let Err(e) = watcher.watch(&config_path, RecursiveMode::NonRecursive) {
        tracing::warn!(
            "Failed to watch config file {}: {} — hot-reload disabled",
            config_path.display(),
            e
        );
        return None;
    }

    let handle = tokio::spawn(async move {
        // Watcher must stay alive; it is also mutated to re-watch after renames.
        let mut watcher = watcher;

        loop {
            tokio::select! {
                biased;

                // Cooperative shutdown: release the kqueue/inotify watch FD.
                _ = cancel.cancelled() => {
                    tracing::debug!("Config watcher received cancellation, exiting");
                    break;
                }

                maybe_msg = fs_rx.recv() => {
                    match maybe_msg {
                        None => break, // sender dropped (shouldn't happen in normal operation)
                        Some(()) => {
                            // Debounce: sleep 500ms to collect rapid-fire events (editor
                            // write + rename), then drain and reload the final state.
                            // This prevents the old drain-then-process pattern that could
                            // permanently swallow valid changes arriving during reload.
                            //
                            // Cancel-aware: if shutdown is requested during the debounce
                            // window, exit immediately instead of blocking drain for 500ms.
                            tokio::select! {
                                biased;
                                _ = cancel.cancelled() => {
                                    tracing::debug!("Config watcher cancelled during debounce, exiting");
                                    break;
                                }
                                _ = tokio::time::sleep(Duration::from_millis(500)) => {}
                            }

                            // Drain any events that arrived during the sleep window
                            while fs_rx.try_recv().is_ok() {}

                            tracing::info!("Config file changed, attempting hot-reload of socket clients...");

                            // Re-watch BEFORE reload so the new inode is tracked while we
                            // parse.  Any edit arriving during reload queues another event
                            // for the next iteration (absorbed by the debounce).
                            if let Err(e) = watcher.unwatch(&config_path) {
                                tracing::debug!(
                                    "unwatch before re-watch returned error (expected after rename): {}",
                                    e
                                );
                            }
                            if let Err(e) = watcher.watch(&config_path, RecursiveMode::NonRecursive) {
                                tracing::warn!(
                                    "Failed to re-watch config file: {} — \
                                     subsequent config changes may not be detected until restart",
                                    e
                                );
                            }

                            match Config::load(&config_path) {
                                Ok(new_config) => {
                                    let new_clients = new_config.socket.clients;
                                    let old_clients = shared_clients.load();

                                    // Log what changed
                                    let old_names: HashSet<&str> =
                                        old_clients.iter().map(|c| c.name.as_str()).collect();
                                    let new_names_str: HashSet<&str> =
                                        new_clients.iter().map(|c| c.name.as_str()).collect();

                                    for name in new_names_str.difference(&old_names) {
                                        tracing::info!("Config hot-reload: added client '{}'", name);
                                    }
                                    for name in old_names.difference(&new_names_str) {
                                        tracing::info!("Config hot-reload: removed client '{}'", name);
                                    }
                                    for name in old_names.intersection(&new_names_str) {
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

                                    // Purge subscription store entries for removed clients.
                                    let retained: HashSet<String> =
                                        new_clients.iter().map(|c| c.name.clone()).collect();
                                    let old_owned: HashSet<String> =
                                        old_clients.iter().map(|c| c.name.clone()).collect();
                                    if retained != old_owned {
                                        let mut store = shared_subs.lock().unwrap_or_else(|poisoned| {
                                            tracing::warn!(target: "socket", "recovered from poisoned mutex");
                                            poisoned.into_inner()
                                        });
                                        store.purge_clients(&retained);
                                    }

                                    shared_clients.store(Arc::new(new_clients));
                                    tracing::info!("Socket client list hot-reloaded successfully");
                                }
                                Err(e) => {
                                    tracing::warn!("Config hot-reload failed (keeping old config): {}", e);
                                }
                            }
                        }
                    }
                }
            }
        }

        // Explicit drop releases the kqueue/inotify watch file-descriptor
        // so that shutdown probes (lsof, /proc/self/fd) see a clean state.
        drop(watcher);
    });

    Some(handle)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio_util::sync::CancellationToken;

    use super::*;

    /// AC-13 / AC-27: The config watcher task exits cooperatively within 500ms
    /// when the CancellationToken is cancelled.  Runs on both macOS (kqueue)
    /// and Linux (inotify) CI — exercises the platform-agnostic cancel path.
    #[tokio::test]
    async fn watcher_task_exits_on_cancel() {
        // Use a temp dir so we don't watch the real terminus.toml.
        let dir = tempfile::TempDir::new().expect("create temp dir");
        let config_path = dir.path().join("terminus.toml");
        std::fs::write(&config_path, b"[auth]\ntelegram_user_id = 1\n").expect("write temp config");

        let cancel = CancellationToken::new();
        let shared_clients: SharedClientList =
            Arc::new(arc_swap::ArcSwap::from_pointee(Vec::new()));
        let shared_subs: SharedSubscriptionStore = Arc::new(std::sync::Mutex::new(
            crate::socket::SubscriptionStoreInner::new(100),
        ));

        let handle = spawn_config_watcher(config_path, shared_clients, shared_subs, cancel.clone());

        let handle = handle.expect("watcher should start successfully");

        // Give the watcher a moment to enter its select! loop.
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Signal cancellation.
        cancel.cancel();

        // The watcher should exit within 500ms.
        let result = tokio::time::timeout(Duration::from_millis(500), handle).await;

        assert!(
            result.is_ok(),
            "watcher task should exit within 500ms of cancel signal"
        );
    }
}
