//! Integration tests for the state-store debounced/force-persist paths.
//!
//! Two concerns from the architecture review's open-questions section:
//!
//! 1. Force-persist watermark variants must not lose updates under burst.
//!    `App::apply_state_update` calls `store.persist()` synchronously for
//!    `SlackWatermark` / `DiscordWatermark` (bypassing the 10-update / 5s
//!    debounce window). A burst of 100 updates must result in all 100
//!    landing on disk.
//!
//! 2. The debounce threshold (>= 10 updates) must trigger a persist on
//!    the boundary update. The previous `crash_mid_batch_recovers_to_last_persist`
//!    test was renamed (R1-P0) — `drop(app)` is a deterministic destructor,
//!    not a real OS-level crash, so the test can only verify "force-persist
//!    completes synchronously before apply returns," not crash-safety.
//!    True crash-safety (atomic rename under SIGKILL) requires filesystem
//!    fault injection and is out of scope.

#![allow(unused_imports)] // mpsc kept for symmetry with other test files

use std::time::Duration;

use tempfile::tempdir;
use tokio::sync::mpsc;
use tokio::time::timeout;

use terminus::state_store::{StateStore, StateUpdate};

mod common;

use common::fixtures::test_app;

#[tokio::test]
async fn force_persist_watermarks_dont_lose_updates_under_burst() {
    let dir = tempdir().unwrap();
    let (mut app, mut state_rx) = test_app(dir.path());

    let state_path = dir.path().join("terminus-state.json");
    const N: usize = 100;
    for i in 0..N {
        app.apply_state_update(StateUpdate::SlackWatermark {
            channel_id: format!("C{i:04}"),
            ts: format!("{}.000000", 1_700_000_000 + i),
        })
        .await;
    }

    // Reload from disk: every channel watermark must be present and equal
    // to the value we wrote (force-persist bypasses debounce).
    let reloaded = StateStore::load(&state_path).expect("reload state");
    let snap = reloaded.snapshot();
    assert_eq!(
        snap.slack_watermarks.len(),
        N,
        "expected {N} slack watermarks persisted, got {}",
        snap.slack_watermarks.len()
    );
    for i in 0..N {
        let key = format!("C{i:04}");
        let expected = format!("{}.000000", 1_700_000_000 + i);
        assert_eq!(
            snap.slack_watermarks.get(&key),
            Some(&expected),
            "watermark for {key} should equal {expected}"
        );
    }

    // Drain any debounced state updates that escaped so the test runtime
    // doesn't have dangling channel work.
    let _ = timeout(Duration::from_millis(50), async {
        while state_rx.try_recv().is_ok() {}
    })
    .await;
}

#[tokio::test]
async fn force_persist_completes_before_apply_returns() {
    // Renamed from `crash_mid_batch_recovers_to_last_persist` (R1-P0).
    // Rust's `drop(app)` is a deterministic synchronous destructor, NOT a
    // process crash — the OS would have to be killed mid-`write`/`rename`
    // to actually exercise atomic-rename crash-safety, which we cannot do
    // from a unit/integration test. What this test PROVES is that the
    // force-persist variants (`SlackWatermark` / `DiscordWatermark`) are
    // durable on disk before `apply_state_update` returns, regardless of
    // what later in-memory state the test mutates.
    let dir = tempdir().unwrap();
    let state_path = dir.path().join("terminus-state.json");

    {
        let (mut app, _state_rx) = test_app(dir.path());
        app.apply_state_update(StateUpdate::SlackWatermark {
            channel_id: "C001".to_string(),
            ts: "1.000000".to_string(),
        })
        .await;
        app.apply_state_update(StateUpdate::DiscordWatermark {
            channel_id: "D001".to_string(),
            message_id: 42,
        })
        .await;
        app.apply_state_update(StateUpdate::SlackWatermark {
            channel_id: "C002".to_string(),
            ts: "2.000000".to_string(),
        })
        .await;
        // Apply 5 non-force-persist updates afterward. These are batched
        // and may not be on disk when the app drops (depends on the
        // debounce threshold).
        for i in 0..5 {
            app.apply_state_update(StateUpdate::TelegramOffset(1000 + i as i64))
                .await;
        }
        // App drops without graceful shutdown.
    }

    // The three force-persisted watermarks must be on disk.
    let reloaded = StateStore::load(&state_path).expect("reload state");
    let snap = reloaded.snapshot();
    assert_eq!(
        snap.slack_watermarks.get("C001").map(String::as_str),
        Some("1.000000"),
        "first slack watermark must be persisted before apply returns"
    );
    assert_eq!(
        snap.slack_watermarks.get("C002").map(String::as_str),
        Some("2.000000"),
        "second slack watermark must be persisted before apply returns"
    );
    assert_eq!(
        snap.discord_watermarks.get("D001").copied(),
        Some(42),
        "discord watermark must be persisted before apply returns"
    );
}

#[tokio::test]
async fn debounced_persist_lands_at_threshold() {
    // The 10th non-force update crosses the >= 10 threshold and triggers
    // a persist that includes the last-applied value (1_000_009 — the
    // 10th increment of the starting offset). The 11th update is applied
    // in memory but not persisted before drop.
    //
    // Tightened from `>= 1_000_009` (R1-P1): the boundary semantic is
    // exactly 1_000_009, not "at least." Looser comparisons would mask
    // a regression that persists too eagerly (e.g. on every update).
    let dir = tempdir().unwrap();
    let state_path = dir.path().join("terminus-state.json");

    {
        let (mut app, _state_rx) = test_app(dir.path());
        for i in 0..11 {
            app.apply_state_update(StateUpdate::TelegramOffset(1_000_000 + i as i64))
                .await;
        }
        // App drops here.
    }

    let reloaded = StateStore::load(&state_path).expect("reload state");
    let snap = reloaded.snapshot();
    assert_eq!(
        snap.telegram.offset, 1_000_009,
        "debounced persist should capture the 10th update exactly (1_000_009); \
         got telegram.offset = {}",
        snap.telegram.offset
    );
}

#[tokio::test]
async fn mark_clean_shutdown_persists_immediately() {
    let dir = tempdir().unwrap();
    let state_path = dir.path().join("terminus-state.json");
    let (mut app, _state_rx) = test_app(dir.path());

    {
        let early = StateStore::load(&state_path).expect("reload");
        assert!(
            !early.snapshot().last_clean_shutdown,
            "App::new should leave last_clean_shutdown = false (MarkDirty)"
        );
    }

    app.mark_clean_shutdown().await;

    let reloaded = StateStore::load(&state_path).expect("reload after clean shutdown");
    assert!(
        reloaded.snapshot().last_clean_shutdown,
        "mark_clean_shutdown should force-persist last_clean_shutdown = true"
    );
}
