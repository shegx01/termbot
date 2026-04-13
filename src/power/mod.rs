//! Power management subsystem.
//!
//! Provides a platform-agnostic `PowerManager` trait and shared types.
//! Platform implementations (macOS, Linux) are added in Step 3.

pub mod fake;
pub mod policy;
pub mod types;

#[allow(unused_imports)]
pub use types::{LidState, PowerEvent, PowerSignal, PowerSource};

/// Trait implemented by platform-specific power managers and the test fake.
///
/// All methods are async-trait; the default `subscribe_events` returns `None`
/// so polling impls require no boilerplate while a future FFI impl can publish
/// events without a breaking trait change.
#[allow(dead_code)]
#[async_trait::async_trait]
pub trait PowerManager: Send + Sync + 'static {
    /// Current lid state. Returns `LidState::Absent` on devices with no lid.
    async fn lid_state(&self) -> LidState;

    /// Current power source. Returns `PowerSource::Unknown` when no battery
    /// device exists — callers treat `Unknown` identically to `Ac`.
    async fn power_source(&self) -> PowerSource;

    /// Idempotent: hold (`true`) or release (`false`) the idle-sleep assertion.
    async fn set_inhibit(&self, hold: bool) -> anyhow::Result<()>;

    /// Best-effort cached. May be stale if the underlying inhibit child died
    /// since last poll. Callers MUST NOT rely on this for correctness — it
    /// exists for metrics/logging only.
    fn is_inhibited_cached(&self) -> bool;

    /// Optional push-event subscription. Default `None` — polling impls have
    /// no source, but a future FFI impl can publish events here without a
    /// trait-level breaking change.
    fn subscribe_events(&self) -> Option<tokio::sync::broadcast::Receiver<PowerEvent>> {
        None
    }
}
