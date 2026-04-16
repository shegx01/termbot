//! macOS power management implementation.
//!
//! Hybrid approach (per ADR):
//!   - Sleep inhibit: spawn `caffeinate -i` child process; kill on release.
//!   - Lid / power state: shell out to `ioreg` and `pmset` (synchronous reads).
//!
//! No IOKit FFI, core-foundation, or new crates are used.

use std::sync::Arc;

use anyhow::Context;
use async_trait::async_trait;
use tokio::process::Child;
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

use crate::power::types::{LidState, PowerSource};
use crate::power::PowerManager;

// ---------------------------------------------------------------------------
// Internal state
// ---------------------------------------------------------------------------

struct Inner {
    /// Handle to a running `caffeinate -i` process, or `None` when no
    /// sleep-inhibit assertion is held.
    child: Option<Child>,
    /// Whether the inhibit assertion is currently believed to be held.
    inhibit_held: bool,
}

// ---------------------------------------------------------------------------
// Public type
// ---------------------------------------------------------------------------

pub struct MacOsPowerManager {
    inner: Arc<Mutex<Inner>>,
}

impl MacOsPowerManager {
    /// Create a new `MacOsPowerManager`.
    ///
    /// Probes whether `caffeinate` is on `PATH` and emits a `WARN` log if not.
    /// Callers can still construct the manager — `set_inhibit(true)` will
    /// return an error when caffeinate is actually needed.
    pub fn new() -> Self {
        // Light probe: run `caffeinate -h`; a non-zero exit or spawn failure
        // means caffeinate is absent or broken.
        let caffeinate_ok = std::process::Command::new("caffeinate")
            .arg("-h")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok();

        if !caffeinate_ok {
            warn!(
                "caffeinate not found on PATH — sleep inhibit via set_inhibit(true) will fail; \
                 lid/power queries are unaffected"
            );
        }

        Self {
            inner: Arc::new(Mutex::new(Inner {
                child: None,
                inhibit_held: false,
            })),
        }
    }
}

// ---------------------------------------------------------------------------
// Default (required by some trait patterns, matches FakePowerManager style)
// ---------------------------------------------------------------------------

impl Default for MacOsPowerManager {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Drop: synchronous best-effort kill of caffeinate child
// ---------------------------------------------------------------------------

impl Drop for MacOsPowerManager {
    fn drop(&mut self) {
        // We cannot .await here; use start_kill() which is non-blocking.
        match self.inner.try_lock() {
            Ok(mut guard) => {
                if let Some(child) = guard.child.as_mut() {
                    debug!(
                        pid = child.id(),
                        "MacOsPowerManager dropped; killing caffeinate child"
                    );
                    let _ = child.start_kill();
                }
            }
            Err(_) => {
                tracing::warn!(
                    "MacOsPowerManager::drop: mutex contended; caffeinate child may be orphaned \
                     — check for stale caffeinate processes"
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Trait impl
// ---------------------------------------------------------------------------

#[async_trait]
impl PowerManager for MacOsPowerManager {
    async fn lid_state(&self) -> LidState {
        match run_ioreg_clamshell().await {
            Ok(output) => parse_clamshell_output(&output),
            Err(e) => {
                debug!(err = %e, "ioreg lid query failed; returning Absent");
                LidState::Absent
            }
        }
    }

    async fn power_source(&self) -> PowerSource {
        match run_pmset_batt().await {
            Ok(output) => parse_power_source_output(&output),
            Err(e) => {
                debug!(err = %e, "pmset power query failed; returning Unknown");
                PowerSource::Unknown
            }
        }
    }

    async fn set_inhibit(&self, hold: bool) -> anyhow::Result<()> {
        let mut g = self.inner.lock().await;

        if hold {
            // Check if an existing child has died since last poll.
            if let Some(child) = g.child.as_mut() {
                match child.try_wait() {
                    Ok(Some(status)) => {
                        // Child exited unexpectedly.
                        warn!(status = ?status, "caffeinate child exited unexpectedly; will respawn");
                        g.child = None;
                        g.inhibit_held = false;
                    }
                    Ok(None) => {
                        // Still running — idempotent no-op.
                        debug!("set_inhibit(true): caffeinate already running; no-op");
                        return Ok(());
                    }
                    Err(e) => {
                        warn!(err = %e, "try_wait on caffeinate child failed; assuming dead, will respawn");
                        g.child = None;
                        g.inhibit_held = false;
                    }
                }
            }

            // Spawn caffeinate -i (prevents idle sleep; open until killed).
            let child = tokio::process::Command::new("caffeinate")
                .arg("-i")
                .spawn()
                .context("spawning caffeinate")?;

            info!(pid = child.id(), "sleep inhibit held via caffeinate -i");
            g.child = Some(child);
            g.inhibit_held = true;
        } else {
            // Release: kill existing child.
            if let Some(mut child) = g.child.take() {
                let pid = child.id();
                child.kill().await.context("killing caffeinate")?;
                child
                    .wait()
                    .await
                    .context("waiting for caffeinate to exit")?;
                info!(pid = pid, "sleep inhibit released; caffeinate killed");
                g.inhibit_held = false;
            } else {
                debug!("set_inhibit(false): no caffeinate child running; no-op");
            }
        }

        Ok(())
    }

    fn is_inhibited_cached(&self) -> bool {
        self.inner
            .try_lock()
            .map(|g| g.inhibit_held)
            .unwrap_or(false)
    }

    // subscribe_events: use the trait default (returns None)
}

// ---------------------------------------------------------------------------
// Shell-out helpers
// ---------------------------------------------------------------------------

async fn run_ioreg_clamshell() -> anyhow::Result<String> {
    let out = tokio::process::Command::new("ioreg")
        .args(["-r", "-k", "AppleClamshellState"])
        .output()
        .await
        .context("running ioreg")?;

    if !out.status.success() {
        anyhow::bail!("ioreg exited with status {}", out.status);
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

async fn run_pmset_batt() -> anyhow::Result<String> {
    let out = tokio::process::Command::new("pmset")
        .args(["-g", "batt"])
        .output()
        .await
        .context("running pmset")?;

    if !out.status.success() {
        anyhow::bail!("pmset exited with status {}", out.status);
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

// ---------------------------------------------------------------------------
// Pure parsing functions (also used in tests)
// ---------------------------------------------------------------------------

/// Parse the output of `ioreg -r -k AppleClamshellState`.
///
/// Scans for the first line containing `"AppleClamshellState" = Yes|No`.
/// Returns:
/// - `LidState::Closed`  if `= Yes`
/// - `LidState::Open`    if `= No`
/// - `LidState::Absent`  if the key is not present (e.g. Mac mini)
pub(crate) fn parse_clamshell_output(output: &str) -> LidState {
    for line in output.lines() {
        if line.contains("\"AppleClamshellState\"") {
            if line.contains("= Yes") {
                return LidState::Closed;
            } else if line.contains("= No") {
                return LidState::Open;
            }
        }
    }
    LidState::Absent
}

/// Parse the output of `pmset -g batt`.
///
/// The first line is the authoritative source; it contains either
/// `'AC Power'` or `'Battery Power'`.
pub(crate) fn parse_power_source_output(output: &str) -> PowerSource {
    for line in output.lines() {
        if line.contains("AC Power") {
            return PowerSource::Ac;
        }
        if line.contains("Battery Power") {
            return PowerSource::Battery;
        }
    }
    PowerSource::Unknown
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // --- Construction ---

    #[test]
    fn new_succeeds() {
        let _pm = MacOsPowerManager::new();
    }

    #[tokio::test]
    async fn initial_inhibit_cached_false() {
        let pm = MacOsPowerManager::new();
        assert!(!pm.is_inhibited_cached());
    }

    // --- Parsing unit tests (no shelling out, run anywhere) ---

    #[test]
    fn parse_clamshell_lid_open() {
        let input = r#"+-o AppleACPIPlatformExpert  <class AppleACPIPlatformExpert, ...>
    {
      "AppleClamshellState" = No
      "AppleClamshellCausesSleep" = Yes
    }"#;
        assert_eq!(parse_clamshell_output(input), LidState::Open);
    }

    #[test]
    fn parse_clamshell_lid_closed() {
        let input = r#"+-o AppleACPIPlatformExpert  <class AppleACPIPlatformExpert, ...>
    {
      "AppleClamshellState" = Yes
      "AppleClamshellCausesSleep" = Yes
    }"#;
        assert_eq!(parse_clamshell_output(input), LidState::Closed);
    }

    #[test]
    fn parse_clamshell_absent_no_key() {
        // Mac mini or headless — no AppleClamshellState key in ioreg output
        let input = r#"+-o IOACPIPlatformDevice  <class IOACPIPlatformDevice, ...>
    {
      "compatible" = <"acpi-device">
    }"#;
        assert_eq!(parse_clamshell_output(input), LidState::Absent);
    }

    #[test]
    fn parse_clamshell_empty_output() {
        assert_eq!(parse_clamshell_output(""), LidState::Absent);
    }

    #[test]
    fn parse_power_source_ac() {
        let input = "Now drawing from 'AC Power'\n";
        assert_eq!(parse_power_source_output(input), PowerSource::Ac);
    }

    #[test]
    fn parse_power_source_battery() {
        let input =
            "Now drawing from 'Battery Power'\n -InternalBattery-0 (id=...)	74%; discharging; 5:23 remaining present: true\n";
        assert_eq!(parse_power_source_output(input), PowerSource::Battery);
    }

    #[test]
    fn parse_power_source_unknown() {
        assert_eq!(parse_power_source_output(""), PowerSource::Unknown);
    }

    #[test]
    fn parse_power_source_unrecognised() {
        let input = "Now drawing from 'UPS Power'\n";
        assert_eq!(parse_power_source_output(input), PowerSource::Unknown);
    }

    // --- Live caffeinate test (macOS only, gated on caffeinate availability) ---

    /// Check whether the `caffeinate` binary is on PATH.
    fn caffeinate_available() -> bool {
        std::process::Command::new("caffeinate")
            .arg("-h")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok()
    }

    #[tokio::test]
    async fn set_inhibit_lifecycle_if_caffeinate_present() {
        if !caffeinate_available() {
            eprintln!("caffeinate not available; skipping live inhibit test");
            return;
        }

        let pm = MacOsPowerManager::new();

        // Initially not inhibited.
        assert!(!pm.is_inhibited_cached());

        // Hold.
        tokio::time::timeout(std::time::Duration::from_secs(5), pm.set_inhibit(true))
            .await
            .expect("set_inhibit(true) timed out")
            .expect("set_inhibit(true) returned an error");

        assert!(pm.is_inhibited_cached());

        // Idempotent second hold — should not error.
        tokio::time::timeout(std::time::Duration::from_secs(5), pm.set_inhibit(true))
            .await
            .expect("idempotent set_inhibit(true) timed out")
            .expect("idempotent set_inhibit(true) returned an error");

        assert!(pm.is_inhibited_cached());

        // Release.
        tokio::time::timeout(std::time::Duration::from_secs(5), pm.set_inhibit(false))
            .await
            .expect("set_inhibit(false) timed out")
            .expect("set_inhibit(false) returned an error");

        assert!(!pm.is_inhibited_cached());

        // Idempotent release — should not error.
        tokio::time::timeout(std::time::Duration::from_secs(5), pm.set_inhibit(false))
            .await
            .expect("idempotent set_inhibit(false) timed out")
            .expect("idempotent set_inhibit(false) returned an error");

        assert!(!pm.is_inhibited_cached());
    }
}
