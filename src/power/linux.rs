//! Linux power management implementation.
//!
//! Uses `systemd-inhibit` (child process) to hold the idle-sleep inhibit lock,
//! and reads sysfs/procfs directly for lid state and power-source detection.
//! No D-Bus / zbus dependency — see ADR in the consensus plan.

use std::sync::Arc;

use anyhow::Context as _;
use async_trait::async_trait;
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

use crate::power::types::{LidState, PowerEvent, PowerSource};
use crate::power::PowerManager;

// ---------------------------------------------------------------------------
// Internal state
// ---------------------------------------------------------------------------

struct Inner {
    /// Live `systemd-inhibit … sleep infinity` child, if the inhibit is held.
    child: Option<tokio::process::Child>,
    /// Cached inhibit state — best-effort, may be stale if the child crashed.
    inhibit_held: bool,
}

// ---------------------------------------------------------------------------
// Public struct
// ---------------------------------------------------------------------------

pub struct LinuxPowerManager {
    inner: Arc<Mutex<Inner>>,
    systemd_inhibit_available: bool,
}

impl LinuxPowerManager {
    /// Construct a new manager.  Probes `systemd-inhibit --version` synchronously
    /// (via `std::process::Command`); if the binary is absent or exits with an
    /// error the manager still works — `set_inhibit` becomes a no-op and a WARN
    /// is logged once.
    pub fn new() -> Self {
        let available = std::process::Command::new("systemd-inhibit")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);

        if !available {
            warn!(
                "power management unavailable: systemd-inhibit not present; \
                 idle sleep will not be inhibited"
            );
        }

        Self {
            inner: Arc::new(Mutex::new(Inner {
                child: None,
                inhibit_held: false,
            })),
            systemd_inhibit_available: available,
        }
    }
}

impl Default for LinuxPowerManager {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Drop: best-effort kill of the inhibit child
// ---------------------------------------------------------------------------

impl Drop for LinuxPowerManager {
    fn drop(&mut self) {
        // We can only try a synchronous best-effort kill here; async drop is
        // not supported in Rust.  `start_kill` sends SIGKILL without waiting.
        if let Ok(mut guard) = self.inner.try_lock() {
            if let Some(child) = guard.child.as_mut() {
                debug!("LinuxPowerManager dropped — sending SIGKILL to systemd-inhibit child");
                let _ = child.start_kill();
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Pure parsing helpers (pub(crate) so tests in this file can reach them)
// ---------------------------------------------------------------------------

/// Parse the content of a `/proc/acpi/button/lid/<name>/state` file.
///
/// Expected format:
/// ```text
/// state:      open
/// ```
/// Returns `LidState::Absent` on any unexpected / empty content.
pub(crate) fn parse_lid_state_file(content: &str) -> LidState {
    let lower = content.to_ascii_lowercase();
    if lower.contains("open") {
        LidState::Open
    } else if lower.contains("closed") {
        LidState::Closed
    } else {
        LidState::Absent
    }
}

/// Parse the content of a `/sys/class/power_supply/<name>/online` file.
///
/// Returns `true` if the AC adapter is online (`"1"` with optional trailing
/// newline), `false` otherwise.
pub(crate) fn parse_ac_online(content: &str) -> bool {
    content.trim() == "1"
}

// ---------------------------------------------------------------------------
// Trait implementation
// ---------------------------------------------------------------------------

#[async_trait]
impl PowerManager for LinuxPowerManager {
    /// Read lid state from `/proc/acpi/button/lid/`.
    ///
    /// Iterates directory entries; returns the state of the first entry whose
    /// `state` file can be read.  Returns `LidState::Absent` when the directory
    /// does not exist, is empty, or all reads fail.
    async fn lid_state(&self) -> LidState {
        let lid_dir = std::path::Path::new("/proc/acpi/button/lid");

        let mut dir = match tokio::fs::read_dir(lid_dir).await {
            Ok(d) => d,
            Err(e) => {
                debug!("lid_state: cannot open {}: {}", lid_dir.display(), e);
                return LidState::Absent;
            }
        };

        loop {
            match dir.next_entry().await {
                Ok(Some(entry)) => {
                    let state_path = entry.path().join("state");
                    match tokio::fs::read_to_string(&state_path).await {
                        Ok(content) => {
                            let result = parse_lid_state_file(&content);
                            if result != LidState::Absent {
                                return result;
                            }
                            // content was unrecognised — try next entry
                        }
                        Err(e) => {
                            debug!("lid_state: cannot read {}: {}", state_path.display(), e);
                        }
                    }
                }
                Ok(None) => break, // no more entries
                Err(e) => {
                    debug!("lid_state: directory iteration error: {}", e);
                    break;
                }
            }
        }

        LidState::Absent
    }

    /// Scan `/sys/class/power_supply/` for AC adapter entries.
    ///
    /// An entry is considered an AC adapter if its name starts with `"AC"` or
    /// `"ADP"` (both are common on Linux).  Reads `<entry>/online`; if any
    /// adapter reports `1`, returns `PowerSource::Ac`.  If all report `0`,
    /// returns `PowerSource::Battery`.  If no AC entry exists at all, returns
    /// `PowerSource::Unknown`.
    async fn power_source(&self) -> PowerSource {
        let supply_dir = std::path::Path::new("/sys/class/power_supply");

        let mut dir = match tokio::fs::read_dir(supply_dir).await {
            Ok(d) => d,
            Err(e) => {
                debug!("power_source: cannot open {}: {}", supply_dir.display(), e);
                return PowerSource::Unknown;
            }
        };

        let mut found_ac = false;
        let mut any_ac_online = false;

        loop {
            match dir.next_entry().await {
                Ok(Some(entry)) => {
                    let name = entry.file_name();
                    let name_str = name.to_string_lossy();
                    if !name_str.starts_with("AC") && !name_str.starts_with("ADP") {
                        continue;
                    }
                    found_ac = true;

                    let online_path = entry.path().join("online");
                    match tokio::fs::read_to_string(&online_path).await {
                        Ok(content) => {
                            if parse_ac_online(&content) {
                                any_ac_online = true;
                                // No need to keep scanning — we have our answer.
                                break;
                            }
                        }
                        Err(e) => {
                            debug!("power_source: cannot read {}: {}", online_path.display(), e);
                        }
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    debug!("power_source: directory iteration error: {}", e);
                    break;
                }
            }
        }

        if !found_ac {
            PowerSource::Unknown
        } else if any_ac_online {
            PowerSource::Ac
        } else {
            PowerSource::Battery
        }
    }

    /// Hold (`true`) or release (`false`) the idle-sleep inhibit lock via a
    /// `systemd-inhibit` child process.
    ///
    /// Idempotent: hold while already holding → no-op; release while not
    /// holding → no-op.  If `systemd_inhibit_available` is `false`, this is
    /// always a no-op.
    async fn set_inhibit(&self, hold: bool) -> anyhow::Result<()> {
        if !self.systemd_inhibit_available {
            debug!("set_inhibit({}) skipped: systemd-inhibit unavailable", hold);
            return Ok(());
        }

        let mut guard = self.inner.lock().await;

        if hold {
            // If a child exists, check whether it is still alive.
            if let Some(child) = guard.child.as_mut() {
                match child.try_wait() {
                    Ok(None) => {
                        // Child is still running — inhibit already held.
                        return Ok(());
                    }
                    Ok(Some(status)) => {
                        // Child exited unexpectedly; fall through to respawn.
                        warn!(
                            "systemd-inhibit child exited unexpectedly ({}); \
                             respawning",
                            status
                        );
                        guard.child = None;
                        guard.inhibit_held = false;
                    }
                    Err(e) => {
                        warn!("try_wait on systemd-inhibit child failed: {}", e);
                        // Treat as alive to avoid a respawn loop.
                        return Ok(());
                    }
                }
            }

            // Spawn a new inhibit child.
            let child = tokio::process::Command::new("systemd-inhibit")
                .args([
                    "--what=idle:sleep",
                    "--mode=block",
                    "--who=termbot",
                    "--why=termbot is running",
                    "sleep",
                    "infinity",
                ])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .context("failed to spawn systemd-inhibit")?;

            info!(
                pid = child.id().unwrap_or(0),
                "idle-sleep inhibit acquired via systemd-inhibit"
            );
            guard.child = Some(child);
            guard.inhibit_held = true;
        } else {
            // Release: kill child if present.
            if let Some(mut child) = guard.child.take() {
                child
                    .kill()
                    .await
                    .context("failed to kill systemd-inhibit")?;
                child
                    .wait()
                    .await
                    .context("failed to wait on systemd-inhibit after kill")?;
                info!("idle-sleep inhibit released (systemd-inhibit child stopped)");
            }
            guard.inhibit_held = false;
        }

        Ok(())
    }

    /// Returns the cached inhibit state (best-effort; may be stale).
    fn is_inhibited_cached(&self) -> bool {
        self.inner
            .try_lock()
            .map(|g| g.inhibit_held)
            .unwrap_or(false)
    }

    /// No push-event source on Linux (polling-only impl).
    fn subscribe_events(&self) -> Option<tokio::sync::broadcast::Receiver<PowerEvent>> {
        None
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // --- parse_lid_state_file ---

    #[test]
    fn lid_state_open() {
        assert_eq!(parse_lid_state_file("state:      open\n"), LidState::Open);
    }

    #[test]
    fn lid_state_closed() {
        assert_eq!(
            parse_lid_state_file("state:      closed\n"),
            LidState::Closed
        );
    }

    #[test]
    fn lid_state_empty() {
        assert_eq!(parse_lid_state_file(""), LidState::Absent);
    }

    #[test]
    fn lid_state_garbage() {
        assert_eq!(parse_lid_state_file("garbage"), LidState::Absent);
    }

    #[test]
    fn lid_state_case_insensitive_open() {
        assert_eq!(parse_lid_state_file("state:      OPEN\n"), LidState::Open);
    }

    #[test]
    fn lid_state_case_insensitive_closed() {
        assert_eq!(
            parse_lid_state_file("state:      CLOSED\n"),
            LidState::Closed
        );
    }

    // --- parse_ac_online ---

    #[test]
    fn ac_online_with_newline() {
        assert!(parse_ac_online("1\n"));
    }

    #[test]
    fn ac_online_without_newline() {
        assert!(parse_ac_online("1"));
    }

    #[test]
    fn ac_offline_with_newline() {
        assert!(!parse_ac_online("0\n"));
    }

    #[test]
    fn ac_online_empty() {
        assert!(!parse_ac_online(""));
    }

    #[test]
    fn ac_online_garbage() {
        assert!(!parse_ac_online("yes"));
    }

    // --- construction ---

    #[test]
    fn new_succeeds() {
        // May log WARN if systemd-inhibit is absent; that's expected.
        let pm = LinuxPowerManager::new();
        // is_inhibited_cached must be false before any set_inhibit call.
        assert!(!pm.is_inhibited_cached());
    }
}
