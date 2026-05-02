//! WebSocket bidirectional API for terminus.
// Items in the socket module are used by the binary target (src/main.rs)
// and — since the integration-test PR — by the test crate at
// tests/socket_lifecycle.rs. The library target (src/lib.rs) compiles
// this module for `cargo test --lib` but does not call any socket API
// itself, so private and pub(crate) items still appear dead from the
// library's perspective. The crate-level allow keeps the compile clean
// without scattering per-item attributes.
#![allow(dead_code)]
//!
//! Exposes an authenticated, pipelined, subscription-capable WebSocket
//! endpoint for local programs and remote agents.
//!
//! Architecture: per-connection task model (Option A from consensus plan).
//! Each accepted connection spawns a `ConnectionTask` that owns its own
//! WebSocket stream, subscription registry, token bucket, and pending
//! request queue. Integration seams:
//!   1. Existing `broadcast::channel<StreamEvent>` for per-request events
//!   2. New `broadcast::channel<AmbientEvent>` for genuinely-new events
//!   3. Existing `mpsc::Sender<IncomingMessage>` for inbound command dispatch

// Socket submodules. `SocketServer`, `SubscriptionStoreInner`, and the
// `SharedSubscriptionStore` type alias are intentionally `pub` because
// `tests/socket_lifecycle.rs` constructs a real server on an ephemeral
// port — `pub(crate)` would be invisible to that test crate. Every other
// submodule item stays `pub(crate)` (preserved by the `mod imp` wrap).
// The library-target dead-code suppression below covers items that look
// dead from the lib perspective but are reachable from main.rs/tests.
//
// `events` stays unconditional because the ambient-event bus (AmbientEvent
// type) is referenced from `app.rs` and the harnesses regardless of whether
// the socket server itself is compiled in — the broadcast channel is a no-op
// fan-out when there are no socket subscribers.
#[cfg(feature = "socket")]
#[allow(dead_code)]
pub mod config_watcher;
#[cfg(feature = "socket")]
#[allow(dead_code)]
pub mod connection;
#[cfg(feature = "socket")]
pub mod envelope;
pub mod events;
#[cfg(feature = "socket")]
pub mod rate_limit;
#[cfg(feature = "socket")]
pub mod subscription;

// `SocketServer` and `SubscriptionStoreInner` are reachable from the binary
// target (`src/main.rs`) but the library target compiles socket internals only
// for `cargo test --lib`, never references them, so clippy flags the re-export
// as unused. The corresponding `#[allow(dead_code)]` on each submodule covers
// items but not re-export use statements; add the explicit allow here.
#[cfg(feature = "socket")]
#[allow(unused_imports)]
pub use imp::{SharedSubscriptionStore, SocketServer, SubscriptionStoreInner};

#[cfg(feature = "socket")]
mod imp {

    use std::collections::{HashMap, VecDeque};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    /// RAII guard that increments the connection counter on construction and
    /// decrements it on drop. Panic-safe: Drop runs even if the connection task
    /// panics, preventing a permanently-shrunk connection cap.
    struct ConnectionGuard {
        counter: Arc<AtomicUsize>,
    }

    impl ConnectionGuard {
        #[cfg(test)]
        fn new(counter: Arc<AtomicUsize>) -> Self {
            counter.fetch_add(1, Ordering::SeqCst);
            Self { counter }
        }

        /// Try to acquire a guard slot. Returns None if the counter is already at
        /// or above `max`. Uses a CAS loop so the increment is atomic with the
        /// limit check (closes the TOCTOU window between load + spawn).
        fn try_new(counter: Arc<AtomicUsize>, max: usize) -> Option<Self> {
            let mut current = counter.load(Ordering::Acquire);
            loop {
                if current >= max {
                    return None;
                }
                match counter.compare_exchange_weak(
                    current,
                    current + 1,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                ) {
                    Ok(_) => return Some(Self { counter }),
                    Err(actual) => current = actual,
                }
            }
        }
    }

    impl Drop for ConnectionGuard {
        fn drop(&mut self) {
            self.counter.fetch_sub(1, Ordering::SeqCst);
        }
    }

    use anyhow::Result;
    use tokio::net::TcpListener;
    use tokio::sync::{broadcast, mpsc};
    use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
    use tokio_tungstenite::tungstenite::http::StatusCode;
    use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;

    use crate::buffer::StreamEvent;
    use crate::chat_adapters::IncomingMessage;
    use crate::config::SocketConfig;

    use super::config_watcher::SharedClientList;
    use super::connection::SharedRateLimiters;
    use super::envelope::Filter;
    use super::events::AmbientEvent;

    /// LRU-bounded per-client subscription store.
    ///
    /// Keyed by client name, stores the subscription list from the last connection.
    /// In-memory only — server restart clears all saved subscriptions.
    ///
    /// The `order` deque tracks LRU order: back == most recently used. When the
    /// store is at capacity and a new (unknown) client is saved, the front entry
    /// (least recently used) is evicted. `map` and `order` are always kept in sync:
    /// a key present in one is present in the other, and vice versa.
    pub struct SubscriptionStoreInner {
        map: HashMap<String, Vec<(String, Filter)>>,
        order: VecDeque<String>,
        cap: usize,
    }

    impl SubscriptionStoreInner {
        pub fn new(cap: usize) -> Self {
            Self {
                map: HashMap::new(),
                order: VecDeque::new(),
                cap,
            }
        }

        /// Upsert subscriptions for `client_name`, bumping to most-recently-used.
        /// If the store is at capacity and `client_name` is new, the LRU entry is
        /// evicted first.
        pub(crate) fn save(&mut self, client_name: String, subs: Vec<(String, Filter)>) {
            if self.map.contains_key(&client_name) {
                // Bump: remove from current position, push to back.
                self.order.retain(|k| k != &client_name);
            } else {
                // New entry: evict LRU if at capacity.
                if self.map.len() >= self.cap {
                    if let Some(evicted) = self.order.pop_front() {
                        self.map.remove(&evicted);
                    }
                }
            }
            self.order.push_back(client_name.clone());
            self.map.insert(client_name, subs);
        }

        /// Look up subscriptions for `client_name`, bumping to most-recently-used.
        pub(crate) fn restore(&mut self, client_name: &str) -> Option<&Vec<(String, Filter)>> {
            if self.map.contains_key(client_name) {
                // Bump to most-recently-used position.
                self.order.retain(|k| k != client_name);
                self.order.push_back(client_name.to_string());
                self.map.get(client_name)
            } else {
                None
            }
        }

        /// Remove a client's subscriptions entirely (e.g. on unsubscribe or config removal).
        pub(crate) fn remove(&mut self, client_name: &str) {
            if self.map.remove(client_name).is_some() {
                self.order.retain(|k| k != client_name);
            }
        }

        /// Purge all clients whose names are not in `retained`. Called after a
        /// config hot-reload that removed one or more `[[socket.client]]` entries.
        pub(crate) fn purge_clients(&mut self, retained: &std::collections::HashSet<String>) {
            let to_remove: Vec<String> = self
                .map
                .keys()
                .filter(|k| !retained.contains(*k))
                .cloned()
                .collect();
            for key in to_remove {
                self.map.remove(&key);
            }
            self.order.retain(|k| retained.contains(k));
        }

        /// Number of clients currently in the store.
        #[allow(dead_code)]
        pub(crate) fn len(&self) -> usize {
            self.map.len()
        }

        /// True when the store contains no entries.
        #[allow(dead_code)]
        pub fn is_empty(&self) -> bool {
            self.map.is_empty()
        }
    }

    /// Shared, thread-safe handle to the LRU subscription store.
    pub type SharedSubscriptionStore = Arc<Mutex<SubscriptionStoreInner>>;

    /// The socket server: binds a TCP listener, accepts connections, authenticates
    /// via Bearer token at HTTP upgrade, and spawns per-connection tasks.
    pub struct SocketServer {
        config: SocketConfig,
        cmd_tx: mpsc::Sender<IncomingMessage>,
        stream_tx: broadcast::Sender<StreamEvent>,
        ambient_tx: broadcast::Sender<AmbientEvent>,
        cancel: tokio_util::sync::CancellationToken,
        cancel_tx: mpsc::Sender<String>,
        shared_clients: SharedClientList,
        shared_subs: SharedSubscriptionStore,
    }

    impl SocketServer {
        /// Construct a new socket server.
        ///
        /// # Preconditions (callers from outside the binary, e.g. tests)
        ///
        /// - `cancel` must NOT already be cancelled — `run` would exit
        ///   immediately on the first poll.
        /// - `stream_tx` and `ambient_tx` broadcast senders must outlive
        ///   `run`; otherwise per-request and ambient subscribers receive
        ///   `RecvError::Closed`.
        /// - `cmd_tx`'s receiver must remain open; a dropped receiver
        ///   silently discards every inbound socket command.
        /// - `shared_clients` may be empty (auth gate rejects all 401);
        ///   when populated, every `SocketClient.token` should be at
        ///   least 32 characters — the same minimum `Config::validate`
        ///   enforces. A `debug_assert!` checks this in test builds.
        ///
        /// `run()` consumes `self` and must be called at most once. Concurrent
        /// invocation is undefined.
        #[allow(clippy::too_many_arguments)]
        pub fn new(
            config: SocketConfig,
            cmd_tx: mpsc::Sender<IncomingMessage>,
            stream_tx: broadcast::Sender<StreamEvent>,
            ambient_tx: broadcast::Sender<AmbientEvent>,
            cancel: tokio_util::sync::CancellationToken,
            cancel_tx: mpsc::Sender<String>,
            shared_clients: SharedClientList,
            shared_subs: SharedSubscriptionStore,
        ) -> Self {
            // Mirror the `Config::validate` 32-char-minimum invariant so a
            // direct `SocketServer::new` call cannot bypass it. Test-builds
            // panic immediately; release builds skip this check (the auth
            // gate's `constant_time_eq` still rejects mismatches at the
            // request level).
            debug_assert!(
                shared_clients.load().iter().all(|c| c.token.len() >= 32),
                "every SocketClient.token must be at least 32 characters \
                 (matching Config::validate's minimum)"
            );
            Self {
                config,
                cmd_tx,
                stream_tx,
                ambient_tx,
                cancel,
                cancel_tx,
                shared_clients,
                shared_subs,
            }
        }

        /// Run the socket server. Binds to `config.bind:config.port`, accepts
        /// connections, and spawns per-connection tasks via a `JoinSet` so that
        /// shutdown can drain them with a bounded timeout. Returns when cancelled
        /// or on bind failure.
        pub async fn run(self) -> Result<()> {
            let addr = format!("{}:{}", self.config.bind, self.config.port);
            let bind_addr = &self.config.bind;
            let listener = match TcpListener::bind(&addr).await {
                Ok(l) => {
                    tracing::info!("Socket server listening on ws://{}", addr);
                    l
                }
                Err(e) => {
                    tracing::warn!(
                        "Socket server failed to bind to {}: {} — continuing without socket",
                        addr,
                        e
                    );
                    return Ok(());
                }
            };

            if bind_addr != "127.0.0.1" && bind_addr != "::1" && bind_addr != "localhost" {
                tracing::warn!(
                    bind = %bind_addr,
                    "socket server binding to non-loopback address — Bearer tokens will be \
                     transmitted in plaintext unless a TLS-terminating reverse proxy is in front"
                );
            }

            let active_connections = Arc::new(AtomicUsize::new(0));
            let shared_rate_limiters: SharedRateLimiters =
                Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
            let max_connections = self.config.max_connections;
            let shutdown_drain_secs = self.config.shutdown_drain_secs;

            // JoinSet owns all per-connection tasks so we can drain them on shutdown.
            let mut tasks: tokio::task::JoinSet<()> = tokio::task::JoinSet::new();

            loop {
                tokio::select! {
                    biased;

                    _ = self.cancel.cancelled() => {
                        tracing::info!("Socket server shutting down, draining {} connections", tasks.len());
                        // Drain the JoinSet with a bounded timeout.
                        let drain_deadline = tokio::time::sleep(Duration::from_secs(shutdown_drain_secs));
                        tokio::pin!(drain_deadline);
                        loop {
                            tokio::select! {
                                biased;
                                _ = &mut drain_deadline => {
                                    tracing::warn!(
                                        "Socket connection drain timed out after {}s — aborting {} remaining",
                                        shutdown_drain_secs,
                                        tasks.len()
                                    );
                                    tasks.abort_all();
                                    // Collect aborted task results (required to free resources).
                                    while tasks.join_next().await.is_some() {}
                                    break;
                                }
                                next = tasks.join_next() => {
                                    match next {
                                        Some(_) => continue,
                                        None => break,
                                    }
                                }
                            }
                        }
                        return Ok(());
                    }

                    accept = listener.accept() => {
                        let (tcp_stream, peer_addr) = match accept {
                            Ok(pair) => pair,
                            Err(e) => {
                                tracing::warn!("Socket accept error: {}", e);
                                continue;
                            }
                        };

                        // Connection limit check: CAS loop so increment is atomic with
                        // the limit check, closing the TOCTOU window between load + spawn.
                        let conn_guard = match ConnectionGuard::try_new(
                            Arc::clone(&active_connections),
                            max_connections,
                        ) {
                            Some(g) => g,
                            None => {
                                tracing::warn!(
                                    peer = %peer_addr,
                                    "Socket connection rejected: max_connections ({}) reached",
                                    max_connections
                                );
                                drop(tcp_stream);
                                continue;
                            }
                        };

                        // Clone config values needed by the connection
                        let config = self.config.clone();
                        let cmd_tx = self.cmd_tx.clone();
                        let stream_rx = self.stream_tx.subscribe();
                        let ambient_rx = self.ambient_tx.subscribe();
                        let cancel = self.cancel.child_token();
                        let rate_limiters = Arc::clone(&shared_rate_limiters);
                        let cancel_tx = self.cancel_tx.clone();
                        // Snapshot the current client list atomically for this connection's auth
                        let clients_snapshot = self.shared_clients.load_full();
                        let shared_clients_live = Arc::clone(&self.shared_clients);
                        let shared_subs = Arc::clone(&self.shared_subs);

                        tasks.spawn(async move {
                            // RAII guard: holds the slot for the lifetime of this task;
                            // decrements on drop (even on panic).
                            let _conn_guard = conn_guard;

                            // Authenticate at HTTP upgrade via Bearer token
                            let mut authenticated_client: Option<String> = None;
                            // WS frame limit must accommodate the larger of text and
                            // binary limits so the tungstenite layer doesn't reject
                            // binary frames before our application handler sees them.
                            let ws_max = std::cmp::max(
                                config.max_message_bytes,
                                config.max_binary_bytes,
                            );

                            let ws_config = WebSocketConfig {
                                max_message_size: Some(ws_max),
                                max_frame_size: Some(ws_max),
                                ..Default::default()
                            };

                            #[allow(clippy::result_large_err)]
                            let ws_stream = match tokio_tungstenite::accept_hdr_async_with_config(
                                tcp_stream,
                                |req: &Request, response: Response| -> Result<Response, ErrorResponse> {
                                    // Extract Authorization header
                                    let auth_header = req
                                        .headers()
                                        .get("authorization")
                                        .and_then(|v| v.to_str().ok());

                                    let token = auth_header
                                        .and_then(|h| h.strip_prefix("Bearer "))
                                        .or_else(|| auth_header.and_then(|h| h.strip_prefix("bearer ")));

                                    match token {
                                        Some(t) => {
                                            // Fix #1: constant-time token comparison
                                            if let Some(client) = clients_snapshot.iter().find(|c| constant_time_eq(c.token.as_bytes(), t.as_bytes())) {
                                                authenticated_client = Some(client.name.clone());
                                                Ok(response)
                                            } else {
                                                tracing::warn!(peer = %peer_addr, "Socket auth failed: unknown token");
                                                let mut err = ErrorResponse::new(None);
                                                *err.status_mut() = StatusCode::UNAUTHORIZED;
                                                Err(err)
                                            }
                                        }
                                        None => {
                                            tracing::warn!(peer = %peer_addr, "Socket auth failed: missing Authorization header");
                                            let mut err = ErrorResponse::new(None);
                                            *err.status_mut() = StatusCode::UNAUTHORIZED;
                                            Err(err)
                                        }
                                    }
                                },
                                Some(ws_config),
                            )
                            .await
                            {
                                Ok(ws) => ws,
                                Err(e) => {
                                    tracing::debug!(peer = %peer_addr, error = %e, "WebSocket upgrade failed");
                                    return;
                                }
                            };

                            let client_name = authenticated_client.unwrap_or_else(|| "unknown".into());
                            let session_id = ulid::Ulid::new().to_string();

                            tracing::info!(
                                client = %client_name,
                                session = %session_id,
                                peer = %peer_addr,
                                "Socket connection established"
                            );

                            super::connection::run(
                                ws_stream,
                                client_name.clone(),
                                session_id.clone(),
                                cmd_tx,
                                stream_rx,
                                ambient_rx,
                                cancel,
                                config,
                                rate_limiters,
                                cancel_tx,
                                shared_subs,
                                shared_clients_live,
                            )
                            .await;

                            tracing::info!(
                                client = %client_name,
                                session = %session_id,
                                "Socket connection ended"
                            );
                        });
                    }
                }
            }
        }
    }

    /// Constant-time byte comparison to prevent timing side-channel attacks
    /// on token authentication. Always iterates over `max(a.len(), b.len())`
    /// bytes, padding the shorter input with zeros, so the loop count is
    /// determined by the longer input. Length mismatch is XOR-folded into the
    /// accumulator.
    fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
        let len_diff = (a.len() != b.len()) as u8;
        let max_len = std::cmp::max(a.len(), b.len());
        let mut acc = len_diff;
        for i in 0..max_len {
            let x = if i < a.len() { a[i] } else { 0 };
            let y = if i < b.len() { b[i] } else { 0 };
            acc |= x ^ y;
        }
        acc == 0
    }

    #[cfg(test)]
    mod tests {
        use std::sync::atomic::{AtomicUsize, Ordering};

        use super::*;

        // ── SubscriptionStoreInner / LRU tests (AC-9, AC-10, AC-11, AC-12, AC-13) ─

        fn make_subs(label: &str) -> Vec<(String, Filter)> {
            vec![(label.to_string(), Filter::default())]
        }

        /// AC-9: LRU evicts the oldest (first-inserted, never-accessed) entry when
        /// the store is filled past capacity.
        #[test]
        fn lru_evicts_oldest_when_past_cap() {
            let mut store = SubscriptionStoreInner::new(2);
            store.save("a".into(), make_subs("a"));
            store.save("b".into(), make_subs("b"));
            store.save("c".into(), make_subs("c")); // "a" is LRU; should be evicted

            assert_eq!(store.len(), 2, "store should be capped at 2");
            assert!(
                store.restore("a").is_none(),
                "oldest entry 'a' should have been evicted"
            );
            assert!(store.restore("b").is_some(), "'b' should remain");
            assert!(store.restore("c").is_some(), "'c' should remain");
        }

        /// AC-10: Saving to an existing key bumps it to most-recently-used, so it
        /// survives the next eviction cycle.
        #[test]
        fn lru_bumps_on_save_for_existing_key() {
            let mut store = SubscriptionStoreInner::new(2);
            store.save("a".into(), make_subs("a")); // order: [a]
            store.save("b".into(), make_subs("b")); // order: [a, b]
            store.save("a".into(), make_subs("a-v2")); // bump a → order: [b, a]
            store.save("c".into(), make_subs("c")); // "b" is LRU; should be evicted

            assert_eq!(store.len(), 2);
            assert!(store.restore("b").is_none(), "'b' should have been evicted");
            assert!(store.restore("a").is_some(), "'a' should remain");
            assert!(store.restore("c").is_some(), "'c' should remain");
        }

        /// AC-11: Restoring (looking up) a key bumps it to most-recently-used.
        #[test]
        fn lru_bumps_on_restore() {
            let mut store = SubscriptionStoreInner::new(2);
            store.save("a".into(), make_subs("a")); // order: [a]
            store.save("b".into(), make_subs("b")); // order: [a, b]
            let _ = store.restore("a"); // bump a → order: [b, a]
            store.save("c".into(), make_subs("c")); // "b" is LRU; should be evicted

            assert_eq!(store.len(), 2);
            assert!(store.restore("b").is_none(), "'b' should have been evicted");
            assert!(store.restore("a").is_some(), "'a' should remain");
            assert!(store.restore("c").is_some(), "'c' should remain");
        }

        /// AC-12: `purge_clients` removes entries whose names are not in the
        /// retained set, and keeps the map and order deque in sync.
        #[test]
        fn purge_clients_removes_unretained_keys() {
            let mut store = SubscriptionStoreInner::new(10);
            store.save("a".into(), make_subs("a"));
            store.save("b".into(), make_subs("b"));
            store.save("c".into(), make_subs("c"));

            let retained: std::collections::HashSet<String> = ["b".to_string()].into();
            store.purge_clients(&retained);

            assert_eq!(store.len(), 1, "only 'b' should remain");
            assert!(store.restore("a").is_none(), "'a' should be purged");
            assert!(store.restore("b").is_some(), "'b' should remain");
            assert!(store.restore("c").is_none(), "'c' should be purged");
        }

        /// AC-13: Explicit remove drops the entry from both map and order deque.
        #[test]
        fn save_empty_removes_via_remove_method() {
            let mut store = SubscriptionStoreInner::new(10);
            store.save("a".into(), make_subs("a"));
            store.save("b".into(), make_subs("b"));
            store.remove("a");

            assert_eq!(store.len(), 1, "only 'b' should remain after remove");
            assert!(store.restore("a").is_none(), "'a' should be gone from map");
            // Verify order deque is also clean by filling to cap (no phantom 'a' slot)
            store.save("c".into(), make_subs("c")); // cap=10, should not evict
            assert!(store.restore("b").is_some());
            assert!(store.restore("c").is_some());
        }

        // ── ConnectionGuard tests (AC-19, AC-20) ──────────────────────────────────

        /// AC-19: ConnectionGuard increments counter on construction and decrements
        /// on drop, leaving the counter at its original value.
        #[test]
        fn connection_guard_decrements_on_drop() {
            let counter = Arc::new(AtomicUsize::new(0));
            assert_eq!(counter.load(Ordering::SeqCst), 0);

            let guard = ConnectionGuard::new(Arc::clone(&counter));
            assert_eq!(
                counter.load(Ordering::SeqCst),
                1,
                "guard construction should increment counter to 1"
            );

            drop(guard);
            assert_eq!(
                counter.load(Ordering::SeqCst),
                0,
                "guard drop should decrement counter back to 0"
            );
        }

        /// AC-20: ConnectionGuard decrement runs even when the task that holds it
        /// panics, keeping the counter accurate.
        #[tokio::test]
        async fn connection_guard_panic_safe() {
            let counter = Arc::new(AtomicUsize::new(0));
            let counter_clone = Arc::clone(&counter);

            let handle = tokio::task::spawn_blocking(move || {
                let _guard = ConnectionGuard::new(Arc::clone(&counter_clone));
                // counter is now 1 inside this closure.
                panic!("intentional panic to test RAII guard");
            });

            // The spawned task should have panicked.
            let result = handle.await;
            assert!(result.is_err(), "task should have panicked");

            // Even after the panic, Drop should have run and decremented the counter.
            assert_eq!(
                counter.load(Ordering::SeqCst),
                0,
                "counter should be 0 after panic unwind — Drop must have run"
            );
        }

        #[test]
        fn connection_guard_try_new_respects_max() {
            use std::sync::atomic::Ordering;
            let counter = Arc::new(AtomicUsize::new(0));
            let g1 = ConnectionGuard::try_new(counter.clone(), 2).expect("first slot");
            let g2 = ConnectionGuard::try_new(counter.clone(), 2).expect("second slot");
            assert_eq!(counter.load(Ordering::SeqCst), 2);
            assert!(
                ConnectionGuard::try_new(counter.clone(), 2).is_none(),
                "third should fail"
            );
            assert_eq!(
                counter.load(Ordering::SeqCst),
                2,
                "failed try_new must not increment"
            );
            drop(g1);
            assert_eq!(counter.load(Ordering::SeqCst), 1);
            drop(g2);
            assert_eq!(counter.load(Ordering::SeqCst), 0);
        }
    }
} // mod imp
