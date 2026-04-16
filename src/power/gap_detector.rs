//! OS-agnostic sleep-gap detection via wall/monotonic clock divergence.
//! Per AD-3: ticks every 1s, compares SystemTime vs Instant deltas,
//! emits PowerSignal::GapDetected when the difference exceeds the threshold.
//!
//! # Known limitation — NTP step-forward false positives
//!
//! A large NTP step-forward (>30s) advances the wall clock without a
//! corresponding monotonic advance and will trigger a spurious
//! `PowerSignal::GapDetected`.  This is an accepted v1 trade-off: NTP steps
//! of this magnitude are rare on stable hosts and the resulting gap banner is
//! harmless (users see a restart-style banner rather than missing output).
//! A future revision could suppress the signal when the monotonic delta is
//! below `TICK_INTERVAL * 2` (indicating the host was never actually asleep).

use std::time::{Duration, Instant, SystemTime};

use tracing::warn;

use super::types::PowerSignal;

pub(crate) const GAP_THRESHOLD: Duration = Duration::from_secs(30);
pub(crate) const TICK_INTERVAL: Duration = Duration::from_secs(1);

/// Pure inner function for testability.
///
/// Returns `Some((paused_at, resumed_at, gap))` when
/// `wall_delta - monotonic_delta > GAP_THRESHOLD`, otherwise `None`.
pub(crate) fn detect_gap(
    last_mono: Instant,
    last_wall: SystemTime,
    now_mono: Instant,
    now_wall: SystemTime,
) -> Option<(SystemTime, SystemTime, Duration)> {
    let mono_delta = now_mono.duration_since(last_mono);
    let wall_delta = now_wall.duration_since(last_wall).unwrap_or_else(|e| {
        tracing::debug!(backwards_by = ?e.duration(), "wall clock went backwards (NTP step?)");
        Duration::ZERO
    });

    if wall_delta > mono_delta + GAP_THRESHOLD {
        let gap = wall_delta - mono_delta;
        Some((last_wall, now_wall, gap))
    } else {
        None
    }
}

/// Spawn the gap-detector background task.  Returns its JoinHandle.
/// The task exits when `tx` is dropped (mpsc send errors).
pub fn spawn_gap_detector(
    tx: tokio::sync::mpsc::Sender<PowerSignal>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut last_mono = Instant::now();
        let mut last_wall = SystemTime::now();

        loop {
            tokio::time::sleep(TICK_INTERVAL).await;

            let now_mono = Instant::now();
            let now_wall = SystemTime::now();

            if let Some((paused_wall, resumed_wall, gap)) =
                detect_gap(last_mono, last_wall, now_mono, now_wall)
            {
                let paused_at: chrono::DateTime<chrono::Utc> = paused_wall.into();
                let resumed_at: chrono::DateTime<chrono::Utc> = resumed_wall.into();
                warn!(
                    paused_at = %paused_at,
                    resumed_at = %resumed_at,
                    gap_secs = gap.as_secs_f64(),
                    "sleep gap detected"
                );
                let signal = PowerSignal::GapDetected {
                    paused_at,
                    resumed_at,
                    gap,
                };
                if tx.send(signal).await.is_err() {
                    // Receiver dropped — nothing left to notify, exit cleanly.
                    break;
                }
            }

            last_mono = now_mono;
            last_wall = now_wall;
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: build a fake `now_wall` by offsetting `base_wall` by the given duration.
    fn wall_plus(base: SystemTime, d: Duration) -> SystemTime {
        base + d
    }

    /// Helper: build a fake `now_mono` by advancing `base_mono` by the given duration.
    /// We can't construct arbitrary `Instant`s, so we sleep a tiny bit and record it,
    /// but for unit testing we rely on the arithmetic: the detect_gap function uses
    /// `now_mono.duration_since(last_mono)` which gives us the elapsed monotonic time.
    /// To avoid actual sleeps we use a trick: advance the wall clock artificially and
    /// keep the mono delta at effectively zero by re-using the same instant pair.
    ///
    /// For tests that need a controlled mono_delta, we use `Instant::now()` captured
    /// twice with a known sleep, but that's only for integration-style tests. Here we
    /// rely on the pure arithmetic in `detect_gap`.

    #[test]
    fn no_gap_when_mono_equals_wall() {
        // mono_delta == wall_delta → difference is zero, no signal.
        let base_mono = Instant::now();
        let base_wall = SystemTime::now();

        // Advance both by exactly 1s (simulate a normal tick).
        // We can't add to Instant directly, so we spin-wait a tiny amount
        // and fudge: the key is mono_delta == wall_delta.
        // Instead, use detect_gap with crafted SystemTime values and two
        // `Instant`s that are guaranteed identical to test the ≤ boundary.
        let now_mono = base_mono; // zero monotonic advance
        let now_wall = base_wall; // zero wall advance

        let result = detect_gap(base_mono, base_wall, now_mono, now_wall);
        assert!(
            result.is_none(),
            "identical timestamps should produce no gap"
        );
    }

    #[test]
    fn no_gap_on_tiny_wall_jitter() {
        // Wall advanced +5s beyond mono — under threshold.
        let base_mono = Instant::now();
        let base_wall = SystemTime::now();

        // mono: zero advance, wall: +5s → difference is 5s < 30s threshold.
        let now_mono = base_mono;
        let now_wall = wall_plus(base_wall, Duration::from_secs(5));

        let result = detect_gap(base_mono, base_wall, now_mono, now_wall);
        assert!(result.is_none(), "+5s wall jitter should not trigger a gap");
    }

    #[test]
    fn gap_detected_for_real_sleep() {
        // Wall jumped +90s, mono advanced only ~0s → gap ≈ 90s.
        let base_mono = Instant::now();
        let base_wall = SystemTime::now();

        let now_mono = base_mono; // monotonic essentially zero
        let now_wall = wall_plus(base_wall, Duration::from_secs(90));

        let result = detect_gap(base_mono, base_wall, now_mono, now_wall);
        assert!(
            result.is_some(),
            "90s wall jump with ~0s mono should trigger a gap"
        );

        let (_paused, _resumed, gap) = result.unwrap();
        // gap ≈ 90s (wall_delta=90s minus mono_delta≈0s)
        assert!(
            gap >= Duration::from_secs(89) && gap <= Duration::from_secs(91),
            "expected gap ≈ 90s, got {:?}",
            gap
        );
    }

    #[test]
    fn threshold_boundary_below() {
        // 29s wall, ~0s mono → difference 29s < 30s threshold → None.
        let base_mono = Instant::now();
        let base_wall = SystemTime::now();

        let now_mono = base_mono;
        let now_wall = wall_plus(base_wall, Duration::from_secs(29));

        let result = detect_gap(base_mono, base_wall, now_mono, now_wall);
        assert!(
            result.is_none(),
            "29s wall advance should be below the 30s threshold"
        );
    }

    #[test]
    fn threshold_boundary_above() {
        // 31s wall, ~0s mono → difference 31s > 30s threshold → Some.
        let base_mono = Instant::now();
        let base_wall = SystemTime::now();

        let now_mono = base_mono;
        let now_wall = wall_plus(base_wall, Duration::from_secs(31));

        let result = detect_gap(base_mono, base_wall, now_mono, now_wall);
        assert!(
            result.is_some(),
            "31s wall advance should exceed the 30s threshold"
        );

        let (_paused, _resumed, gap) = result.unwrap();
        assert!(
            gap >= Duration::from_secs(30) && gap <= Duration::from_secs(32),
            "expected gap ≈ 31s, got {:?}",
            gap
        );
    }
}
