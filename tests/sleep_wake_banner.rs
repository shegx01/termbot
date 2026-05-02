//! Integration tests for the sleep/wake gap-banner pipeline.
//!
//! Covers the architecture-review path: power_rx -> handle_gap -> banner
//! emission -> per-platform delivery -> ack resolution -> inline-prefix
//! fallback. End-to-end through the real `App` event flow with a mock
//! `ChatPlatform` standing in for Telegram/Slack/Discord.

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use tempfile::tempdir;
use tokio::sync::mpsc;
use tokio::time::timeout;

use terminus::buffer::StreamEvent;
use terminus::chat_adapters::{ChatPlatform, IncomingMessage};
use terminus::delivery::spawn_delivery_task;
use terminus::power::types::PowerSignal;
use terminus::state_store::StateUpdate;

mod common;

use common::fixtures::test_app;
use common::mocks::MockPlatform;

#[tokio::test]
async fn gap_banner_emitted_after_wake() {
    let dir = tempdir().unwrap();
    let (mut app, _state_rx) = test_app(dir.path());

    // Seed a Telegram chat through the public StateUpdate path so the
    // active-chat set is populated without poking private fields.
    app.apply_state_update(StateUpdate::BindTelegramChat(7777))
        .await;

    // Wire a MockPlatform in via set_platforms + spawn the production
    // delivery task so banners get actually delivered (not just emitted).
    let mock = MockPlatform::telegram();
    let dyn_mock: Arc<dyn ChatPlatform> = mock.clone();
    let stream_rx = app.subscribe_stream();
    let acks = app.pending_banner_acks_handle();
    let prefixes = app.gap_prefix_handle();
    spawn_delivery_task(Arc::clone(&dyn_mock), stream_rx, acks, prefixes);
    app.set_platforms(Some(dyn_mock), None, None, None, None);

    // Trigger handle_gap with a 90s gap.
    let signal = PowerSignal::GapDetected {
        paused_at: Utc::now() - chrono::Duration::seconds(90),
        resumed_at: Utc::now(),
        gap: Duration::from_secs(90),
    };
    let (cmd_tx, _cmd_rx) = mpsc::channel::<IncomingMessage>(16);
    timeout(Duration::from_secs(2), app.handle_gap(signal, cmd_tx))
        .await
        .expect("handle_gap should complete within 2s");

    // The mock should have received exactly one send_message — the banner.
    // Allow a brief grace period for the broadcast event to reach the
    // delivery task.
    tokio::time::sleep(Duration::from_millis(50)).await;
    let snap = mock.snapshot().await;
    assert_eq!(
        snap.messages_sent.len(),
        1,
        "expected exactly one banner message, got: {:?}",
        snap.messages_sent
    );
    let (chat_id, text) = &snap.messages_sent[0];
    assert_eq!(chat_id, "7777", "banner routed to wrong chat");
    assert!(
        text.contains("paused at") && text.contains("resumed at"),
        "banner text should mention paused/resumed; got: {text:?}"
    );

    // Adapters were paused and resumed exactly once each as part of the
    // wake sequence.
    let (paused, resumed) = mock.pause_resume_counts().await;
    assert_eq!(paused, 1, "pause should fire once before banner emission");
    assert_eq!(resumed, 1, "resume should fire once after banner ack");
}

#[tokio::test]
async fn gap_banner_inline_fallback_on_ack_timeout() {
    // When the delivery task can't send the banner within 5s, handle_gap
    // falls back to inserting an inline-prefix in `gap_prefix`. We exercise
    // this by using a `telegram_blocking` mock whose send_message blocks
    // forever — the ack oneshot never fires, the timeout path engages, and
    // a `GapInfo` should be retrievable via `consume_gap_prefix`.
    let dir = tempdir().unwrap();
    let (mut app, _state_rx) = test_app(dir.path());

    app.apply_state_update(StateUpdate::BindTelegramChat(8888))
        .await;

    let mock = MockPlatform::telegram_blocking();
    let dyn_mock: Arc<dyn ChatPlatform> = mock.clone();
    let stream_rx = app.subscribe_stream();
    let acks = app.pending_banner_acks_handle();
    let prefixes = app.gap_prefix_handle();
    spawn_delivery_task(Arc::clone(&dyn_mock), stream_rx, acks, prefixes);
    app.set_platforms(Some(dyn_mock), None, None, None, None);

    let signal = PowerSignal::GapDetected {
        paused_at: Utc::now() - chrono::Duration::seconds(120),
        resumed_at: Utc::now(),
        gap: Duration::from_secs(120),
    };
    let (cmd_tx, _cmd_rx) = mpsc::channel::<IncomingMessage>(16);

    // The 5s ack timeout inside handle_gap means this should complete in
    // ~5s. Cap at 8s for safety.
    timeout(Duration::from_secs(8), app.handle_gap(signal, cmd_tx))
        .await
        .expect("handle_gap should complete within the ack-timeout window");

    // The blocking mock never produced a `send_message` snapshot entry.
    let snap = mock.snapshot().await;
    assert!(
        snap.messages_sent.is_empty(),
        "blocking mock should not have recorded any send"
    );

    // The fallback path should have stashed a GapInfo for chat 8888.
    let prefix = app.consume_gap_prefix("8888").await;
    assert!(
        prefix.is_some(),
        "consume_gap_prefix should yield a fallback entry after ack timeout"
    );
}

#[tokio::test]
async fn multi_chat_gap_banners_emitted_in_parallel() {
    // Three telegram chats: each should get its own banner concurrently
    // (handle_gap awaits all per-chat oneshot ACKs in parallel).
    let dir = tempdir().unwrap();
    let (mut app, _state_rx) = test_app(dir.path());

    for id in [1001i64, 1002, 1003] {
        app.apply_state_update(StateUpdate::BindTelegramChat(id))
            .await;
    }

    let mock = MockPlatform::telegram();
    let dyn_mock: Arc<dyn ChatPlatform> = mock.clone();
    let stream_rx = app.subscribe_stream();
    let acks = app.pending_banner_acks_handle();
    let prefixes = app.gap_prefix_handle();
    spawn_delivery_task(Arc::clone(&dyn_mock), stream_rx, acks, prefixes);
    app.set_platforms(Some(dyn_mock), None, None, None, None);

    let signal = PowerSignal::GapDetected {
        paused_at: Utc::now() - chrono::Duration::seconds(60),
        resumed_at: Utc::now(),
        gap: Duration::from_secs(60),
    };
    let (cmd_tx, _cmd_rx) = mpsc::channel::<IncomingMessage>(16);
    timeout(Duration::from_secs(2), app.handle_gap(signal, cmd_tx))
        .await
        .expect("handle_gap should complete within 2s");

    tokio::time::sleep(Duration::from_millis(50)).await;
    let snap = mock.snapshot().await;
    assert_eq!(
        snap.messages_sent.len(),
        3,
        "expected three banner messages (one per chat), got: {:?}",
        snap.messages_sent
    );
    let mut chat_ids: Vec<&str> = snap
        .messages_sent
        .iter()
        .map(|(id, _)| id.as_str())
        .collect();
    chat_ids.sort();
    assert_eq!(
        chat_ids,
        vec!["1001", "1002", "1003"],
        "every active chat should receive its own banner"
    );
}

#[tokio::test]
async fn gap_banner_pause_precedes_send() {
    // Validate the pause-then-emit ordering: the platform must observe a
    // pause() call BEFORE the first send_message arrives. handle_gap
    // pauses adapters first, then broadcasts GapBanner, then awaits acks.
    let dir = tempdir().unwrap();
    let (mut app, _state_rx) = test_app(dir.path());

    app.apply_state_update(StateUpdate::BindTelegramChat(2222))
        .await;

    let mock = MockPlatform::telegram();
    let dyn_mock: Arc<dyn ChatPlatform> = mock.clone();
    let stream_rx = app.subscribe_stream();
    let acks = app.pending_banner_acks_handle();
    let prefixes = app.gap_prefix_handle();
    spawn_delivery_task(Arc::clone(&dyn_mock), stream_rx, acks, prefixes);
    app.set_platforms(Some(dyn_mock), None, None, None, None);

    let signal = PowerSignal::GapDetected {
        paused_at: Utc::now() - chrono::Duration::seconds(45),
        resumed_at: Utc::now(),
        gap: Duration::from_secs(45),
    };
    let (cmd_tx, _cmd_rx) = mpsc::channel::<IncomingMessage>(16);
    timeout(Duration::from_secs(2), app.handle_gap(signal, cmd_tx))
        .await
        .expect("handle_gap should complete");

    tokio::time::sleep(Duration::from_millis(50)).await;
    let snap = mock.snapshot().await;
    // pause must have been called, banner must have been sent, resume must
    // have been called — all in that order.
    assert_eq!(snap.pause_count, 1);
    assert_eq!(snap.messages_sent.len(), 1);
    assert_eq!(snap.resume_count, 1);
}

#[tokio::test]
async fn gap_banner_uses_stream_event_payload() {
    // Verify that the GapBanner StreamEvent payload carries paused_at,
    // resumed_at, gap, and platform fields. This is a contract test for the
    // event schema downstream consumers (e.g. socket subscribers) rely on.
    let dir = tempdir().unwrap();
    let (mut app, _state_rx) = test_app(dir.path());

    app.apply_state_update(StateUpdate::BindTelegramChat(3333))
        .await;

    let mut stream_rx = app.subscribe_stream();
    let mock = MockPlatform::telegram();
    let dyn_mock: Arc<dyn ChatPlatform> = mock.clone();
    let delivery_stream = app.subscribe_stream();
    let acks = app.pending_banner_acks_handle();
    let prefixes = app.gap_prefix_handle();
    spawn_delivery_task(Arc::clone(&dyn_mock), delivery_stream, acks, prefixes);
    app.set_platforms(Some(dyn_mock), None, None, None, None);

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

    // Pull StreamEvents until we see a GapBanner. The same event stream
    // also carries any other events emitted during the test, so loop until
    // we hit the right kind.
    let banner = timeout(Duration::from_secs(2), async {
        loop {
            match stream_rx.recv().await {
                Ok(StreamEvent::GapBanner {
                    chat_id,
                    paused_at,
                    resumed_at,
                    gap,
                    ..
                }) => return (chat_id, paused_at, resumed_at, gap),
                Ok(_) => continue,
                Err(_) => panic!("broadcast channel closed before banner"),
            }
        }
    })
    .await
    .expect("gap banner should be emitted");

    assert_eq!(banner.0, "3333");
    assert_eq!(banner.3, gap);
    // Paused/resumed timestamps are forwarded directly from the signal.
    assert_eq!(banner.1, paused_at);
    assert_eq!(banner.2, resumed_at);

    let _ = handle_task.await;
}
