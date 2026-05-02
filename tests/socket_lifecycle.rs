//! Integration tests for the WebSocket bidirectional API.
//!
//! Spins up a real `SocketServer` on an ephemeral port and connects with a
//! real `tokio_tungstenite` client. Covers: auth gate, ambient subscription
//! delivery, reconnect-restore, rate limiting, idle timeout, and the
//! two-phase attachment_meta + binary frame upload protocol.

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

/// Find a free port by binding 0 and immediately releasing. Standard TOCTOU
/// race exists but is acceptable for serialized local tests.
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
    let (stream_tx, _) = broadcast::channel::<StreamEvent>(64);
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

    // Brief grace for the server to reach the accept loop.
    tokio::time::sleep(Duration::from_millis(50)).await;

    ServerHandle {
        addr,
        cancel,
        ambient_tx,
        cmd_rx: Arc::new(tokio::sync::Mutex::new(cmd_rx)),
        shared_subs,
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

const TEST_TOKEN: &str = "tk_test_valid_token_thirty-two_chars_min_aaa";

#[tokio::test]
async fn connect_with_invalid_token_rejected_at_upgrade() {
    let server = start_server(
        vec![SocketClient {
            name: "valid-client".to_string(),
            token: TEST_TOKEN.to_string(),
        }],
        ServerOpts::default(),
    )
    .await;

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
async fn connect_subscribe_receives_ambient_event() {
    let server = start_server(
        vec![SocketClient {
            name: "test-client".to_string(),
            token: TEST_TOKEN.to_string(),
        }],
        ServerOpts::default(),
    )
    .await;

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

#[tokio::test]
async fn disconnect_then_reconnect_restores_subscriptions() {
    let server = start_server(
        vec![SocketClient {
            name: "stable-client".to_string(),
            token: TEST_TOKEN.to_string(),
        }],
        ServerOpts::default(),
    )
    .await;

    // ── First connection: subscribe, then disconnect ───────────────────────
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
        // Drop drops the stream; server cleans up the connection task.
    }

    // Beat: let the server clean up + persist the subscription.
    tokio::time::sleep(Duration::from_millis(100)).await;

    // The shared subscription store should have an entry for `stable-client`.
    {
        let store = server.shared_subs.lock().unwrap();
        assert!(
            !store.is_empty(),
            "subscription store should retain entries after disconnect"
        );
    }

    // ── Second connection: hello with restore_subscriptions: true ──────────
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

    // Drain frames until we see the restored `subscribed` confirmation OR
    // the hello_ack, whichever comes first. The protocol re-emits a
    // `subscribed` for each restored subscription_id.
    let restored = timeout(Duration::from_secs(3), async {
        loop {
            let msg = stream.next().await.unwrap().unwrap();
            if let Message::Text(txt) = msg {
                let v: serde_json::Value = serde_json::from_str(&txt).unwrap();
                if v["type"].as_str() == Some("subscribed")
                    && v["subscription_id"].as_str() == Some("persistent-sub")
                {
                    return true;
                }
            }
        }
    })
    .await
    .expect("restored subscription should re-emit `subscribed`");
    assert!(restored);

    let _ = sink.send(Message::Close(None)).await;
    server.shutdown().await;
}

#[tokio::test]
async fn rate_limit_returns_error_envelope() {
    // Tight rate limit: 1 req/s sustained, 2 burst. Send 30 requests as
    // fast as possible — the server must respond with at least one
    // `error` envelope carrying `code: "rate_limited"`.
    let server = start_server(
        vec![SocketClient {
            name: "rl-client".to_string(),
            token: TEST_TOKEN.to_string(),
        }],
        ServerOpts {
            rate_limit_per_second: Some(1.0),
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

    let saw_rate_limited = timeout(Duration::from_secs(3), async {
        loop {
            let msg = stream.next().await.unwrap().unwrap();
            if let Message::Text(txt) = msg {
                let v: serde_json::Value = serde_json::from_str(&txt).unwrap();
                if v["type"].as_str() == Some("error") && v["code"].as_str() == Some("rate_limited")
                {
                    return true;
                }
            }
        }
    })
    .await
    .expect("at least one rate_limited error should arrive");
    assert!(saw_rate_limited);

    let _ = sink.send(Message::Close(None)).await;
    server.shutdown().await;
}

#[tokio::test]
async fn idle_timeout_closes_connection() {
    // 1s idle timeout. A connected client that sends nothing should have
    // its connection torn down by the server within ~1.5s.
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

    // Wait for the stream to terminate (Close frame or connection drop) due
    // to inactivity. We assert the stream returns None within 5s.
    let closed = timeout(Duration::from_secs(5), async {
        while let Some(msg) = stream.next().await {
            if let Ok(Message::Close(_)) = msg {
                return true;
            }
            if msg.is_err() {
                // Connection dropped by server after timeout.
                return true;
            }
        }
        true // stream ended (None) — connection closed
    })
    .await
    .expect("server should close the idle connection within the timeout window");
    assert!(closed);

    server.shutdown().await;
}

#[tokio::test]
async fn attachment_meta_then_binary_frame_routes_to_harness() {
    // Two-phase upload: send AttachmentMeta JSON envelope, then a binary
    // frame with the payload. The server writes the bytes to a temp file
    // and forwards the IncomingMessage with the attachment to cmd_tx.
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

    // Protocol: AttachmentMeta JSON envelope first, then a binary frame
    // with the payload. The server writes the bytes to a temp file and
    // forwards an IncomingMessage with the attachment to cmd_tx. We do
    // NOT send a `request` envelope — that path produces a separate
    // attachment-less IncomingMessage we'd have to skip past.
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

    // The server should route an IncomingMessage with the attachment to
    // cmd_rx. We don't assert the file path (server-controlled tempfile
    // location) — only that the routed message carries the attachment.
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
