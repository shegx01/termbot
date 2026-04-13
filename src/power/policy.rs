//! OS-agnostic policy: derives the desired inhibit state from current
//! lid/power readings and the user's configuration.

use crate::power::types::{LidState, PowerSource};

/// Returns `true` when the idle-sleep inhibit assertion should be held.
///
/// Rules:
/// - Lid must be open (or absent, i.e. a desktop/headless machine).
/// - Power must be AC or Unknown *unless* `stayawake_on_battery` is `true`.
pub fn desired_inhibit(lid: LidState, power: PowerSource, stayawake_on_battery: bool) -> bool {
    let lid_permits = matches!(lid, LidState::Open | LidState::Absent);
    let power_permits = match power {
        PowerSource::Ac | PowerSource::Unknown => true,
        PowerSource::Battery => stayawake_on_battery,
    };
    lid_permits && power_permits
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Exhaustive matrix: 3 lid × 3 power × 2 config = 18 combinations.
    #[test]
    fn desired_inhibit_matrix() {
        use LidState::{Absent, Closed, Open};
        use PowerSource::{Ac, Battery, Unknown};

        struct Case {
            lid: LidState,
            power: PowerSource,
            battery_ok: bool,
            expected: bool,
        }

        let cases = [
            // --- Lid::Open ---
            Case {
                lid: Open,
                power: Ac,
                battery_ok: false,
                expected: true,
            },
            Case {
                lid: Open,
                power: Ac,
                battery_ok: true,
                expected: true,
            },
            Case {
                lid: Open,
                power: Battery,
                battery_ok: false,
                expected: false,
            },
            Case {
                lid: Open,
                power: Battery,
                battery_ok: true,
                expected: true,
            },
            Case {
                lid: Open,
                power: Unknown,
                battery_ok: false,
                expected: true,
            },
            Case {
                lid: Open,
                power: Unknown,
                battery_ok: true,
                expected: true,
            },
            // --- Lid::Absent (desktop) ---
            Case {
                lid: Absent,
                power: Ac,
                battery_ok: false,
                expected: true,
            },
            Case {
                lid: Absent,
                power: Ac,
                battery_ok: true,
                expected: true,
            },
            Case {
                lid: Absent,
                power: Battery,
                battery_ok: false,
                expected: false,
            },
            Case {
                lid: Absent,
                power: Battery,
                battery_ok: true,
                expected: true,
            },
            Case {
                lid: Absent,
                power: Unknown,
                battery_ok: false,
                expected: true,
            },
            Case {
                lid: Absent,
                power: Unknown,
                battery_ok: true,
                expected: true,
            },
            // --- Lid::Closed — inhibit must never be held ---
            Case {
                lid: Closed,
                power: Ac,
                battery_ok: false,
                expected: false,
            },
            Case {
                lid: Closed,
                power: Ac,
                battery_ok: true,
                expected: false,
            },
            Case {
                lid: Closed,
                power: Battery,
                battery_ok: false,
                expected: false,
            },
            Case {
                lid: Closed,
                power: Battery,
                battery_ok: true,
                expected: false,
            },
            Case {
                lid: Closed,
                power: Unknown,
                battery_ok: false,
                expected: false,
            },
            Case {
                lid: Closed,
                power: Unknown,
                battery_ok: true,
                expected: false,
            },
        ];

        for (i, c) in cases.iter().enumerate() {
            let got = desired_inhibit(c.lid, c.power, c.battery_ok);
            assert_eq!(
                got, c.expected,
                "case {i}: lid={:?} power={:?} battery_ok={} → expected {}, got {}",
                c.lid, c.power, c.battery_ok, c.expected, got
            );
        }
    }
}
