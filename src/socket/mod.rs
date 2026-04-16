//! WebSocket bidirectional API for terminus.
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

pub mod config_watcher;
pub mod connection;
pub mod envelope;
pub mod events;
pub mod rate_limit;
pub mod subscription;

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use tokio::net::TcpListener;
use tokio::sync::{broadcast, mpsc};
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::http::StatusCode;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;

use crate::buffer::StreamEvent;
use crate::chat_adapters::IncomingMessage;
use crate::config::SocketConfig;

use self::config_watcher::SharedClientList;
use self::connection::SharedRateLimiters;
use self::envelope::Filter;
use self::events::AmbientEvent;

/// Shared per-client subscription store. Keyed by client name, stores the
/// subscription list from the last connection. In-memory only (not persisted
/// to disk — server restart clears all saved subscriptions).
pub type SharedSubscriptionStore = Arc<Mutex<HashMap<String, Vec<(String, Filter)>>>>;

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
    /// connections, and spawns per-connection tasks. Returns when cancelled
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

        loop {
            tokio::select! {
                biased;

                _ = self.cancel.cancelled() => {
                    tracing::info!("Socket server shutting down");
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

                    // Connection limit check: fetch_add before spawn to close the TOCTOU
                    // window between the check and the increment.
                    let prev = active_connections.fetch_add(1, Ordering::AcqRel);
                    if prev >= max_connections {
                        active_connections.fetch_sub(1, Ordering::Release);
                        tracing::warn!(
                            peer = %peer_addr,
                            "Socket connection rejected: max_connections ({}) reached",
                            max_connections
                        );
                        drop(tcp_stream);
                        continue;
                    }

                    // Clone config values needed by the connection
                    let config = self.config.clone();
                    let cmd_tx = self.cmd_tx.clone();
                    let stream_rx = self.stream_tx.subscribe();
                    let ambient_rx = self.ambient_tx.subscribe();
                    let cancel = self.cancel.child_token();
                    let active = Arc::clone(&active_connections);
                    let rate_limiters = Arc::clone(&shared_rate_limiters);
                    let cancel_tx = self.cancel_tx.clone();
                    // Snapshot the current client list atomically for this connection's auth
                    let clients_snapshot = self.shared_clients.load_full();
                    let shared_clients_live = Arc::clone(&self.shared_clients);
                    let shared_subs = Arc::clone(&self.shared_subs);

                    tokio::spawn(async move {
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
                                active.fetch_sub(1, Ordering::Release);
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

                        connection::run(
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

                        active.fetch_sub(1, Ordering::Release);
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
