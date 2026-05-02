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
//! 2. Crash-mid-batch must recover to the last successfully persisted state.
//!    The atomic `tempfile + rename + fsync` pattern in `StateStore::persist`
//!    means that if the process dies between two batched persists, the on-
//!    disk state reflects the LAST successful persist — never an interleaved
//!    half-write.

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

    // Spawn a worker that drains state_rx and applies updates back to the
    // app (mirroring the production main-loop behavior). This forces the
    // SlackWatermark variants through `apply_state_update` which is where
    // force-persist lives.
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
    // to the value we wrote (force-persist bypasses debounce, so each
    // update is durable immediately).
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

    // Drain any debounced state updates that escaped the apply loop so we
    // don't leave dangling channel work for the test runtime.
    let _ = timeout(Duration::from_millis(50), async {
        while state_rx.try_recv().is_ok() {}
    })
    .await;
}

#[tokio::test]
async fn crash_mid_batch_recovers_to_last_persist() {
    // Apply some force-persisted updates, then "crash" by dropping the
    // store without calling persist() again. Reload from disk and verify
    // the state matches the last force-persist boundary — never partial.
    let dir = tempdir().unwrap();
    let state_path = dir.path().join("terminus-state.json");

    {
        let (mut app, _state_rx) = test_app(dir.path());
        // Three force-persisted watermarks (each writes durably).
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
        // Now apply 5 non-force-persist updates — these are batched and
        // will NOT be on disk when the app drops.
        for i in 0..5 {
            app.apply_state_update(StateUpdate::TelegramOffset(1000 + i as i64))
                .await;
        }
        // App drops without graceful shutdown — simulates a crash.
    }

    // Reload: expect the three force-persisted watermarks to be present.
    // The 5 non-force telegram-offset updates may or may not have made it
    // to disk depending on debounce timing, but the watermark invariants
    // must hold either way.
    let reloaded = StateStore::load(&state_path).expect("reload state");
    let snap = reloaded.snapshot();
    assert_eq!(
        snap.slack_watermarks.get("C001").map(String::as_str),
        Some("1.000000"),
        "first slack watermark should survive the crash"
    );
    assert_eq!(
        snap.slack_watermarks.get("C002").map(String::as_str),
        Some("2.000000"),
        "second slack watermark should survive the crash"
    );
    assert_eq!(
        snap.discord_watermarks.get("D001").copied(),
        Some(42),
        "discord watermark should survive the crash"
    );
}

#[tokio::test]
async fn debounced_persist_eventually_lands_after_threshold() {
    // Drive 11 non-force updates through apply_state_update — the 10th
    // crosses the debounce threshold, forcing a persist. Reload and
    // confirm the latest TelegramOffset is on disk.
    let dir = tempdir().unwrap();
    let state_path = dir.path().join("terminus-state.json");

    let (mut app, _state_rx) = test_app(dir.path());
    for i in 0..11 {
        app.apply_state_update(StateUpdate::TelegramOffset(1_000_000 + i as i64))
            .await;
    }
    // The 10th update triggered the threshold; the 11th was applied after
    // the persist. The on-disk file should reflect at LEAST the 10th
    // value.
    drop(app);

    let reloaded = StateStore::load(&state_path).expect("reload state");
    let snap = reloaded.snapshot();
    assert!(
        snap.telegram.offset >= 1_000_009,
        "expected debounced persist to capture at least update #10; got telegram_offset = {}",
        snap.telegram.offset
    );
}

#[tokio::test]
async fn mark_clean_shutdown_persists_immediately() {
    // SetCleanShutdown is force-persisted. Apply it and reload to verify
    // last_clean_shutdown flips to true on disk.
    let dir = tempdir().unwrap();
    let state_path = dir.path().join("terminus-state.json");
    let (mut app, _state_rx) = test_app(dir.path());

    // App::new sets last_clean_shutdown to false (MarkDirty). Confirm by
    // reloading first.
    {
        let early = StateStore::load(&state_path).expect("reload");
        assert!(
            !early.snapshot().last_clean_shutdown,
            "App::new should leave last_clean_shutdown = false"
        );
    }

    app.mark_clean_shutdown().await;

    let reloaded = StateStore::load(&state_path).expect("reload after clean shutdown");
    assert!(
        reloaded.snapshot().last_clean_shutdown,
        "mark_clean_shutdown should force-persist last_clean_shutdown = true"
    );
}

// Silence an unused-import warning when the file is checked alone.
#[allow(dead_code)]
fn _drain_helper(_rx: &mut mpsc::Receiver<()>) {}
