//! Periodic power-state supervisor.  Polls lid + power-source via the
//! PowerManager trait, applies the desired_inhibit policy, and calls
//! set_inhibit on transitions.  Emits PowerEvent on an optional
//! broadcast channel for observability.

use std::sync::Arc;

use tracing::{error, info};

use super::policy;
use super::types::{LidState, PowerEvent, PowerSource};
use super::PowerManager;

pub(crate) const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

/// Spawn the periodic power-state supervisor task.  Returns its JoinHandle.
///
/// - Polls `pm.lid_state()` and `pm.power_source()` every `POLL_INTERVAL`.
/// - Applies `policy::desired_inhibit` and calls `pm.set_inhibit` on transitions.
/// - Emits `PowerEvent` on `events_tx` when provided (lid, power, or inhibit change).
/// - On the first iteration, behaves as if the previous state was unknown — always
///   sets the initial inhibit (even if desired is `false`), so the child process
///   is in a known state from tick 1.
pub fn spawn_power_supervisor(
    pm: Arc<dyn PowerManager>,
    stayawake_on_battery: bool,
    events_tx: Option<tokio::sync::broadcast::Sender<PowerEvent>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        // `None` on each arm means "not yet polled" — first iteration always transitions.
        let mut prev_lid: Option<LidState> = None;
        let mut prev_power: Option<PowerSource> = None;
        let mut prev_inhibit: Option<bool> = None;

        loop {
            tokio::time::sleep(POLL_INTERVAL).await;

            let lid = pm.lid_state().await;
            let power = pm.power_source().await;

            // Emit lid-changed event if the lid moved.
            if prev_lid != Some(lid) {
                if let Some(tx) = &events_tx {
                    let _ = tx.send(PowerEvent::LidChanged(lid));
                }
                prev_lid = Some(lid);
            }

            // Emit power-source-changed event if the source moved.
            if prev_power != Some(power) {
                if let Some(tx) = &events_tx {
                    let _ = tx.send(PowerEvent::PowerSourceChanged(power));
                }
                prev_power = Some(power);
            }

            let desired = policy::desired_inhibit(lid, power, stayawake_on_battery);

            if prev_inhibit != Some(desired) {
                match pm.set_inhibit(desired).await {
                    Ok(()) => {
                        info!(
                            inhibit = desired,
                            lid = ?lid,
                            power = ?power,
                            "inhibit transitioned"
                        );
                        if let Some(tx) = &events_tx {
                            let _ = tx.send(PowerEvent::Inhibit(desired));
                        }
                        prev_inhibit = Some(desired);
                    }
                    Err(e) => {
                        error!(error = %e, "set_inhibit failed — will retry next tick");
                        // Do NOT update prev_inhibit so we retry next tick.
                    }
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use tokio::sync::broadcast;

    use super::*;
    use crate::power::fake::FakePowerManager;
    use crate::power::types::{LidState, PowerSource};

    /// Wait up to `timeout` for the inhibit history to satisfy a predicate.
    async fn wait_for_history(
        pm: &FakePowerManager,
        timeout: Duration,
        pred: impl Fn(&[bool]) -> bool,
    ) -> Vec<bool> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let h = pm.inhibit_history().await;
            if pred(&h) {
                return h;
            }
            if tokio::time::Instant::now() >= deadline {
                return h;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    #[tokio::test]
    async fn open_ac_holds_inhibit() {
        // (Open, Ac) → desired_inhibit = true → history should contain [true].
        let pm = Arc::new(FakePowerManager::new(LidState::Open, PowerSource::Ac));
        let (tx, _rx) = broadcast::channel(8);

        let _handle = spawn_power_supervisor(pm.clone(), false, Some(tx));

        let history = wait_for_history(&pm, Duration::from_secs(4), |h| !h.is_empty()).await;
        assert_eq!(
            history,
            vec![true],
            "Open+Ac should cause a single inhibit=true call"
        );
    }

    #[tokio::test]
    async fn lid_close_releases_inhibit() {
        // Start Open+Ac (inhibit=true), then close lid → inhibit=false appended.
        let pm = Arc::new(FakePowerManager::new(LidState::Open, PowerSource::Ac));
        let (tx, _rx) = broadcast::channel(8);

        let _handle = spawn_power_supervisor(pm.clone(), false, Some(tx));

        // Wait for the first inhibit=true.
        wait_for_history(&pm, Duration::from_secs(4), |h| h == [true]).await;

        // Close the lid.
        pm.set_lid(LidState::Closed).await;

        // Wait for inhibit=false to be appended.
        let history = wait_for_history(&pm, Duration::from_secs(4), |h| h == [true, false]).await;
        assert_eq!(
            history,
            vec![true, false],
            "Closing lid should append inhibit=false"
        );
    }

    #[tokio::test]
    async fn open_battery_no_stayawake_sets_inhibit_false() {
        // (Open, Battery, stayawake=false) → desired=false → set_inhibit(false) called once.
        // On startup prev_inhibit=None, desired=false → transitions and calls set_inhibit(false).
        let pm = Arc::new(FakePowerManager::new(LidState::Open, PowerSource::Battery));
        let (tx, _rx) = broadcast::channel(8);

        let _handle = spawn_power_supervisor(pm.clone(), false, Some(tx));

        let history = wait_for_history(&pm, Duration::from_secs(4), |h| !h.is_empty()).await;
        assert_eq!(
            history,
            vec![false],
            "Open+Battery with stayawake=false should call set_inhibit(false) once"
        );
    }

    #[tokio::test]
    async fn open_battery_stayawake_holds_inhibit() {
        // (Open, Battery, stayawake=true) → desired=true → set_inhibit(true) called.
        let pm = Arc::new(FakePowerManager::new(LidState::Open, PowerSource::Battery));
        let (tx, _rx) = broadcast::channel(8);

        let _handle = spawn_power_supervisor(pm.clone(), true, Some(tx));

        let history = wait_for_history(&pm, Duration::from_secs(4), |h| !h.is_empty()).await;
        assert_eq!(
            history,
            vec![true],
            "Open+Battery with stayawake=true should hold inhibit"
        );
    }
}
