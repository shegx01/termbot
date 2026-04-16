//! Per-connection task for the terminus WebSocket server.
//!
//! Each accepted connection spawns one of these tasks. It owns:
//! - The WebSocket stream
//! - A subscription registry
//! - A token bucket rate limiter
//! - A pending-request map (for correlation + cancel)
//!
//! Inbound commands are forwarded to the main loop via `cmd_tx`.
//! Responses route back through a per-request `mpsc::UnboundedSender<String>`
//! stored in `ReplyContext::socket_reply_tx`.
//!
//! Reply delivery uses channel-close-as-end semantics: when `App::handle_command`
//! finishes, the `IncomingMessage` (and its `ReplyContext` with `socket_reply_tx`)
//! drops, closing the sender. The drain loop detects `Disconnected` and emits
//! a single `result` (with all accumulated text) + `end` frame.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use futures_util::{SinkExt, StreamExt};
use tokio::sync::{broadcast, mpsc};
use tokio::time::interval;
use tokio_tungstenite::tungstenite::Message;

use crate::buffer::StreamEvent;
use crate::chat_adapters::{Attachment, IncomingMessage, PlatformType, ReplyContext};
use crate::config::SocketConfig;

use super::config_watcher::SharedClientList;
use super::envelope::{ErrorCode, InboundEnvelope, OutboundEnvelope};
use super::events::AmbientEvent;
use super::rate_limit::TokenBucket;
use super::subscription::SubscriptionRegistry;

/// Shared per-client rate limiter. Keyed by client name so reconnecting
/// clients share the same token bucket (prevents rate-limit bypass via reconnect).
pub type SharedRateLimiters = Arc<Mutex<HashMap<String, TokenBucket>>>;

/// State for a pending binary frame after receiving an `attachment_meta` envelope.
/// Cleared once the binary frame arrives or the 30-second timeout elapses.
struct PendingBinary {
    request_id: String,
    filename: String,
    content_type: String,
    received_at: Instant,
}

/// State for one pending (in-flight) request.
struct PendingRequest {
    /// Accumulated reply text chunks (may be multiple from split_message).
    chunks: Vec<String>,
    /// Receives reply text from App::send_reply via socket_reply_tx.
    reply_rx: mpsc::UnboundedReceiver<String>,
    /// Whether the client has cancelled this request.
    cancelled: bool,
}

/// Run a single WebSocket connection to completion.
///
/// This is spawned by the listener for each accepted + authenticated client.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    ws_stream: tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
    client_name: String,
    session_id: String,
    cmd_tx: mpsc::Sender<IncomingMessage>,
    mut stream_rx: broadcast::Receiver<StreamEvent>,
    mut ambient_rx: broadcast::Receiver<AmbientEvent>,
    cancel: tokio_util::sync::CancellationToken,
    config: SocketConfig,
    shared_rate_limiters: SharedRateLimiters,
    cancel_tx: mpsc::Sender<String>,
    shared_subs: super::SharedSubscriptionStore,
    shared_clients: SharedClientList,
) {
    let (mut ws_sink, mut ws_stream_read) = ws_stream.split();

    // Per-connection state
    let mut subs = SubscriptionRegistry::new(config.max_subscriptions_per_connection);
    let mut pending: HashMap<String, PendingRequest> = HashMap::new();
    let mut pending_binary: Option<PendingBinary> = None;
    let mut hello_received = false;
    let mut last_activity = Instant::now();
    // Pong tracking: close connection if pong not received within timeout after ping
    let mut last_pong = Instant::now();
    let mut ping_outstanding = false;
    let pong_timeout = Duration::from_secs(config.pong_timeout_secs);

    // Shared per-client rate limiter (survives reconnects)
    // Insert a fresh bucket if this client doesn't have one yet.
    {
        let mut limiters = shared_rate_limiters
            .lock()
            .expect("rate limiter mutex poisoned");
        limiters.entry(client_name.clone()).or_insert_with(|| {
            TokenBucket::new(config.rate_limit_burst, config.rate_limit_per_second)
        });
    }

    // Timers
    let mut ping_ticker = interval(Duration::from_secs(config.ping_interval_secs));
    let mut reply_poll = interval(Duration::from_millis(50));
    let idle_timeout = Duration::from_secs(config.idle_timeout_secs);

    // Send hello_ack immediately on connection
    let hello_ack = OutboundEnvelope::HelloAck {
        session_id: session_id.clone(),
        client_name: client_name.clone(),
        protocol: "terminus/v1".to_string(),
        capabilities: vec![
            "pipelining".to_string(),
            "subscriptions".to_string(),
            "cancel".to_string(),
        ],
    };
    if send_envelope(&mut ws_sink, &hello_ack).await.is_err() {
        // Save subscriptions even on early exit (may have been restored via hello)
        save_subscriptions(&subs, &client_name, &shared_subs);
        return;
    }

    loop {
        // Drain pending reply channels. Uses channel-close-as-end semantics:
        // accumulate all text chunks; when the sender is dropped (command handler
        // returned), emit a single `result` with joined text + a single `end`.
        if drain_pending(&mut pending, &mut ws_sink).await.is_err() {
            break; // WS write error
        }

        tokio::select! {
            biased;

            // 1. Cancellation (shutdown)
            _ = cancel.cancelled() => {
                let env = OutboundEnvelope::ShuttingDown {
                    drain_deadline_ms: config.shutdown_drain_secs * 1000,
                };
                let _ = send_envelope(&mut ws_sink, &env).await;
                // Bounded drain: give pending requests up to shutdown_drain_secs
                // to complete rather than closing immediately.
                let drain_deadline = tokio::time::Instant::now()
                    + Duration::from_secs(config.shutdown_drain_secs);
                while !pending.is_empty() {
                    let remaining = drain_deadline.saturating_duration_since(tokio::time::Instant::now());
                    if remaining.is_zero() {
                        break;
                    }
                    match tokio::time::timeout(remaining, reply_poll.tick()).await {
                        Ok(_) => {
                            if drain_pending(&mut pending, &mut ws_sink).await.is_err() {
                                break;
                            }
                        }
                        Err(_) => break, // deadline exceeded
                    }
                }
                let _ = ws_sink.close().await;
                tracing::info!(client = %client_name, "socket connection closed (shutdown)");
                break;
            }

            // 2. Inbound WebSocket frame
            frame = ws_stream_read.next() => {
                match frame {
                    Some(Ok(Message::Text(text))) => {
                        last_activity = Instant::now();

                        // If a binary frame was expected (attachment_meta sent) but
                        // a text frame arrived instead, cancel the pending binary.
                        // This prevents stale state from correlating a later binary
                        // frame with the wrong attachment_meta.
                        if let Some(stale) = pending_binary.take() {
                            tracing::warn!(
                                client = %client_name,
                                request_id = %stale.request_id,
                                "text frame received while binary frame was expected — \
                                 cancelling pending attachment"
                            );
                            let _ = send_envelope(&mut ws_sink, &OutboundEnvelope::Error {
                                request_id: Some(stale.request_id),
                                code: ErrorCode::AttachmentTimeout,
                                message: "Text frame received while binary frame was expected".to_string(),
                                retry_after_ms: None,
                            }).await;
                        }

                        // Max message size check
                        if text.len() > config.max_message_bytes {
                            let _ = send_envelope(&mut ws_sink, &OutboundEnvelope::Error {
                                request_id: None,
                                code: ErrorCode::MessageTooLarge,
                                message: format!("Message exceeds {} bytes", config.max_message_bytes),
                                retry_after_ms: None,
                            }).await;
                            let _ = ws_sink.close().await;
                            break;
                        }

                        // Rate limit check (shared per-client bucket)
                        let rate_result = {
                            let mut limiters = shared_rate_limiters
                                .lock()
                                .expect("rate limiter mutex poisoned");
                            if let Some(bucket) = limiters.get_mut(&client_name) {
                                bucket.try_consume(1.0)
                            } else {
                                Ok(()) // shouldn't happen — inserted at connect
                            }
                        };
                        if let Err(retry_after) = rate_result {
                            let ms = retry_after.as_millis() as u64;
                            let _ = send_envelope(&mut ws_sink, &OutboundEnvelope::Error {
                                request_id: None,
                                code: ErrorCode::RateLimited,
                                message: format!("retry after {}ms", ms),
                                retry_after_ms: Some(ms),
                            }).await;
                            continue;
                        }

                        // Parse envelope (fix #12: generic error, no serde internals)
                        let envelope: InboundEnvelope = match serde_json::from_str(&text) {
                            Ok(env) => env,
                            Err(e) => {
                                tracing::debug!(client = %client_name, error = %e, "envelope parse error");
                                let _ = send_envelope(&mut ws_sink, &OutboundEnvelope::Error {
                                    request_id: None,
                                    code: ErrorCode::ParseError,
                                    message: "Invalid envelope format".to_string(),
                                    retry_after_ms: None,
                                }).await;
                                continue;
                            }
                        };

                        match envelope {
                            InboundEnvelope::Hello { protocol, restore_subscriptions } => {
                                // Guard: only process hello once per connection.
                                // A second hello could re-import stale subscriptions
                                // that the client explicitly unsubscribed from.
                                if hello_received {
                                    // Silently ignore duplicate hello (backward compat)
                                    continue;
                                }
                                hello_received = true;

                                // Validate protocol version; hello_ack already sent
                                if let Some(ref p) = protocol {
                                    if p != "terminus/v1" {
                                        let _ = send_envelope(&mut ws_sink, &OutboundEnvelope::Error {
                                            request_id: None,
                                            code: ErrorCode::UnsupportedProtocol,
                                            message: format!("Unsupported protocol: {}", p),
                                            retry_after_ms: None,
                                        }).await;
                                        let _ = ws_sink.close().await;
                                        break;
                                    }
                                }
                                // Restore subscriptions from previous connection if requested
                                if restore_subscriptions {
                                    let stored = {
                                        let store = shared_subs
                                            .lock()
                                            .expect("subscription store mutex poisoned");
                                        store.get(&client_name).cloned().unwrap_or_default()
                                    };
                                    if !stored.is_empty() {
                                        let count = subs.import(stored);
                                        tracing::info!(
                                            client = %client_name,
                                            restored = count,
                                            "Restored subscriptions"
                                        );
                                        // Persist the restored set and notify client
                                        let restored_subs = subs.export();
                                        {
                                            let mut store = shared_subs
                                                .lock()
                                                .expect("subscription store mutex poisoned");
                                            store.insert(client_name.clone(), restored_subs.clone());
                                        }
                                        // Notify client of each restored subscription
                                        for (sub_id, _) in restored_subs {
                                            let _ = send_envelope(
                                                &mut ws_sink,
                                                &OutboundEnvelope::Subscribed {
                                                    subscription_id: sub_id,
                                                },
                                            )
                                            .await;
                                        }
                                    }
                                }
                            }

                            InboundEnvelope::Request { request_id, command, options: _ } => {
                                // Reject duplicate request_id while already in-flight
                                if pending.contains_key(&request_id) {
                                    let _ = send_envelope(
                                        &mut ws_sink,
                                        &OutboundEnvelope::Error {
                                            request_id: Some(request_id),
                                            code: ErrorCode::QueueFull,
                                            message: "duplicate request_id: already in-flight"
                                                .to_string(),
                                            retry_after_ms: None,
                                        },
                                    )
                                    .await;
                                    continue;
                                }

                                // Check pending queue cap
                                if pending.len() >= config.max_pending_requests {
                                    let _ = send_envelope(&mut ws_sink, &OutboundEnvelope::Error {
                                        request_id: Some(request_id),
                                        code: ErrorCode::QueueFull,
                                        message: "Pending request queue full".to_string(),
                                        retry_after_ms: None,
                                    }).await;
                                    continue;
                                }

                                // Create per-request response channel
                                let (reply_tx, reply_rx) = mpsc::unbounded_channel::<String>();

                                // Build IncomingMessage for the main loop
                                // (fix #4: use PlatformType::Telegram as wire-compat placeholder,
                                // but socket_reply_tx intercepts all replies before platform routing)
                                let incoming = IncomingMessage {
                                    user_id: client_name.clone(),
                                    text: command,
                                    platform: PlatformType::Telegram,
                                    reply_context: ReplyContext {
                                        platform: PlatformType::Telegram,
                                        chat_id: format!("socket:{}", client_name),
                                        thread_ts: None,
                                        socket_reply_tx: Some(reply_tx),
                                    },
                                    attachments: Vec::new(),
                                    socket_request_id: Some(request_id.clone()),
                                    socket_client_name: Some(client_name.clone()),
                                };

                                // Forward to main loop
                                if cmd_tx.send(incoming).await.is_err() {
                                    tracing::error!(client = %client_name, "cmd_tx closed");
                                    break;
                                }

                                // Store pending request
                                pending.insert(request_id.clone(), PendingRequest {
                                    chunks: Vec::new(),
                                    reply_rx,
                                    cancelled: false,
                                });

                                // Send ack
                                let _ = send_envelope(&mut ws_sink, &OutboundEnvelope::Ack {
                                    request_id,
                                    accepted_at: chrono::Utc::now().to_rfc3339(),
                                }).await;
                            }

                            InboundEnvelope::Cancel { request_id } => {
                                if let Some(req) = pending.get_mut(&request_id) {
                                    // Dedup: only send one interrupt per request.
                                    // Multiple cancel envelopes for the same request_id
                                    // should not produce multiple C-c signals.
                                    if !req.cancelled {
                                        req.cancelled = true;
                                        if let Err(e) = cancel_tx.try_send(request_id) {
                                            tracing::warn!(
                                                client = %client_name,
                                                error = %e,
                                                "cancel_tx full or closed — interrupt not sent"
                                            );
                                        }
                                    }
                                }
                                // If request_id not found in pending, it already completed
                                // — no interrupt needed.
                            }

                            InboundEnvelope::Subscribe { subscription_id, filter } => {
                                match subs.add(subscription_id.clone(), filter) {
                                    Ok(()) => {
                                        // Persist updated subscription set
                                        {
                                            let mut store = shared_subs
                                                .lock()
                                                .expect("subscription store mutex poisoned");
                                            store.insert(client_name.clone(), subs.export());
                                        }
                                        let _ = send_envelope(&mut ws_sink, &OutboundEnvelope::Subscribed {
                                            subscription_id,
                                        }).await;
                                    }
                                    Err(()) => {
                                        let _ = send_envelope(&mut ws_sink, &OutboundEnvelope::Error {
                                            request_id: None,
                                            code: ErrorCode::SubscriptionLimit,
                                            message: format!("Max {} subscriptions per connection",
                                                config.max_subscriptions_per_connection),
                                            retry_after_ms: None,
                                        }).await;
                                    }
                                }
                            }

                            InboundEnvelope::Unsubscribe { subscription_id } => {
                                match subs.remove(&subscription_id) {
                                    Ok(()) => {
                                        // Persist updated subscription set
                                        {
                                            let mut store = shared_subs
                                                .lock()
                                                .expect("subscription store mutex poisoned");
                                            let exported = subs.export();
                                            if exported.is_empty() {
                                                store.remove(&client_name);
                                            } else {
                                                store.insert(client_name.clone(), exported);
                                            }
                                        }
                                        let _ = send_envelope(&mut ws_sink, &OutboundEnvelope::Unsubscribed {
                                            subscription_id,
                                        }).await;
                                    }
                                    Err(()) => {
                                        let _ = send_envelope(&mut ws_sink, &OutboundEnvelope::Error {
                                            request_id: None,
                                            code: ErrorCode::UnknownSubscription,
                                            message: "Unknown subscription".to_string(),
                                            retry_after_ms: None,
                                        }).await;
                                    }
                                }
                            }

                            InboundEnvelope::Ping => {
                                let _ = send_envelope(&mut ws_sink, &OutboundEnvelope::Pong).await;
                            }

                            InboundEnvelope::AttachmentMeta {
                                request_id,
                                filename,
                                content_type,
                                size_bytes,
                            } => {
                                // Reject if another binary frame is already pending
                                if pending_binary.is_some() {
                                    let _ = send_envelope(&mut ws_sink, &OutboundEnvelope::Error {
                                        request_id: Some(request_id),
                                        code: ErrorCode::QueueFull,
                                        message: "A binary frame is already pending".to_string(),
                                        retry_after_ms: None,
                                    }).await;
                                    continue;
                                }
                                // Reject zero-size attachments
                                if size_bytes == 0 {
                                    let _ = send_envelope(&mut ws_sink, &OutboundEnvelope::Error {
                                        request_id: Some(request_id),
                                        code: ErrorCode::ParseError,
                                        message: "Attachment size_bytes must be > 0".to_string(),
                                        retry_after_ms: None,
                                    }).await;
                                    continue;
                                }
                                // Enforce size limit
                                if size_bytes > config.max_binary_bytes {
                                    let _ = send_envelope(&mut ws_sink, &OutboundEnvelope::Error {
                                        request_id: Some(request_id),
                                        code: ErrorCode::AttachmentTooLarge,
                                        message: format!(
                                            "Attachment size {} exceeds limit of {} bytes",
                                            size_bytes, config.max_binary_bytes
                                        ),
                                        retry_after_ms: None,
                                    }).await;
                                    continue;
                                }
                                pending_binary = Some(PendingBinary {
                                    request_id: request_id.clone(),
                                    filename,
                                    content_type,
                                    received_at: Instant::now(),
                                });
                                // Ack — client should send binary frame next
                                let _ = send_envelope(&mut ws_sink, &OutboundEnvelope::Ack {
                                    request_id,
                                    accepted_at: chrono::Utc::now().to_rfc3339(),
                                }).await;
                            }
                        }
                    }

                    Some(Ok(Message::Ping(data))) => {
                        last_activity = Instant::now();
                        let _ = ws_sink.send(Message::Pong(data)).await;
                    }

                    Some(Ok(Message::Pong(_))) => {
                        last_activity = Instant::now();
                        last_pong = Instant::now();
                        ping_outstanding = false;
                    }

                    Some(Ok(Message::Close(_))) | None => {
                        tracing::info!(client = %client_name, "socket connection closed by client");
                        break;
                    }

                    Some(Err(e)) => {
                        tracing::warn!(client = %client_name, error = %e, "WebSocket error");
                        break;
                    }

                    Some(Ok(Message::Binary(data))) => {
                        last_activity = Instant::now();

                        // Rate limit check (shared per-client bucket)
                        let rate_result = {
                            let mut limiters = shared_rate_limiters
                                .lock()
                                .expect("rate limiter mutex poisoned");
                            if let Some(bucket) = limiters.get_mut(&client_name) {
                                bucket.try_consume(1.0)
                            } else {
                                Ok(())
                            }
                        };
                        if let Err(retry_after) = rate_result {
                            let ms = retry_after.as_millis() as u64;
                            let _ = send_envelope(&mut ws_sink, &OutboundEnvelope::Error {
                                request_id: None,
                                code: ErrorCode::RateLimited,
                                message: format!("retry after {}ms", ms),
                                retry_after_ms: Some(ms),
                            }).await;
                            continue;
                        }

                        let Some(meta) = pending_binary.take() else {
                            // No pending attachment_meta — ignore binary frame
                            tracing::debug!(
                                client = %client_name,
                                bytes = data.len(),
                                "ignoring unexpected binary frame (no pending attachment_meta)"
                            );
                            continue;
                        };

                        // Validate actual binary frame size against config limit.
                        // The attachment_meta declared size was pre-checked, but a
                        // malicious client can lie — this check is authoritative.
                        if data.len() > config.max_binary_bytes {
                            let _ = send_envelope(&mut ws_sink, &OutboundEnvelope::Error {
                                request_id: Some(meta.request_id),
                                code: ErrorCode::AttachmentTooLarge,
                                message: format!(
                                    "Binary frame {} bytes exceeds limit of {} bytes",
                                    data.len(), config.max_binary_bytes
                                ),
                                retry_after_ms: None,
                            }).await;
                            continue;
                        }

                        // Write binary data to a temp file.
                        // Sanitize extension: only keep alphanumeric chars to
                        // prevent path traversal via crafted filenames.
                        let raw_ext = std::path::Path::new(&meta.filename)
                            .extension()
                            .and_then(|e| e.to_str())
                            .unwrap_or("bin");
                        let ext: String = raw_ext
                            .chars()
                            .take(10)
                            .filter(|c| c.is_ascii_alphanumeric())
                            .collect();
                        let ext = if ext.is_empty() { "bin".to_string() } else { ext };
                        let ulid_str = ulid::Ulid::new().to_string();
                        let tmp_path = std::path::PathBuf::from(format!(
                            "/tmp/terminus-attachment-{}.{}",
                            ulid_str, ext
                        ));
                        let tmp_path_for_cleanup = tmp_path.clone();

                        if let Err(e) = tokio::fs::write(&tmp_path, &data).await {
                            tracing::warn!(
                                client = %client_name,
                                error = %e,
                                "failed to write attachment to temp file"
                            );
                            let _ = send_envelope(&mut ws_sink, &OutboundEnvelope::Error {
                                request_id: Some(meta.request_id),
                                code: ErrorCode::InternalError,
                                message: "Failed to write attachment".to_string(),
                                retry_after_ms: None,
                            }).await;
                            continue;
                        }

                        #[cfg(unix)]
                        {
                            use std::os::unix::fs::PermissionsExt;
                            let perms = std::fs::Permissions::from_mode(0o600);
                            let _ = tokio::fs::set_permissions(&tmp_path, perms).await;
                        }

                        // Create per-request response channel
                        let (reply_tx, reply_rx) = mpsc::unbounded_channel::<String>();
                        let request_id = meta.request_id.clone();

                        let incoming = IncomingMessage {
                            user_id: client_name.clone(),
                            text: String::new(),
                            platform: PlatformType::Telegram,
                            reply_context: ReplyContext {
                                platform: PlatformType::Telegram,
                                chat_id: format!("socket:{}", client_name),
                                thread_ts: None,
                                socket_reply_tx: Some(reply_tx),
                            },
                            attachments: vec![Attachment {
                                path: tmp_path,
                                filename: meta.filename,
                                media_type: meta.content_type,
                            }],
                            socket_request_id: Some(request_id.clone()),
                            socket_client_name: Some(client_name.clone()),
                        };

                        if cmd_tx.send(incoming).await.is_err() {
                            tracing::error!(client = %client_name, "cmd_tx closed");
                            let _ = tokio::fs::remove_file(&tmp_path_for_cleanup).await;
                            break;
                        }

                        pending.insert(request_id, PendingRequest {
                            chunks: Vec::new(),
                            reply_rx,
                            cancelled: false,
                        });
                    }

                    _ => {} // Other frame types ignored
                }
            }

            // 3. StreamEvent from broadcast (for subscription matching)
            event = stream_rx.recv() => {
                match event {
                    Ok(ref ev) => {
                        let matching_subs = subs.matches_stream(ev);
                        for sub_id in matching_subs {
                            let event_json = stream_event_to_json(ev);
                            let _ = send_envelope(&mut ws_sink, &OutboundEnvelope::Event {
                                subscription_id: sub_id,
                                event: event_json,
                            }).await;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        let _ = send_envelope(&mut ws_sink, &OutboundEnvelope::Warning {
                            subscription_id: None,
                            code: "lagged".to_string(),
                            missed_count: Some(n),
                        }).await;
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }

            // 4. AmbientEvent from broadcast
            event = ambient_rx.recv() => {
                match event {
                    Ok(ref ev) => {
                        let matching_subs = subs.matches_ambient(ev);
                        for sub_id in matching_subs {
                            // Fix #13: log warning on serialization failure instead of silent null
                            match serde_json::to_value(ev) {
                                Ok(event_json) => {
                                    let _ = send_envelope(&mut ws_sink, &OutboundEnvelope::Event {
                                        subscription_id: sub_id,
                                        event: event_json,
                                    }).await;
                                }
                                Err(e) => {
                                    tracing::warn!(error = %e, "failed to serialize AmbientEvent");
                                }
                            }
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        let _ = send_envelope(&mut ws_sink, &OutboundEnvelope::Warning {
                            subscription_id: None,
                            code: "lagged".to_string(),
                            missed_count: Some(n),
                        }).await;
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }

            // 5. Ping ticker
            _ = ping_ticker.tick() => {
                // Check pong timeout before sending the next ping
                if ping_outstanding && last_pong.elapsed() > pong_timeout {
                    tracing::info!(client = %client_name, "pong timeout — closing connection");
                    let _ = ws_sink.close().await;
                    break;
                }
                // Periodic client re-validation: check that this client name still
                // exists in the live client list (handles token revocation via hot-reload).
                {
                    let current_clients = shared_clients.load();
                    if !current_clients.iter().any(|c| c.name == client_name) {
                        tracing::warn!(
                            client = %client_name,
                            "client no longer in config — closing connection (token revoked)"
                        );
                        let env = OutboundEnvelope::ShuttingDown {
                            drain_deadline_ms: 0,
                        };
                        let _ = send_envelope(&mut ws_sink, &env).await;
                        let _ = ws_sink.close().await;
                        break;
                    }
                }
                if let Err(e) = ws_sink.send(Message::Ping(vec![])).await {
                    tracing::debug!(client = %client_name, error = %e, "ping send failed");
                    break;
                }
                ping_outstanding = true;
            }

            // 6. Reply poll (fix #5: ensures replies drained within 50ms)
            _ = reply_poll.tick() => {
                // Drain happens at top of loop — this tick just wakes the select
            }
        }

        // Idle timeout check
        if last_activity.elapsed() > idle_timeout {
            tracing::info!(client = %client_name, "idle timeout — closing connection");
            let _ = ws_sink.close().await;
            break;
        }

        // Attachment timeout check: if a binary frame was expected but not received
        // within 30 seconds, send an error and clear the pending state.
        if let Some(ref meta) = pending_binary {
            if meta.received_at.elapsed() > Duration::from_secs(30) {
                let request_id = meta.request_id.clone();
                pending_binary = None;
                let _ = send_envelope(
                    &mut ws_sink,
                    &OutboundEnvelope::Error {
                        request_id: Some(request_id),
                        code: ErrorCode::AttachmentTimeout,
                        message: "Binary frame not received within 30 seconds".to_string(),
                        retry_after_ms: None,
                    },
                )
                .await;
            }
        }
    }

    // Final drain: emit result+end for any completed-but-not-yet-drained requests.
    // Errors ignored — the WebSocket may already be dead.
    let _ = drain_pending(&mut pending, &mut ws_sink).await;

    // Save subscriptions on connection close (regardless of close reason).
    save_subscriptions(&subs, &client_name, &shared_subs);
}

/// Save current subscriptions to the shared store for potential reconnect restore.
fn save_subscriptions(
    subs: &SubscriptionRegistry,
    client_name: &str,
    shared_subs: &super::SharedSubscriptionStore,
) {
    let exported = subs.export();
    let mut store = shared_subs
        .lock()
        .expect("subscription store mutex poisoned");
    if exported.is_empty() {
        store.remove(client_name);
    } else {
        store.insert(client_name.to_string(), exported);
    }
}

/// Drain pending reply channels using channel-close-as-end semantics.
///
/// For each pending request:
/// - `try_recv` Ok → accumulate the text chunk
/// - `try_recv` Empty → channel still open, command still running, skip
/// - `try_recv` Disconnected → sender dropped (command finished), emit result+end
///
/// This fixes the multi-result+end bug: multi-chunk commands (e.g. `: screen`
/// with split_message) produce multiple sends to the channel, but we accumulate
/// them all and emit exactly ONE `result` + ONE `end`.
async fn drain_pending<S>(
    pending: &mut HashMap<String, PendingRequest>,
    ws_sink: &mut S,
) -> Result<()>
where
    S: SinkExt<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin,
{
    let mut completed_ids: Vec<String> = Vec::new();

    for (id, req) in pending.iter_mut() {
        loop {
            match req.reply_rx.try_recv() {
                Ok(text) => {
                    req.chunks.push(text);
                }
                Err(mpsc::error::TryRecvError::Empty) => {
                    // Channel still open — command still running
                    break;
                }
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    // Sender dropped — command handler finished.
                    // Emit accumulated result + end.
                    if !req.chunks.is_empty() {
                        let joined = req.chunks.join("\n");
                        let env = OutboundEnvelope::Result {
                            request_id: id.clone(),
                            schema: None,
                            value: serde_json::json!({ "text": joined }),
                            run_id: None,
                            cancelled: req.cancelled,
                        };
                        send_envelope(ws_sink, &env).await?;
                    }
                    let end = OutboundEnvelope::End {
                        request_id: id.clone(),
                    };
                    send_envelope(ws_sink, &end).await?;
                    completed_ids.push(id.clone());
                    break;
                }
            }
        }
    }

    for id in &completed_ids {
        pending.remove(id);
    }

    Ok(())
}

/// Serialize and send an outbound envelope as a WebSocket text message.
async fn send_envelope<S>(sink: &mut S, env: &OutboundEnvelope) -> Result<()>
where
    S: SinkExt<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin,
{
    let json = serde_json::to_string(env)?;
    sink.send(Message::Text(json)).await?;
    Ok(())
}

/// Convert a StreamEvent to a JSON value for subscription delivery.
fn stream_event_to_json(event: &StreamEvent) -> serde_json::Value {
    match event {
        StreamEvent::StructuredOutputRendered { payload, chat } => {
            serde_json::json!({
                "type": "structured_output",
                "schema": payload.schema,
                "value": payload.value,
                "run_id": payload.run_id,
                "chat": format!("{:?}:{}", chat.platform, chat.chat_id),
            })
        }
        StreamEvent::WebhookStatus {
            schema,
            run_id,
            status,
            chat,
        } => {
            serde_json::json!({
                "type": "webhook_status",
                "schema": schema,
                "run_id": run_id,
                "status": format!("{:?}", status),
                "chat": format!("{:?}:{}", chat.platform, chat.chat_id),
            })
        }
        StreamEvent::QueueDrained {
            delivered_count,
            chat,
        } => {
            serde_json::json!({
                "type": "queue_drained",
                "delivered_count": delivered_count,
                "chat": format!("{:?}:{}", chat.platform, chat.chat_id),
            })
        }
        StreamEvent::NewMessage { session, content } => {
            serde_json::json!({
                "type": "session_output",
                "session": session,
                "chunk": content,
            })
        }
        StreamEvent::SessionExited { session, code } => {
            serde_json::json!({
                "type": "session_exited",
                "session": session,
                "code": code,
            })
        }
        StreamEvent::SessionStarted {
            session, chat_id, ..
        } => {
            serde_json::json!({
                "type": "session_started",
                "session": session,
                "chat_id": chat_id,
            })
        }
        StreamEvent::GapBanner {
            chat_id,
            gap,
            missed_count,
            ..
        } => {
            serde_json::json!({
                "type": "gap_banner",
                "chat_id": chat_id,
                "gap_secs": gap.as_secs(),
                "missed_count": missed_count,
            })
        }
    }
}
