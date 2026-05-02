//! Integration tests for the sleep/wake gap-banner pipeline.
//!
//! Covers: power_rx -> handle_gap -> banner emission -> per-platform
//! delivery -> ack resolution -> inline-prefix fallback. End-to-end
//! through the real `App` event flow with `MockPlatform`.
//!
//! NOTE: `FakePowerManager` and `gap_detector.rs` are covered by their
//! own unit tests in `src/power/{fake,gap_detector}.rs`; these
//! integration tests drive `App::handle_gap` directly to keep timing
//! deterministic and avoid spinning up a real supervisor loop.

use std::time::Duration;

use chrono::Utc;
use tempfile::tempdir;
use tokio::sync::mpsc;
use tokio::time::timeout;

use terminus::buffer::StreamEvent;
use terminus::chat_adapters::IncomingMessage;
use terminus::power::types::PowerSignal;
use terminus::state_store::StateUpdate;

mod common;

use common::fixtures::{
    subscribe_stream, test_app, wire_telegram_blocking_mock, wire_telegram_mock,
};
use common::mocks::MockEvent;

// Synthetic chat IDs used across tests. Distinct values per test so a
// future cross-test assertion can't accidentally collide.
const CHAT_BASIC: i64 = 7777;
const CHAT_BLOCKING: i64 = 8888;
const CHAT_ORDERING: i64 = 2222;
const CHAT_PAYLOAD: i64 = 3333;
const CHATS_MULTI: [i64; 3] = [1001, 1002, 1003];

#[tokio::test]
async fn gap_banner_emitted_after_wake() {
    let dir = tempdir().unwrap();
    let (mut app, _state_rx) = test_app(dir.path());

    app.apply_state_update(StateUpdate::BindTelegramChat(CHAT_BASIC))
        .await;
    let mock = wire_telegram_mock(&mut app);

    let signal = PowerSignal::GapDetected {
        paused_at: Utc::now() - chrono::Duration::seconds(90),
        resumed_at: Utc::now(),
        gap: Duration::from_secs(90),
    };
    let (cmd_tx, _cmd_rx) = mpsc::channel::<IncomingMessage>(16);
    timeout(Duration::from_secs(2), app.handle_gap(signal, cmd_tx))
        .await
        .expect("handle_gap should complete within 2s");

    // No `tokio::time::sleep` grace period needed: handle_gap awaits the
    // banner-ack oneshot, which the delivery task only resolves AFTER
    // send_message completes and pushes to messages_sent. By the time
    // handle_gap returns, the snapshot is already populated.
    let snap = mock.snapshot().await;
    assert_eq!(
        snap.messages_sent.len(),
        1,
        "expected exactly one banner message, got: {:?}",
        snap.messages_sent
    );
    let (chat_id, text) = &snap.messages_sent[0];
    assert_eq!(
        chat_id,
        &CHAT_BASIC.to_string(),
        "banner routed to wrong chat"
    );
    assert!(
        text.contains("paused at") && text.contains("resumed at"),
        "banner text should mention paused/resumed; got: {text:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn gap_banner_inline_fallback_on_ack_timeout() {
    // The banner-ack oneshot inside `App::handle_gap` has a 5s timeout
    // (see `src/app.rs::handle_gap`). When the platform's `send_message`
    // never returns, the ack never fires, the timeout engages, and a
    // `GapInfo` is stashed in `gap_prefix` for inline-prefix fallback.
    //
    // `start_paused = true` makes the tokio time driver virtual: the 5s
    // wait completes in microseconds via `tokio::time::advance`. Without
    // this, the test would consume real wall-clock time and fragile
    // under loaded CI runners.
    let dir = tempdir().unwrap();
    let (mut app, _state_rx) = test_app(dir.path());

    app.apply_state_update(StateUpdate::BindTelegramChat(CHAT_BLOCKING))
        .await;
    let mock = wire_telegram_blocking_mock(&mut app);

    let signal = PowerSignal::GapDetected {
        paused_at: Utc::now() - chrono::Duration::seconds(120),
        resumed_at: Utc::now(),
        gap: Duration::from_secs(120),
    };
    let (cmd_tx, _cmd_rx) = mpsc::channel::<IncomingMessage>(16);

    // Drive handle_gap into the ack-timeout path on a separate task while
    // we advance the virtual clock past 5s.
    let handle = tokio::spawn(async move {
        app.handle_gap(signal, cmd_tx).await;
        app
    });

    // Yield once so the spawned task gets a chance to install the ack
    // oneshot and enter `tokio::time::timeout(5s, ...)`. Then jump past
    // the timeout.
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_secs(6)).await;

    // The handle_gap future must have returned (timeout fired). 8s is
    // an upper bound on the virtual-time advance; `advance` is
    // synchronous, so this should resolve immediately.
    let app = timeout(Duration::from_secs(8), handle)
        .await
        .expect("handle_gap should complete after virtual time advance")
        .expect("spawned task should not panic");

    // The blocking mock should have NO recorded sends.
    let snap = mock.snapshot().await;
    assert!(
        snap.messages_sent.is_empty(),
        "blocking mock should not have recorded any send"
    );

    // The fallback path should have stashed a GapInfo for the chat.
    let prefix = app.consume_gap_prefix(&CHAT_BLOCKING.to_string()).await;
    assert!(
        prefix.is_some(),
        "consume_gap_prefix should yield a fallback entry after ack timeout"
    );
}

#[tokio::test]
async fn multi_chat_gap_banners_all_delivered() {
    // Three telegram chats. Verifies every active chat receives a banner.
    // Does NOT prove parallel delivery — `handle_gap` emits banners
    // sequentially on a single broadcast channel; the only "parallel"
    // aspect is `join_all` over per-chat ack oneshots. Naming has been
    // adjusted to match what the assertion actually proves.
    let dir = tempdir().unwrap();
    let (mut app, _state_rx) = test_app(dir.path());

    for id in CHATS_MULTI {
        app.apply_state_update(StateUpdate::BindTelegramChat(id))
            .await;
    }
    let mock = wire_telegram_mock(&mut app);

    let signal = PowerSignal::GapDetected {
        paused_at: Utc::now() - chrono::Duration::seconds(60),
        resumed_at: Utc::now(),
        gap: Duration::from_secs(60),
    };
    let (cmd_tx, _cmd_rx) = mpsc::channel::<IncomingMessage>(16);
    timeout(Duration::from_secs(2), app.handle_gap(signal, cmd_tx))
        .await
        .expect("handle_gap should complete within 2s");

    let snap = mock.snapshot().await;
    assert_eq!(
        snap.messages_sent.len(),
        CHATS_MULTI.len(),
        "expected one banner per chat, got: {:?}",
        snap.messages_sent
    );
    let mut chat_ids: Vec<String> = snap
        .messages_sent
        .iter()
        .map(|(id, _)| id.clone())
        .collect();
    chat_ids.sort();
    let mut expected: Vec<String> = CHATS_MULTI.iter().map(|id| id.to_string()).collect();
    expected.sort();
    assert_eq!(
        chat_ids, expected,
        "every active chat should receive its own banner"
    );
}

#[tokio::test]
async fn gap_banner_emits_pause_then_send_then_resume_in_order() {
    // True ordering proof using `MockPlatformState::event_log` — counters
    // alone (`pause=1, send=1, resume=1`) would be satisfied by any
    // permutation. This test asserts the strict pause -> send -> resume
    // sequence the production wake-recovery sequence relies on.
    let dir = tempdir().unwrap();
    let (mut app, _state_rx) = test_app(dir.path());

    app.apply_state_update(StateUpdate::BindTelegramChat(CHAT_ORDERING))
        .await;
    let mock = wire_telegram_mock(&mut app);

    let signal = PowerSignal::GapDetected {
        paused_at: Utc::now() - chrono::Duration::seconds(45),
        resumed_at: Utc::now(),
        gap: Duration::from_secs(45),
    };
    let (cmd_tx, _cmd_rx) = mpsc::channel::<IncomingMessage>(16);
    timeout(Duration::from_secs(2), app.handle_gap(signal, cmd_tx))
        .await
        .expect("handle_gap should complete");

    let snap = mock.snapshot().await;
    let log = &snap.event_log;
    let pause_idx = log
        .iter()
        .position(|e| matches!(e, MockEvent::Paused))
        .expect("MockEvent::Paused must appear in the log");
    let send_idx = log
        .iter()
        .position(|e| matches!(e, MockEvent::Sent { .. }))
        .expect("MockEvent::Sent must appear in the log");
    let resume_idx = log
        .iter()
        .position(|e| matches!(e, MockEvent::Resumed))
        .expect("MockEvent::Resumed must appear in the log");

    assert!(
        pause_idx < send_idx,
        "Paused must come BEFORE Sent in the event log; got {log:?}"
    );
    assert!(
        send_idx < resume_idx,
        "Sent must come BEFORE Resumed in the event log; got {log:?}"
    );
}

#[tokio::test]
async fn gap_banner_stream_event_carries_signal_fields() {
    // Contract test for the wire schema: the `StreamEvent::GapBanner`
    // payload carries paused_at, resumed_at, gap, and chat_id. Socket
    // subscribers and downstream consumers depend on this contract.
    let dir = tempdir().unwrap();
    let (mut app, _state_rx) = test_app(dir.path());

    app.apply_state_update(StateUpdate::BindTelegramChat(CHAT_PAYLOAD))
        .await;

    // Pre-subscribe to the stream BEFORE wiring the mock, so the test
    // sees the GapBanner event before the delivery task consumes it.
    let mut stream_rx = subscribe_stream(&app);
    let _mock = wire_telegram_mock(&mut app);

    let paused_at = Utc::now() - chrono::Duration::seconds(75);
    let resumed_at = Utc::now();
    let gap = Duration::from_secs(75);
    let signal = PowerSignal::GapDetected {
        paused_at,
        resumed_at,
        gap,
    };
    let (cmd_tx, _cmd_rx) = mpsc::channel::<IncomingMessage>(16);

    let handle_task = tokio::spawn(async move {
        let _ = timeout(Duration::from_secs(2), app.handle_gap(signal, cmd_tx)).await;
        app
    });

    // Pull StreamEvents until we see a GapBanner. Lagged broadcast
    // receivers can return Err — those are non-fatal, so we retry rather
    // than panic.
    let banner = timeout(Duration::from_secs(2), async {
        loop {
            match stream_rx.recv().await {
                Ok(StreamEvent::GapBanner {
                    chat_id,
                    paused_at: pa,
                    resumed_at: ra,
                    gap: g,
                    ..
                }) => return (chat_id, pa, ra, g),
                Ok(_) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    panic!("broadcast channel closed before banner")
                }
            }
        }
    })
    .await
    .expect("gap banner should be emitted");

    let (banner_chat_id, banner_paused_at, banner_resumed_at, banner_gap) = banner;
    assert_eq!(banner_chat_id, CHAT_PAYLOAD.to_string());
    assert_eq!(banner_gap, gap);
    assert_eq!(banner_paused_at, paused_at);
    assert_eq!(banner_resumed_at, resumed_at);

    let _ = handle_task.await;
}
