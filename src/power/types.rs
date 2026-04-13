//! Shared type vocabulary for the power management subsystem.

/// State of the laptop lid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LidState {
    Open,
    Closed,
    /// No lid device — desktop or headless server; treated as always-open.
    Absent,
}

/// Current power source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerSource {
    Ac,
    Battery,
    /// No battery device detected; treat as AC.
    Unknown,
}

/// Push events that a platform impl may publish on its broadcast channel.
/// Polling impls never publish these; they exist for a future FFI impl.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub enum PowerEvent {
    LidChanged(LidState),
    PowerSourceChanged(PowerSource),
    Inhibit(bool),
}

/// Signals produced by the gap-detector task and sent over a dedicated
/// `mpsc::Sender<PowerSignal>` owned by `App`.
#[derive(Debug, Clone)]
pub enum PowerSignal {
    GapDetected {
        paused_at: chrono::DateTime<chrono::Utc>,
        resumed_at: chrono::DateTime<chrono::Utc>,
        gap: std::time::Duration,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn power_event_inhibit_debug_roundtrip() {
        let ev = PowerEvent::Inhibit(true);
        let s = format!("{:?}", ev);
        assert!(s.contains("Inhibit"));
        assert!(s.contains("true"));
    }
}
