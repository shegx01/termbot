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
use crate::chat_adapters::{IncomingMessage, PlatformType, ReplyContext};
use crate::config::SocketConfig;

use super::envelope::{ErrorCode, InboundEnvelope, OutboundEnvelope};
use super::events::AmbientEvent;
use super::rate_limit::TokenBucket;
use super::subscription::SubscriptionRegistry;

/// Shared per-client rate limiter. Keyed by client name so reconnecting
/// clients share the same token bucket (prevents rate-limit bypass via reconnect).
pub type SharedRateLimiters = Arc<Mutex<HashMap<String, TokenBucket>>>;

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
) {
    let (mut ws_sink, mut ws_stream_read) = ws_stream.split();

    // Per-connection state
    let mut subs = SubscriptionRegistry::new(config.max_subscriptions_per_connection);
    let mut pending: HashMap<String, PendingRequest> = HashMap::new();
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
        return;
    }

    loop {
        // Drain pending reply channels. Uses channel-close-as-end semantics:
        // accumulate all text chunks; when the sender is dropped (command handler
        // returned), emit a single `result` with joined text + a single `end`.
        if drain_pending(&mut pending, &mut ws_sink).await.is_err() {
            return; // WS write error
        }

        tokio::select! {
            biased;

            // 1. Cancellation (shutdown)
            _ = cancel.cancelled() => {
                let env = OutboundEnvelope::ShuttingDown {
                    drain_deadline_ms: config.shutdown_drain_secs * 1000,
                };
                let _ = send_envelope(&mut ws_sink, &env).await;
                let _ = ws_sink.close().await;
                tracing::info!(client = %client_name, "socket connection closed (shutdown)");
                return;
            }

            // 2. Inbound WebSocket frame
            frame = ws_stream_read.next() => {
                match frame {
                    Some(Ok(Message::Text(text))) => {
                        last_activity = Instant::now();

                        // Max message size check
                        if text.len() > config.max_message_bytes {
                            let _ = send_envelope(&mut ws_sink, &OutboundEnvelope::Error {
                                request_id: None,
                                code: ErrorCode::MessageTooLarge,
                                message: format!("Message exceeds {} bytes", config.max_message_bytes),
                                retry_after_ms: None,
                            }).await;
                            let _ = ws_sink.close().await;
                            return;
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
                            InboundEnvelope::Hello { protocol } => {
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
                                        return;
                                    }
                                }
                                // hello_ack already sent on connect; no-op
                            }

                            InboundEnvelope::Request { request_id, command, options: _ } => {
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
                                    return;
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
                                    req.cancelled = true;
                                }
                            }

                            InboundEnvelope::Subscribe { subscription_id, filter } => {
                                match subs.add(subscription_id.clone(), filter) {
                                    Ok(()) => {
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
                        return;
                    }

                    Some(Err(e)) => {
                        tracing::warn!(client = %client_name, error = %e, "WebSocket error");
                        return;
                    }

                    _ => {} // Binary frames ignored
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
                    Err(broadcast::error::RecvError::Closed) => return,
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
                    Err(broadcast::error::RecvError::Closed) => return,
                }
            }

            // 5. Ping ticker
            _ = ping_ticker.tick() => {
                // Check pong timeout before sending the next ping
                if ping_outstanding && last_pong.elapsed() > pong_timeout {
                    tracing::info!(client = %client_name, "pong timeout — closing connection");
                    let _ = ws_sink.close().await;
                    return;
                }
                if let Err(e) = ws_sink.send(Message::Ping(vec![])).await {
                    tracing::debug!(client = %client_name, error = %e, "ping send failed");
                    return;
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
            return;
        }
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
