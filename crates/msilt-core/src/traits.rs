use thiserror::Error;

use crate::{AuthorizedEcWrite, CapabilityFlags, ControlChange, ControlTarget, ErrorCode, HardwareState, OsdKind, PowerSource, Rgb8, SystemSnapshot, TelemetrySample, KEYBOARD_ZONE_COUNT};

pub type HalResult<T> = Result<T, HalError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum HalError {
    #[error("operation is unsupported by the backend")]
    Unsupported,
    #[error("backend is unavailable")]
    Unavailable,
    #[error("backend is busy; no I/O started")]
    Busy,
    #[error("backend I/O failed and the call has returned")]
    Io,
    #[error("backend I/O failed and returned with native status 0x{0:08X}")]
    NativeIo(u32),
    #[error("backend timed out; the call may still be in flight")]
    TimedOut,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HardwareReport {
    pub readable: CapabilityFlags,
    pub controls_detected: CapabilityFlags,
    pub state: HardwareState,
}

pub trait HardwareRead {
    fn inspect(&mut self) -> HalResult<HardwareReport>;
    fn sample_telemetry(&mut self) -> HalResult<TelemetrySample>;
}

/// Low-level I/O for a verified byte mapping; never exposed to IPC clients.
pub trait EcRegisterIo {
    fn read_byte(&mut self, address: u8) -> HalResult<u8>;
    fn write_byte(&mut self, write: &AuthorizedEcWrite) -> HalResult<()>;
}

/// Explicit non-EC allowlist. The daemon dispatches EC controls via `WriteGuard`.
pub trait PlatformControl {
    fn set_display_refresh_rate(&mut self, hz: u16) -> HalResult<u16>;
    fn set_display_brightness(&mut self, percent: u8) -> HalResult<u8>;
    fn set_color_profile(&mut self, profile_id: &str) -> HalResult<()>;
    fn set_keyboard_lighting(&mut self, zones: &[Rgb8; KEYBOARD_ZONE_COUNT]) -> HalResult<()>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum EffectivePowerMode {
    BatterySaver,
    BetterBattery,
    Balanced,
    HighPerformance,
    MaxPerformance,
    GameMode,
    MixedReality,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum AutomationEvent {
    StateChanged { revision: u64 },
    PowerSourceChanged { source: Option<PowerSource> },
    BatteryPercentChanged { percent: Option<u8> },
    PowerSavingChanged { enabled: bool },
    EffectivePowerModeChanged { mode: Option<EffectivePowerMode> },
    ForegroundChanged { app_id: Option<String>, instance: Option<u64> },
    ProcessStarted { app_id: String, instance: u64 },
    ProcessExited { instance: u64 },
    DisplayTopologyChanged { external_display_count: u16 },
    SessionChanged { unlocked: bool },
    IdleThresholdReached { idle_ms: u64 },
    ClockChanged,
    TimerElapsed,
    RulesReloaded,
    ControlSettled { target: ControlTarget, result: Result<(), ErrorCode> },
}

#[derive(Debug, Clone, Copy)]
pub struct AutomationContext<'a> {
    pub snapshot: &'a SystemSnapshot,
    pub monotonic_ms: u64,
    pub unix_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum AutomationAction {
    Acquire(AutomationLease),
    Release { target: ControlTarget, owner: LeaseOwner },
    Notify(OsdKind),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeaseOwner {
    Rule(u32),
    Scenario(u32),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutomationLease {
    pub owner: LeaseOwner,
    pub change: ControlChange,
    pub priority: u16,
    pub specificity: u16,
    pub expires_at_monotonic_ms: Option<u64>,
}

/// Pure evaluator invoked by the coordinator, never by a Win32 pump or callback.
/// The daemon authorizes and executes the proposed actions.
pub trait AutomationEngine {
    fn on_event(
        &mut self,
        event: &AutomationEvent,
        context: AutomationContext<'_>,
        actions: &mut Vec<AutomationAction>,
    );

    /// Uses the same clock and epoch as `AutomationContext::monotonic_ms`.
    fn next_deadline_monotonic_ms(&self) -> Option<u64>;
}
