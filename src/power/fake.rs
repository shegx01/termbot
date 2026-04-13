//! Test double for `PowerManager`. Publicly accessible so integration-style
//! tests in other modules can construct one without feature flags.

use std::sync::Arc;

use tokio::sync::Mutex;

use crate::power::types::{LidState, PowerEvent, PowerSource};
use crate::power::PowerManager;

struct FakeState {
    lid: LidState,
    power: PowerSource,
    inhibit_held: bool,
    inhibit_calls: Vec<bool>,
}

/// A fully in-memory `PowerManager` that allows tests to drive lid/power
/// transitions and inspect the inhibit call history.
pub struct FakePowerManager {
    inner: Arc<Mutex<FakeState>>,
}

#[allow(dead_code)]
impl FakePowerManager {
    pub fn new(lid: LidState, power: PowerSource) -> Self {
        Self {
            inner: Arc::new(Mutex::new(FakeState {
                lid,
                power,
                inhibit_held: false,
                inhibit_calls: Vec::new(),
            })),
        }
    }

    /// Drive a lid-state change; subsequent `lid_state()` calls return `l`.
    pub async fn set_lid(&self, l: LidState) {
        self.inner.lock().await.lid = l;
    }

    /// Drive a power-source change; subsequent `power_source()` calls return `p`.
    pub async fn set_power(&self, p: PowerSource) {
        self.inner.lock().await.power = p;
    }

    /// Returns the full ordered history of `set_inhibit(hold)` calls.
    pub async fn inhibit_history(&self) -> Vec<bool> {
        self.inner.lock().await.inhibit_calls.clone()
    }
}

#[async_trait::async_trait]
impl PowerManager for FakePowerManager {
    async fn lid_state(&self) -> LidState {
        self.inner.lock().await.lid
    }

    async fn power_source(&self) -> PowerSource {
        self.inner.lock().await.power
    }

    async fn set_inhibit(&self, hold: bool) -> anyhow::Result<()> {
        let mut s = self.inner.lock().await;
        s.inhibit_held = hold;
        s.inhibit_calls.push(hold);
        Ok(())
    }

    fn is_inhibited_cached(&self) -> bool {
        // `try_lock` gives us a non-async peek; falls back to false if contended
        // (safe: this is best-effort / metrics-only per the trait contract).
        self.inner
            .try_lock()
            .map(|s| s.inhibit_held)
            .unwrap_or(false)
    }

    fn subscribe_events(&self) -> Option<tokio::sync::broadcast::Receiver<PowerEvent>> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use LidState::{Absent, Closed, Open};
    use PowerSource::{Ac, Battery};

    #[tokio::test]
    async fn initial_state_reflected() {
        let pm = FakePowerManager::new(Open, Ac);
        assert_eq!(pm.lid_state().await, Open);
        assert_eq!(pm.power_source().await, Ac);
        assert!(!pm.is_inhibited_cached());
    }

    #[tokio::test]
    async fn set_inhibit_records_history() {
        let pm = FakePowerManager::new(Open, Ac);
        pm.set_inhibit(true).await.unwrap();
        pm.set_inhibit(false).await.unwrap();
        pm.set_inhibit(true).await.unwrap();
        assert_eq!(pm.inhibit_history().await, vec![true, false, true]);
        assert!(pm.is_inhibited_cached());
    }

    #[tokio::test]
    async fn set_lid_transition() {
        let pm = FakePowerManager::new(Open, Ac);
        assert_eq!(pm.lid_state().await, Open);
        pm.set_lid(Closed).await;
        assert_eq!(pm.lid_state().await, Closed);
        pm.set_lid(Absent).await;
        assert_eq!(pm.lid_state().await, Absent);
    }

    #[tokio::test]
    async fn set_power_transition() {
        let pm = FakePowerManager::new(Open, Ac);
        assert_eq!(pm.power_source().await, Ac);
        pm.set_power(Battery).await;
        assert_eq!(pm.power_source().await, Battery);
    }

    #[tokio::test]
    async fn subscribe_events_returns_none() {
        let pm = FakePowerManager::new(Open, Ac);
        assert!(pm.subscribe_events().is_none());
    }

    #[tokio::test]
    async fn inhibit_false_initially() {
        let pm = FakePowerManager::new(Closed, Battery);
        assert!(pm.inhibit_history().await.is_empty());
        assert!(!pm.is_inhibited_cached());
    }
}
