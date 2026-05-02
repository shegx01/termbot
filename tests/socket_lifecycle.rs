//! Integration tests for the WebSocket bidirectional API.
//!
//! Spins up a real `SocketServer` on an ephemeral port and connects with
//! a real `tokio_tungstenite` client. Covers: auth gate (positive +
//! negative paths), ambient subscription delivery, reconnect-restore,
//! rate limiting, idle timeout, and the two-phase attachment_meta +
//! binary frame upload protocol.
//!
//! All tests are `#[serial_test::serial]` because the ephemeral-port
//! pattern (`bind 0` then release) has a small TOCTOU race that can
//! collide with parallel test binaries.

#![cfg(feature = "socket")]

use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpListener;
use tokio::sync::{broadcast, mpsc};
use tokio::time::timeout;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::StatusCode;
use tokio_tungstenite::tungstenite::Message;
use tokio_util::sync::CancellationToken;

use terminus::buffer::StreamEvent;
use terminus::chat_adapters::IncomingMessage;
use terminus::config::{SocketClient, SocketConfig};
use terminus::socket::events::AmbientEvent;
use terminus::socket::{SharedSubscriptionStore, SocketServer, SubscriptionStoreInner};

/// Find a free port by binding 0 and immediately releasing. The
/// `#[serial_test::serial]` attribute on each test serializes execution
/// within this binary; `pick_free_port` collisions with other test
/// binaries running in parallel are extremely unlikely (the OS assigns
/// fresh ephemeral ports), but the serialization keeps the failure mode
/// bounded.
async fn pick_free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    listener.local_addr().expect("local_addr").port()
}

struct ServerHandle {
    addr: String,
    cancel: CancellationToken,
    ambient_tx: broadcast::Sender<AmbientEvent>,
    cmd_rx: Arc<tokio::sync::Mutex<mpsc::Receiver<IncomingMessage>>>,
    shared_subs: SharedSubscriptionStore,
    /// Retained so the broadcast channel always has at least one
    /// receiver (otherwise sends would race with subscriber creation
    /// and a future test asserting on stream events could miss them).
    #[allow(dead_code)]
    stream_rx: tokio::sync::broadcast::Receiver<StreamEvent>,
    join: tokio::task::JoinHandle<()>,
}

impl ServerHandle {
    async fn shutdown(self) {
        self.cancel.cancel();
        let _ = timeout(Duration::from_secs(2), self.join).await;
    }
}

#[derive(Default)]
struct ServerOpts {
    idle_timeout_secs: Option<u64>,
    rate_limit_per_second: Option<f64>,
    rate_limit_burst: Option<f64>,
}

async fn start_server(clients: Vec<SocketClient>, opts: ServerOpts) -> ServerHandle {
    let port = pick_free_port().await;
    let bind = "127.0.0.1".to_string();
    let addr = format!("{bind}:{port}");

    let mut config = toml::from_str::<SocketConfig>("enabled = true").expect("default socket cfg");
    config.bind = bind.clone();
    config.port = port;
    if let Some(secs) = opts.idle_timeout_secs {
        config.idle_timeout_secs = secs;
    }
    if let Some(rate) = opts.rate_limit_per_second {
        config.rate_limit_per_second = rate;
    }
    if let Some(burst) = opts.rate_limit_burst {
        config.rate_limit_burst = burst;
    }

    let (cmd_tx, cmd_rx) = mpsc::channel::<IncomingMessage>(64);
    let (stream_tx, stream_rx) = broadcast::channel::<StreamEvent>(64);
    let (ambient_tx, _) = broadcast::channel::<AmbientEvent>(64);
    let (cancel_tx, _cancel_rx) = mpsc::channel::<String>(16);
    let cancel = CancellationToken::new();
    let shared_clients = Arc::new(ArcSwap::from_pointee(clients));
    let shared_subs: SharedSubscriptionStore =
        Arc::new(std::sync::Mutex::new(SubscriptionStoreInner::new(8)));

    let server = SocketServer::new(
        config,
        cmd_tx,
        stream_tx,
        ambient_tx.clone(),
        cancel.clone(),
        cancel_tx,
        shared_clients,
        Arc::clone(&shared_subs),
    );
    let join = tokio::spawn(async move {
        let _ = server.run().await;
    });

    // Replace the previous fixed `tokio::time::sleep(50ms)` with a
    // bounded retry loop that polls the listener until it accepts. This
    // is deterministic on slow CI runners — no fixed wait that can be
    // exceeded by scheduler stalls.
    let server_addr = addr.clone();
    timeout(Duration::from_secs(3), async move {
        loop {
            match tokio::net::TcpStream::connect(&server_addr).await {
                Ok(_) => break,
                Err(_) => tokio::time::sleep(Duration::from_millis(10)).await,
            }
        }
    })
    .await
    .expect("server should accept connections within 3s of spawning");

    ServerHandle {
        addr,
        cancel,
        ambient_tx,
        cmd_rx: Arc::new(tokio::sync::Mutex::new(cmd_rx)),
        shared_subs,
        stream_rx,
        join,
    }
}

fn auth_request(
    addr: &str,
    token: &str,
) -> tokio_tungstenite::tungstenite::handshake::client::Request {
    let mut request = format!("ws://{addr}").into_client_request().unwrap();
    request
        .headers_mut()
        .insert("Authorization", format!("Bearer {token}").parse().unwrap());
    request
}

/// Test bearer token. Uses `tk_test_*` prefix to keep it visibly distinct
/// from the production `tk_live_*` pattern. 44 characters — well above
/// the 32-character minimum enforced by `Config::validate`.
const TEST_TOKEN: &str = "tk_test_valid_token_thirty-two_chars_min_aaa";

fn test_client() -> SocketClient {
    SocketClient {
        name: "test-client".to_string(),
        token: TEST_TOKEN.to_string(),
    }
}

// ─── Auth gate ──────────────────────────────────────────────────────────────

#[tokio::test]
#[serial_test::serial]
async fn auth_invalid_token_rejected_at_upgrade() {
    let server = start_server(vec![test_client()], ServerOpts::default()).await;
    let request = auth_request(&server.addr, "tk_test_wrong_token_X_aaaaaaaaaaaa");
    match connect_async(request).await {
        Err(tokio_tungstenite::tungstenite::Error::Http(resp)) => {
            assert_eq!(
                resp.status(),
                StatusCode::UNAUTHORIZED,
                "invalid Bearer token must be rejected at the HTTP upgrade"
            );
        }
        other => panic!("expected HTTP 401 error, got: {other:?}"),
    }
    server.shutdown().await;
}

#[tokio::test]
#[serial_test::serial]
async fn auth_missing_authorization_header_rejected() {
    // Negative path: no `Authorization` header at all should produce a
    // 401 — the auth gate's `None` branch.
    let server = start_server(vec![test_client()], ServerOpts::default()).await;
    let request = format!("ws://{}", server.addr)
        .into_client_request()
        .unwrap();
    // No Authorization header inserted.
    match connect_async(request).await {
        Err(tokio_tungstenite::tungstenite::Error::Http(resp)) => {
            assert_eq!(
                resp.status(),
                StatusCode::UNAUTHORIZED,
                "missing Authorization header must be rejected at the HTTP upgrade"
            );
        }
        other => panic!("expected HTTP 401 error, got: {other:?}"),
    }
    server.shutdown().await;
}

#[tokio::test]
#[serial_test::serial]
async fn auth_empty_bearer_rejected() {
    // Negative path: `Authorization: Bearer ` (empty token after the
    // scheme) should be rejected. Production `constant_time_eq(b"", t)`
    // would only match a zero-length client token, which `Config::validate`
    // disallows. This test pins that "empty token reaches the gate but is
    // rejected" property.
    let server = start_server(vec![test_client()], ServerOpts::default()).await;
    let mut request = format!("ws://{}", server.addr)
        .into_client_request()
        .unwrap();
    request
        .headers_mut()
        .insert("Authorization", "Bearer ".parse().unwrap());
    match connect_async(request).await {
        Err(tokio_tungstenite::tungstenite::Error::Http(resp)) => {
            assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        }
        other => panic!("expected HTTP 401 error, got: {other:?}"),
    }
    server.shutdown().await;
}

// ─── Subscription delivery ──────────────────────────────────────────────────

#[tokio::test]
#[serial_test::serial]
async fn subscribe_receives_ambient_event() {
    let server = start_server(vec![test_client()], ServerOpts::default()).await;

    let (ws_stream, _resp) = connect_async(auth_request(&server.addr, TEST_TOKEN))
        .await
        .expect("connect with valid Bearer should succeed");
    let (mut sink, mut stream) = ws_stream.split();

    let subscribe = serde_json::json!({
        "type": "subscribe",
        "subscription_id": "sub-1",
        "filter": { "event_types": ["harness_started"], "schemas": [], "sessions": [] }
    });
    sink.send(Message::Text(subscribe.to_string()))
        .await
        .expect("send subscribe");

    timeout(Duration::from_secs(2), async {
        loop {
            let msg = stream.next().await.expect("stream open").expect("frame");
            if let Message::Text(txt) = msg {
                let v: serde_json::Value = serde_json::from_str(&txt).unwrap();
                if v["type"].as_str() == Some("subscribed")
                    && v["subscription_id"].as_str() == Some("sub-1")
                {
                    return;
                }
            }
        }
    })
    .await
    .expect("subscription confirmation should arrive");

    let event = AmbientEvent::HarnessStarted {
        harness: "claude".to_string(),
        run_id: "01HXTEST".to_string(),
        prompt_hash: None,
    };
    server.ambient_tx.send(event).expect("ambient send");

    let event_seen = timeout(Duration::from_secs(2), async {
        loop {
            let msg = stream.next().await.expect("stream open").expect("frame");
            if let Message::Text(txt) = msg {
                let v: serde_json::Value = serde_json::from_str(&txt).unwrap();
                if v["type"].as_str() == Some("event")
                    && v["subscription_id"].as_str() == Some("sub-1")
                {
                    return v;
                }
            }
        }
    })
    .await
    .expect("event envelope should be delivered");

    assert_eq!(event_seen["event"]["type"], "harness_started");
    assert_eq!(event_seen["event"]["harness"], "claude");
    assert_eq!(event_seen["event"]["run_id"], "01HXTEST");

    let _ = sink.send(Message::Close(None)).await;
    server.shutdown().await;
}

// ─── Reconnect-restore ──────────────────────────────────────────────────────

#[tokio::test]
#[serial_test::serial]
async fn reconnect_restores_subscriptions() {
    let server = start_server(
        vec![SocketClient {
            name: "stable-client".to_string(),
            token: TEST_TOKEN.to_string(),
        }],
        ServerOpts::default(),
    )
    .await;

    {
        let (ws, _) = connect_async(auth_request(&server.addr, TEST_TOKEN))
            .await
            .expect("first connect");
        let (mut sink, mut stream) = ws.split();

        let subscribe = serde_json::json!({
            "type": "subscribe",
            "subscription_id": "persistent-sub",
            "filter": { "event_types": ["harness_started"], "schemas": [], "sessions": [] }
        });
        sink.send(Message::Text(subscribe.to_string()))
            .await
            .unwrap();

        timeout(Duration::from_secs(2), async {
            loop {
                let msg = stream.next().await.unwrap().unwrap();
                if let Message::Text(txt) = msg {
                    let v: serde_json::Value = serde_json::from_str(&txt).unwrap();
                    if v["type"].as_str() == Some("subscribed") {
                        return;
                    }
                }
            }
        })
        .await
        .expect("subscribed confirmation");

        let _ = sink.send(Message::Close(None)).await;
    }

    // Wait for the server to persist the subscription before reconnecting.
    // Polling beats a fixed sleep — the cleanup happens "fast" on healthy
    // runners (~ms) and "slow" on contended ones (~hundreds of ms).
    timeout(Duration::from_secs(2), async {
        loop {
            {
                let store = server.shared_subs.lock().unwrap();
                if !store.is_empty() {
                    return;
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("subscription should be persisted within 2s of disconnect");

    let (ws, _) = connect_async(auth_request(&server.addr, TEST_TOKEN))
        .await
        .expect("second connect");
    let (mut sink, mut stream) = ws.split();

    let hello = serde_json::json!({
        "type": "hello",
        "protocol": "terminus/v1",
        "restore_subscriptions": true
    });
    sink.send(Message::Text(hello.to_string())).await.unwrap();

    timeout(Duration::from_secs(3), async {
        loop {
            let msg = stream.next().await.unwrap().unwrap();
            if let Message::Text(txt) = msg {
                let v: serde_json::Value = serde_json::from_str(&txt).unwrap();
                if v["type"].as_str() == Some("subscribed")
                    && v["subscription_id"].as_str() == Some("persistent-sub")
                {
                    return;
                }
            }
        }
    })
    .await
    .expect("restored subscription should re-emit `subscribed`");

    let _ = sink.send(Message::Close(None)).await;
    server.shutdown().await;
}

// ─── Rate limiting ──────────────────────────────────────────────────────────

#[tokio::test]
#[serial_test::serial]
async fn rate_limit_returns_error_envelope() {
    // Burst = 2, refill = 0.001 token/sec. After the first 2 requests
    // consume the burst, the bucket cannot refill within the test's
    // lifetime. This makes the test deterministic regardless of how
    // slowly the runner schedules the send loop. (R3-P0 fix: previously
    // refill = 1/sec was vulnerable to slow-sender flakes.)
    let server = start_server(
        vec![test_client()],
        ServerOpts {
            rate_limit_per_second: Some(0.001),
            rate_limit_burst: Some(2.0),
            ..Default::default()
        },
    )
    .await;

    let (ws, _) = connect_async(auth_request(&server.addr, TEST_TOKEN))
        .await
        .expect("connect");
    let (mut sink, mut stream) = ws.split();

    for i in 0..30 {
        let req = serde_json::json!({
            "type": "request",
            "request_id": format!("rl-{i}"),
            "command": ": list"
        });
        sink.send(Message::Text(req.to_string())).await.unwrap();
    }

    // The .expect() on the timeout is the actual failure site. If no
    // rate_limited error arrives, the timeout fires and the message
    // explains why. No vacuous post-timeout assert needed.
    timeout(Duration::from_secs(3), async {
        loop {
            let msg = stream.next().await.unwrap().unwrap();
            if let Message::Text(txt) = msg {
                let v: serde_json::Value = serde_json::from_str(&txt).unwrap();
                if v["type"].as_str() == Some("error") && v["code"].as_str() == Some("rate_limited")
                {
                    return;
                }
            }
        }
    })
    .await
    .expect("at least one rate_limited error should arrive within 3s");

    let _ = sink.send(Message::Close(None)).await;
    server.shutdown().await;
}

// ─── Idle timeout ───────────────────────────────────────────────────────────

#[tokio::test]
#[serial_test::serial]
async fn idle_timeout_closes_connection() {
    // 1s idle timeout. The connection-task `last_activity` uses
    // `std::time::Instant`, so we cannot use `tokio::time::pause` here —
    // the check reads the OS monotonic clock directly. Real-time wait
    // is acceptable: 1s timeout + 50ms reply_poll cadence → close at
    // ~1050ms; the 5s ceiling provides a generous margin for CI jitter.
    let server = start_server(
        vec![SocketClient {
            name: "idle-client".to_string(),
            token: TEST_TOKEN.to_string(),
        }],
        ServerOpts {
            idle_timeout_secs: Some(1),
            ..Default::default()
        },
    )
    .await;

    let (ws, _) = connect_async(auth_request(&server.addr, TEST_TOKEN))
        .await
        .expect("connect");
    let (mut _sink, mut stream) = ws.split();

    // The .expect() is the assertion: if the server fails to close the
    // idle connection within 5s, the test panics with a clear message.
    timeout(Duration::from_secs(5), async {
        while let Some(msg) = stream.next().await {
            if let Ok(Message::Close(_)) = msg {
                return;
            }
            if msg.is_err() {
                return;
            }
        }
    })
    .await
    .expect("server should close the idle connection within 5s");

    server.shutdown().await;
}

// ─── Attachment upload (two-phase) ──────────────────────────────────────────

#[tokio::test]
#[serial_test::serial]
async fn attachment_meta_then_binary_frame_routes_to_harness() {
    let server = start_server(
        vec![SocketClient {
            name: "upload-client".to_string(),
            token: TEST_TOKEN.to_string(),
        }],
        ServerOpts::default(),
    )
    .await;

    let (ws, _) = connect_async(auth_request(&server.addr, TEST_TOKEN))
        .await
        .expect("connect");
    let (mut sink, mut _stream) = ws.split();

    // Two-phase: AttachmentMeta JSON envelope first, then a binary frame.
    let request_id = "upload-1".to_string();
    let mut payload: Vec<u8> = b"PNG\x89".to_vec();
    payload.extend((0u8..32).collect::<Vec<u8>>());

    let meta = serde_json::json!({
        "type": "attachment_meta",
        "request_id": request_id,
        "filename": "test.png",
        "content_type": "image/png",
        "size_bytes": payload.len()
    });
    sink.send(Message::Text(meta.to_string())).await.unwrap();
    sink.send(Message::Binary(payload.clone())).await.unwrap();

    let mut cmd_rx_guard = server.cmd_rx.lock().await;
    let msg = timeout(Duration::from_secs(3), cmd_rx_guard.recv())
        .await
        .expect("cmd_tx should receive the routed message")
        .expect("channel open");
    assert!(
        !msg.attachments.is_empty(),
        "routed message should carry the attachment, got: {:?}",
        msg.attachments
    );
    assert_eq!(
        msg.attachments[0].filename, "test.png",
        "attachment filename should round-trip"
    );
    drop(cmd_rx_guard);

    let _ = sink.send(Message::Close(None)).await;
    server.shutdown().await;
}
