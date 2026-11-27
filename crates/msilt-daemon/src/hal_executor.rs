use std::{
    io,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU8, Ordering},
        mpsc::{Receiver, SyncSender, sync_channel},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use msilt_core::*;
use msilt_hal::{Diagnostic, DiagnosticKind, oem_manager, startup};
use windows::Win32::{Devices::Display::*, Foundation::ERROR_INSUFFICIENT_BUFFER};

use crate::{
    automation_rules::dark_keyboard,
    events::Inbox,
    iocp::{Port, WAKE_HAL},
    lease::{TARGETS, observed},
};

pub trait HalBackend: HardwareRead + EcRegisterIo + PlatformControl {}
impl<T: HardwareRead + EcRegisterIo + PlatformControl> HalBackend for T {}

/// Supplies a reviewed mapping only after verifying complete device identity and writer exclusivity.
pub trait EcWritePolicy {
    fn authorize(&mut self, report: &HardwareReport) -> Result<&EcMap, EcWriteStatus>;
    fn writable_targets(&self) -> CapabilityFlags;
    fn register_intent(&self, change: &ControlChange) -> Option<EcWrite>;
    fn register_plan(&self, change: &ControlChange) -> Vec<(EcWrite, u8)> {
        self.register_intent(change)
            .into_iter()
            .map(|write| (write, u8::MAX))
            .collect()
    }
    fn fan_mode_write(&self, _mode: FanMode) -> Option<(EcWrite, u8)> {
        None
    }
    fn default_fan_mode(&self, _mode: PerformanceMode) -> Option<FanMode> {
        None
    }
    fn mode_register_plan(&self, mode: PerformanceMode, _custom_fan: bool) -> Vec<(EcWrite, u8)> {
        self.register_plan(&ControlChange::SetPerformanceMode { mode })
    }
}

pub struct HalBackends {
    device: Box<dyn HalBackend>,
    platform_writable: CapabilityFlags,
    ec: Option<Box<dyn EcWritePolicy>>,
    recovery: bool,
    readonly: bool,
    governor: EcGovernor,
}

impl HalBackends {
    /// Constructs a backend on the HAL thread, so COM and device objects need not implement Send.
    pub fn new(device: impl HalBackend + 'static, platform_writable: CapabilityFlags) -> Self {
        Self {
            device: Box::new(device),
            platform_writable: mask(platform_writable, PLATFORM_BITS),
            ec: None,
            recovery: false,
            readonly: false,
            governor: EcGovernor::default(),
        }
    }

    pub fn with_ec_policy(mut self, policy: impl EcWritePolicy + 'static) -> Self {
        self.ec = Some(Box::new(policy));
        self
    }

    pub fn noop() -> Self {
        Self::new(Noop, CapabilityFlags::NONE)
    }

    pub(crate) fn read_only(mut self) -> Self {
        self.readonly = true;
        self
    }

    fn inspect(&mut self) -> HalResult<Observation> {
        let mut report = self.device.inspect()?;
        normalize(&mut report.state);
        let controls = report.controls_detected.bits() & report.readable.bits();
        let mut allowed = self.platform_writable.bits();
        let mut ec_status = EcWriteStatus::Unavailable;
        if self.recovery {
            ec_status = EcWriteStatus::RecoveryRequired;
        } else if let Some(policy) = &mut self.ec {
            let ec_allowed = policy.writable_targets().bits() & EC_BITS;
            match policy.authorize(&report) {
                Ok(map) if map.matches(&report.state.identity) => {
                    ec_status = EcWriteStatus::Enabled;
                    allowed |= ec_allowed;
                }
                Ok(_) => ec_status = EcWriteStatus::ReadOnlyFirmware,
                Err(EcWriteStatus::Enabled) => ec_status = EcWriteStatus::UnverifiedMapping,
                Err(status) => ec_status = status,
            }
        } else if controls & EC_BITS != 0 {
            ec_status = EcWriteStatus::UnverifiedMapping;
        }
        if self.readonly {
            allowed = 0;
            if ec_status == EcWriteStatus::Enabled {
                ec_status = EcWriteStatus::ReadOnlyFirmware;
            }
        }
        let mut writable = 0;
        for target in TARGETS {
            let capability = target.capability().bits();
            if (controls & allowed & capability) != 0
                && observed(&report.state, target)
                    .is_some_and(|value| value.validate_against(&report.state).is_ok())
            {
                writable |= capability;
            }
        }
        Ok(Observation {
            report,
            writable: CapabilityFlags::from_bits_retain(writable),
            ec_status,
        })
    }

    #[cfg(test)]
    fn apply(
        &mut self,
        change: &ControlChange,
        expected: &HardwareIdentity,
        permit: impl FnMut() -> bool,
    ) -> Applied {
        self.apply_with_fan_profile(change, expected, None, permit)
    }

    fn apply_with_fan_profile(
        &mut self,
        change: &ControlChange,
        expected: &HardwareIdentity,
        fan_profile: Option<ModeFanProfile>,
        mut permit: impl FnMut() -> bool,
    ) -> Applied {
        let before = match self.inspect() {
            Ok(observation) => observation,
            Err(error) => return Applied::failure(error.into(), None),
        };
        if &before.report.state.identity != expected {
            return Applied::failure(ErrorCode::ConflictDetected, Some(before));
        }
        if let Err(error) = validate(
            &before.report.state,
            before.writable,
            before.ec_status,
            change,
        ) {
            return Applied::failure(error, Some(before));
        }
        let original = observed(&before.report.state, change.target());
        let effective_curves = resolve_mode_fan_curves(&before, change, fan_profile);
        let expected_fan_mode = match change {
            ControlChange::SetFanCurve { .. } => self
                .ec
                .as_ref()
                .and_then(|policy| policy.fan_mode_write(FanMode::Advanced).map(|_| FanMode::Advanced)),
            ControlChange::SetPerformanceMode { mode } => {
                if effective_curves.is_some() {
                    self.ec
                        .as_ref()
                        .and_then(|policy| policy.fan_mode_write(FanMode::Advanced).map(|_| FanMode::Advanced))
                } else {
                    self.ec
                        .as_ref()
                        .and_then(|policy| policy.default_fan_mode(*mode))
                }
            }
            _ => None,
        };
        let fan_mode_matches = |state: &HardwareState| {
            expected_fan_mode.is_none_or(|expected_mode| state.fan_mode == Some(expected_mode))
        };
        let curves_match = |state: &HardwareState| match effective_curves {
            Some((cpu, gpu)) => {
                cpu.is_none_or(|curve| state.cpu_fan.is_some_and(|fan| fan.curve == curve))
                    && gpu.is_none_or(|curve| state.gpu_fan.is_some_and(|fan| fan.curve == curve))
            }
            None => true,
        };
        // Keyboard HID has no hardware readback; always transmit explicit lighting frames
        // so re-applying after a Fn hotkey or external reset actually updates the LEDs.
        if !matches!(change, ControlChange::SetKeyboardLighting { .. })
            && original.as_ref() == Some(change)
            && fan_mode_matches(&before.report.state)
            && curves_match(&before.report.state)
        {
            if !permit() {
                return Applied::failure(ErrorCode::NotReady, Some(before));
            }
            return Applied {
                result: Ok(()),
                before: original,
                observation: Some(before),
            };
        }
        let ec = is_ec(change.target());
        let result = if ec {
            self.apply_ec(&before.report, change, effective_curves, permit)
        } else if permit() {
            self.apply_platform(change)
        } else {
            Err(ErrorCode::NotReady)
        };
        if result == Err(ErrorCode::TimedOut) {
            self.recovery |= ec;
            return Applied::failure(ErrorCode::TimedOut, None);
        }
        if matches!(
            result,
            Err(ErrorCode::Busy
                | ErrorCode::NotReady
                | ErrorCode::Unsupported
                | ErrorCode::ReadOnlyFirmware
                | ErrorCode::WriteNotAllowed
                | ErrorCode::DeniedWrite
                | ErrorCode::ConflictDetected)
        ) {
            return Applied::failure(result.expect_err("filtered error"), Some(before));
        }
        // A failed but completed write can have side effects; publish the new observation as well.
        let mut after = match self.inspect() {
            Ok(observation) => observation,
            Err(error) => {
                self.recovery |= ec;
                return Applied::failure(
                    if error == HalError::TimedOut {
                        ErrorCode::TimedOut
                    } else {
                        result.err().unwrap_or(ErrorCode::BackendUnavailable)
                    },
                    None,
                );
            }
        };
        let verified = &after.report.state.identity == expected
            && observed(&after.report.state, change.target()).as_ref() == Some(change)
            && fan_mode_matches(&after.report.state)
            && curves_match(&after.report.state);
        let result = result.and(if verified {
            Ok(())
        } else {
            Err(ErrorCode::VerificationFailed)
        });
        if ec && (!verified && result == Err(ErrorCode::VerificationFailed) || self.recovery) {
            self.recovery = true;
            after.ec_status = EcWriteStatus::RecoveryRequired;
            after.writable = mask(after.writable, !EC_BITS);
        }
        Applied {
            result,
            before: original,
            observation: Some(after),
        }
    }

    fn reset_fan_mode(
        &mut self,
        expected: &HardwareIdentity,
        mut permit: impl FnMut() -> bool,
    ) -> Applied {
        let before = match self.inspect() {
            Ok(observation) => observation,
            Err(error) => return Applied::failure(error.into(), None),
        };
        if &before.report.state.identity != expected {
            return Applied::failure(ErrorCode::ConflictDetected, Some(before));
        }
        if before.ec_status != EcWriteStatus::Enabled {
            return Applied::failure(ec_error(before.ec_status), Some(before));
        }
        if !before.writable.contains(CapabilityFlags::FAN_CURVE) {
            return Applied::failure(ErrorCode::Unsupported, Some(before));
        }
        if before.report.state.cooler_boost == Some(true) {
            return Applied::failure(ErrorCode::ConflictDetected, Some(before));
        }
        let Some(mode) = before.report.state.performance_mode else {
            return Applied::failure(ErrorCode::Unsupported, Some(before));
        };
        let Some((target_fan_mode, register)) = self.ec.as_ref().and_then(|policy| {
            let target_mode = policy.default_fan_mode(mode)?;
            let reg = policy.fan_mode_write(target_mode)?;
            Some((target_mode, reg))
        }) else {
            return Applied::failure(ErrorCode::Unsupported, Some(before));
        };
        if before.report.state.fan_mode == Some(target_fan_mode) {
            if !permit() {
                return Applied::failure(ErrorCode::NotReady, Some(before));
            }
            return Applied {
                result: Ok(()),
                before: None,
                observation: Some(before),
            };
        }
        let result = (|| -> Result<(), ErrorCode> {
            let policy = self.ec.as_mut().ok_or(ErrorCode::Unsupported)?;
            let map = policy.authorize(&before.report).map_err(ec_error)?;
            let mut guard = WriteGuard::new(map, &before.report.state.identity);
            self.governor
                .admit(ControlTarget::FanCurve { channel: FanChannel::Cpu }, Instant::now())?;
            let result = guarded_register_plan(
                &mut guard,
                self.device.as_mut(),
                map,
                &[register],
                &mut permit,
            );
            self.recovery |= guard.recovery_required()
                || matches!(
                    result,
                    Err(ErrorCode::TimedOut
                        | ErrorCode::RecoveryRequired
                        | ErrorCode::RollbackFailed)
                );
            result
        })();
        if result == Err(ErrorCode::TimedOut) {
            self.recovery = true;
            return Applied::failure(ErrorCode::TimedOut, None);
        }
        if matches!(
            result,
            Err(ErrorCode::Busy
                | ErrorCode::NotReady
                | ErrorCode::Unsupported
                | ErrorCode::ReadOnlyFirmware
                | ErrorCode::WriteNotAllowed
                | ErrorCode::DeniedWrite
                | ErrorCode::ConflictDetected)
        ) {
            return Applied::failure(result.expect_err("filtered error"), Some(before));
        }
        let mut after = match self.inspect() {
            Ok(observation) => observation,
            Err(error) => {
                self.recovery = true;
                return Applied::failure(
                    if error == HalError::TimedOut {
                        ErrorCode::TimedOut
                    } else {
                        result.err().unwrap_or(ErrorCode::BackendUnavailable)
                    },
                    None,
                );
            }
        };
        let verified = &after.report.state.identity == expected
            && after.report.state.performance_mode == Some(mode)
            && after.report.state.fan_mode == Some(target_fan_mode);
        let result = result.and(if verified {
            Ok(())
        } else {
            Err(ErrorCode::VerificationFailed)
        });
        if !verified && result == Err(ErrorCode::VerificationFailed) || self.recovery {
            self.recovery = true;
            after.ec_status = EcWriteStatus::RecoveryRequired;
            after.writable = mask(after.writable, !EC_BITS);
        }
        Applied {
            result,
            before: None,
            observation: Some(after),
        }
    }

    fn apply_platform(&mut self, change: &ControlChange) -> Result<(), ErrorCode> {
        match change {
            ControlChange::SetDisplayRefreshRate { hz } => {
                if self
                    .device
                    .set_display_refresh_rate(*hz)
                    .map_err(ErrorCode::from)?
                    != *hz
                {
                    return Err(ErrorCode::VerificationFailed);
                }
            }
            ControlChange::SetDisplayBrightness { percent } => {
                if self
                    .device
                    .set_display_brightness(*percent)
                    .map_err(ErrorCode::from)?
                    != *percent
                {
                    return Err(ErrorCode::VerificationFailed);
                }
            }
            ControlChange::SetColorProfile { profile_id } => {
                self.device.set_color_profile(profile_id)?
            }
            ControlChange::SetKeyboardLighting { zones } => {
                self.device.set_keyboard_lighting(zones)?
            }
            _ => return Err(ErrorCode::Unsupported),
        }
        Ok(())
    }

    fn apply_ec(
        &mut self,
        report: &HardwareReport,
        change: &ControlChange,
        effective_curves: Option<(Option<FanCurve>, Option<FanCurve>)>,
        mut permit: impl FnMut() -> bool,
    ) -> Result<(), ErrorCode> {
        let policy = self.ec.as_mut().ok_or(ErrorCode::Unsupported)?;
        let registers = match change {
            ControlChange::SetPerformanceMode { mode } => {
                policy.mode_register_plan(*mode, effective_curves.is_some())
            }
            _ => policy.register_plan(change),
        };
        let fan_activation = match change {
            ControlChange::SetFanCurve { .. } => policy.fan_mode_write(FanMode::Advanced),
            _ => None,
        };
        let map = policy.authorize(report).map_err(ec_error)?;
        let mut guard = WriteGuard::new(map, &report.state.identity);
        self.governor.admit(change.target(), Instant::now())?;
        let mut authorized = false;
        let result = match change {
            ControlChange::SetFanCurve { channel, curve } => {
                let original = match channel {
                    FanChannel::Cpu => report.state.cpu_fan.map(|f| f.curve),
                    FanChannel::Gpu => report.state.gpu_fan.map(|f| f.curve),
                };
                match commit_fan_curve(
                    &mut guard,
                    self.device.as_mut(),
                    *channel,
                    curve,
                    &mut permit,
                    &mut authorized,
                ) {
                    Ok(curve_changed) => {
                        if let Some(activation) = fan_activation {
                            let mode_result = guarded_register_plan(
                                &mut guard,
                                self.device.as_mut(),
                                map,
                                &[activation],
                                &mut || {
                                    if authorized {
                                        true
                                    } else {
                                        authorized = permit();
                                        authorized
                                    }
                                },
                            );
                            if let Err(error) = mode_result {
                                if curve_changed
                                    && !guard.recovery_required()
                                    && error != ErrorCode::TimedOut
                                {
                                    if let Some(orig) = original {
                                        if rollback_fan_curve(
                                            &mut guard,
                                            self.device.as_mut(),
                                            *channel,
                                            &orig,
                                        )
                                        .is_err()
                                        {
                                            Err(ErrorCode::RollbackFailed)
                                        } else {
                                            Err(error)
                                        }
                                    } else {
                                        Err(error)
                                    }
                                } else {
                                    Err(error)
                                }
                            } else {
                                Ok(())
                            }
                        } else {
                            Ok(())
                        }
                    }
                    Err(error) => Err(error),
                }
            }
            ControlChange::SetPerformanceMode { .. } if effective_curves.is_some() => {
                let (cpu_curve, gpu_curve) = effective_curves.expect("checked some");
                let orig_cpu = report.state.cpu_fan.map(|f| f.curve);
                let orig_gpu = report.state.gpu_fan.map(|f| f.curve);
                let mut cpu_changed = false;
                let mut gpu_changed = false;
                let mut step_result: Result<(), ErrorCode> = Ok(());
                if let Some(cpu) = cpu_curve {
                    match commit_fan_curve(
                        &mut guard,
                        self.device.as_mut(),
                        FanChannel::Cpu,
                        &cpu,
                        &mut permit,
                        &mut authorized,
                    ) {
                        Ok(changed) => cpu_changed = changed,
                        Err(error) => step_result = Err(error),
                    }
                }
                if step_result.is_ok() {
                    if let Some(gpu) = gpu_curve {
                        match commit_fan_curve(
                            &mut guard,
                            self.device.as_mut(),
                            FanChannel::Gpu,
                            &gpu,
                            &mut permit,
                            &mut authorized,
                        ) {
                            Ok(changed) => gpu_changed = changed,
                            Err(error) => step_result = Err(error),
                        }
                    }
                }
                if step_result.is_ok() {
                    step_result = guarded_register_plan(
                        &mut guard,
                        self.device.as_mut(),
                        map,
                        &registers,
                        &mut || {
                            if authorized {
                                true
                            } else {
                                authorized = permit();
                                authorized
                            }
                        },
                    );
                }
                if let Err(error) = step_result {
                    if !guard.recovery_required() && error != ErrorCode::TimedOut {
                        let mut rollback_ok = true;
                        if gpu_changed {
                            if let Some(orig) = orig_gpu {
                                rollback_ok &= rollback_fan_curve(
                                    &mut guard,
                                    self.device.as_mut(),
                                    FanChannel::Gpu,
                                    &orig,
                                )
                                .is_ok();
                            }
                        }
                        if rollback_ok && cpu_changed {
                            if let Some(orig) = orig_cpu {
                                rollback_ok &= rollback_fan_curve(
                                    &mut guard,
                                    self.device.as_mut(),
                                    FanChannel::Cpu,
                                    &orig,
                                )
                                .is_ok();
                            }
                        }
                        if !rollback_ok {
                            Err(ErrorCode::RollbackFailed)
                        } else {
                            Err(error)
                        }
                    } else {
                        Err(error)
                    }
                } else {
                    Ok(())
                }
            }
            _ => guarded_register_plan(
                &mut guard,
                self.device.as_mut(),
                map,
                &registers,
                &mut permit,
            ),
        };
        self.recovery |= guard.recovery_required()
            || matches!(
                result,
                Err(ErrorCode::TimedOut | ErrorCode::RecoveryRequired | ErrorCode::RollbackFailed)
            );
        result
    }
}

fn resolve_mode_fan_curves(
    before: &Observation,
    change: &ControlChange,
    fan_profile: Option<ModeFanProfile>,
) -> Option<(Option<FanCurve>, Option<FanCurve>)> {
    let ControlChange::SetPerformanceMode { mode } = change else {
        return None;
    };
    let profile = fan_profile?;
    if !profile.custom_enabled
        || !FanProfileStore::supports_custom(*mode)
        || before.report.state.cooler_boost == Some(true)
        || !before.writable.contains(CapabilityFlags::FAN_CURVE)
    {
        return None;
    }
    let check = |channel: FanChannel, curve: Option<FanCurve>| -> Result<Option<FanCurve>, ()> {
        let Some(curve) = curve else {
            return Ok(None);
        };
        let fan = match channel {
            FanChannel::Cpu => before.report.state.cpu_fan.ok_or(())?,
            FanChannel::Gpu => before.report.state.gpu_fan.ok_or(())?,
        };
        let limits = fan.limits.ok_or(())?;
        curve.validate_with_limits(&limits).map_err(|_| ())?;
        Ok(Some(curve))
    };
    match (check(FanChannel::Cpu, profile.cpu), check(FanChannel::Gpu, profile.gpu)) {
        (Ok(cpu), Ok(gpu)) if cpu.is_some() || gpu.is_some() => Some((cpu, gpu)),
        _ => None,
    }
}

fn commit_fan_curve(
    guard: &mut WriteGuard<'_>,
    io: &mut dyn EcRegisterIo,
    channel: FanChannel,
    curve: &FanCurve,
    permit: &mut impl FnMut() -> bool,
    authorized: &mut bool,
) -> Result<bool, ErrorCode> {
    let transaction = guard
        .prepare(io, EcWriteIntent::FanCurve { channel, curve })
        .map_err(|error| ErrorCode::from(&error))?;
    let changed = transaction
        .snapshot()
        .iter()
        .any(|item| item.before != item.after);
    if !*authorized {
        if !permit() {
            return Err(ErrorCode::NotReady);
        }
        *authorized = true;
    }
    transaction
        .commit()
        .map(|_| changed)
        .map_err(|error| ErrorCode::from(&error))
}

fn rollback_fan_curve(
    guard: &mut WriteGuard<'_>,
    io: &mut dyn EcRegisterIo,
    channel: FanChannel,
    original: &FanCurve,
) -> Result<(), ErrorCode> {
    guard
        .execute(
            io,
            EcWriteIntent::FanCurve {
                channel,
                curve: original,
            },
        )
        .map(|_| ())
        .map_err(|_| ErrorCode::RollbackFailed)
}

fn guarded_register_plan(
    guard: &mut WriteGuard<'_>,
    io: &mut dyn EcRegisterIo,
    map: &EcMap,
    registers: &[(EcWrite, u8)],
    permit: &mut impl FnMut() -> bool,
) -> Result<(), ErrorCode> {
    if registers.is_empty() {
        return Err(ErrorCode::Unsupported);
    }
    let mut before = Vec::<RegisterChange>::with_capacity(registers.len());
    for &(mut write, mask) in registers {
        if map.denies_write(write.address) {
            return Err(ErrorCode::DeniedWrite);
        }
        let raw = if mask != u8::MAX {
            let raw = io.read_byte(write.address).map_err(ErrorCode::from)?;
            write.value = (raw & !mask) | (write.value & mask);
            Some(raw)
        } else {
            None
        };
        let proposed = guard
            .prepare(io, EcWriteIntent::Register(write))
            .map_err(|error| ErrorCode::from(&error))?
            .snapshot()[0];
        if raw.is_some_and(|raw| raw != proposed.before) {
            return Err(ErrorCode::ConflictDetected);
        }
        before.push(proposed);
    }
    let mut completed = Vec::new();
    let result = (|| {
        let mut authorized = false;
        for change in &before {
            let write = EcWrite {
                address: change.address,
                value: change.after,
            };
            let transaction = guard
                .prepare(io, EcWriteIntent::Register(write))
                .map_err(|error| ErrorCode::from(&error))?;
            if transaction.snapshot()[0].before != change.before {
                return Err(ErrorCode::ConflictDetected);
            }
            if !authorized {
                if !permit() {
                    return Err(ErrorCode::NotReady);
                }
                authorized = true;
            }
            transaction
                .commit()
                .map_err(|error| ErrorCode::from(&error))?;
            if change.before != change.after {
                completed.push(*change);
            }
        }
        if before.len() > 1 {
            for change in &before {
                if io.read_byte(change.address).map_err(ErrorCode::from)? != change.after {
                    return Err(ErrorCode::VerificationFailed);
                }
            }
        }
        Ok(())
    })();
    if let Err(error) = result {
        if completed.is_empty() || guard.recovery_required() || error == ErrorCode::TimedOut {
            return Err(error);
        }
        // Core guards each byte and rolls back the failed byte. Restore earlier mode bytes
        // in reverse order only if they still have the values this transaction wrote.
        for change in completed.into_iter().rev() {
            let write = EcWrite {
                address: change.address,
                value: change.before,
            };
            let transaction = guard
                .prepare(io, EcWriteIntent::Register(write))
                .map_err(|_| ErrorCode::RollbackFailed)?;
            if transaction.snapshot()[0].before != change.after {
                return Err(ErrorCode::RollbackFailed);
            }
            transaction
                .commit()
                .map_err(|_| ErrorCode::RollbackFailed)?;
        }
    }
    result
}

const EC_BITS: u32 = CapabilityFlags::PERFORMANCE_MODE.bits()
    | CapabilityFlags::FAN_CURVE.bits()
    | CapabilityFlags::COOLER_BOOST.bits()
    | CapabilityFlags::CHARGE_LIMIT.bits();
const PLATFORM_BITS: u32 = CapabilityFlags::DISPLAY_REFRESH_RATE.bits()
    | CapabilityFlags::DISPLAY_BRIGHTNESS.bits()
    | CapabilityFlags::COLOR_PROFILE.bits()
    | CapabilityFlags::KEYBOARD_LIGHTING.bits();

pub(crate) fn is_ec(target: ControlTarget) -> bool {
    target.capability().bits() & EC_BITS != 0
}
pub(crate) fn mask(flags: CapabilityFlags, bits: u32) -> CapabilityFlags {
    CapabilityFlags::from_bits_retain(flags.bits() & bits)
}
pub(crate) fn ec_off(flags: CapabilityFlags) -> CapabilityFlags {
    mask(flags, !EC_BITS)
}

fn ec_error(status: EcWriteStatus) -> ErrorCode {
    match status {
        EcWriteStatus::ReadOnlyFirmware => ErrorCode::ReadOnlyFirmware,
        EcWriteStatus::ConflictDetected => ErrorCode::ConflictDetected,
        EcWriteStatus::RecoveryRequired => ErrorCode::RecoveryRequired,
        EcWriteStatus::UnverifiedMapping => ErrorCode::WriteNotAllowed,
        EcWriteStatus::Unavailable | EcWriteStatus::Enabled => ErrorCode::Unsupported,
    }
}

pub(crate) fn validate(
    state: &HardwareState,
    writable: CapabilityFlags,
    ec_status: EcWriteStatus,
    change: &ControlChange,
) -> Result<(), ErrorCode> {
    change.validate_parameters()?;
    if is_ec(change.target()) && ec_status != EcWriteStatus::Enabled {
        return Err(ec_error(ec_status));
    }
    if !writable.contains(change.capability()) {
        return Err(ErrorCode::Unsupported);
    }
    change.validate_against(state)?;
    let baseline = observed(state, change.target()).ok_or(ErrorCode::Unsupported)?;
    baseline.validate_against(state)?;
    if matches!(change, ControlChange::SetFanCurve { .. })
        && (state.cooler_boost == Some(true)
            || state.performance_mode == Some(PerformanceMode::Silent))
    {
        return Err(ErrorCode::ConflictDetected);
    }
    Ok(())
}

fn normalize(state: &mut HardwareState) {
    state.supported_performance_modes.truncate(4);
    state.supported_refresh_rates.retain(|&hz| hz != 0);
    state.supported_refresh_rates.sort_unstable();
    state.supported_refresh_rates.dedup();
    state.supported_refresh_rates.truncate(128);
    state
        .supported_charge_limits
        .retain(|v| (1..=100).contains(v));
    state.supported_charge_limits.sort_unstable();
    state.supported_charge_limits.dedup();
    state.supported_color_profiles.retain(|id| {
        !id.is_empty()
            && id.len() <= 64
            && id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    });
    state.supported_color_profiles.truncate(64);
    for identity in [
        &mut state.identity.board_id,
        &mut state.identity.ec_firmware,
    ] {
        if identity.as_ref().is_some_and(|id| id.len() > 128) {
            *identity = None;
        }
    }
    if state
        .color_profile_id
        .as_ref()
        .is_some_and(|id| id.len() > 64)
    {
        state.color_profile_id = None;
    }
}

pub struct DaemonBackends {
    pub hal: Box<dyn FnOnce() -> HalBackends + Send>,
    pub automation: Box<dyn AutomationEngine + Send>,
}

impl DaemonBackends {
    pub fn new(
        hal: impl FnOnce() -> HalBackends + Send + 'static,
        automation: impl AutomationEngine + Send + 'static,
    ) -> Self {
        Self {
            hal: Box::new(hal),
            automation: Box::new(automation),
        }
    }

    pub fn noop() -> Self {
        Self::new(HalBackends::noop, Noop)
    }
}

impl Default for DaemonBackends {
    fn default() -> Self {
        Self::new(
            || crate::BackendOptions::default().hal(),
            msilt_automation::RuleEngine::new(),
        )
    }
}

struct Noop;
impl HardwareRead for Noop {
    fn inspect(&mut self) -> HalResult<HardwareReport> {
        Ok(HardwareReport::default())
    }
    fn sample_telemetry(&mut self) -> HalResult<TelemetrySample> {
        Err(HalError::Unsupported)
    }
}
impl EcRegisterIo for Noop {
    fn read_byte(&mut self, _: u8) -> HalResult<u8> {
        Err(HalError::Unsupported)
    }
    fn write_byte(&mut self, _: &AuthorizedEcWrite) -> HalResult<()> {
        Err(HalError::Unsupported)
    }
}
impl PlatformControl for Noop {
    fn set_display_refresh_rate(&mut self, _: u16) -> HalResult<u16> {
        Err(HalError::Unsupported)
    }
    fn set_display_brightness(&mut self, _: u8) -> HalResult<u8> {
        Err(HalError::Unsupported)
    }
    fn set_color_profile(&mut self, _: &str) -> HalResult<()> {
        Err(HalError::Unsupported)
    }
    fn set_keyboard_lighting(&mut self, _: &[Rgb8; KEYBOARD_ZONE_COUNT]) -> HalResult<()> {
        Err(HalError::Unsupported)
    }
}
impl AutomationEngine for Noop {
    fn on_event(
        &mut self,
        _: &AutomationEvent,
        _: AutomationContext<'_>,
        _: &mut Vec<AutomationAction>,
    ) {
    }
    fn next_deadline_monotonic_ms(&self) -> Option<u64> {
        None
    }
}

#[derive(Default)]
struct EcGovernor {
    recent: Option<Instant>,
    fan: Option<Instant>,
    charge: Option<Instant>,
    bucket: Option<(u8, Instant)>,
}

impl EcGovernor {
    fn admit(&mut self, target: ControlTarget, now: Instant) -> Result<(), ErrorCode> {
        let cooling = matches!(target, ControlTarget::FanCurve { .. });
        let charging = matches!(target, ControlTarget::ChargeLimit {});
        let too_soon = |last: Option<Instant>, ms| {
            last.is_some_and(|last| now.duration_since(last) < Duration::from_millis(ms))
        };
        if too_soon(self.recent, 250)
            || cooling && too_soon(self.fan, 400)
            || charging && too_soon(self.charge, 5000)
        {
            return Err(ErrorCode::Busy);
        }
        let (available, since) = self.bucket.get_or_insert((4, now));
        let refills = (now.duration_since(*since).as_millis() / 2500).min(4) as u8;
        if refills != 0 {
            *available = available.saturating_add(refills).min(4);
            *since = if *available == 4 {
                now
            } else {
                *since + Duration::from_millis(u64::from(refills) * 2500)
            };
        }
        if *available == 0 {
            return Err(ErrorCode::Busy);
        }
        *available -= 1;
        self.recent = Some(now);
        if cooling {
            self.fan = Some(now);
        }
        if charging {
            self.charge = Some(now);
        }
        Ok(())
    }
}

pub(crate) struct Observation {
    pub(crate) report: HardwareReport,
    pub(crate) writable: CapabilityFlags,
    pub(crate) ec_status: EcWriteStatus,
}

pub(crate) struct Applied {
    pub(crate) result: Result<(), ErrorCode>,
    pub(crate) before: Option<ControlChange>,
    pub(crate) observation: Option<Observation>,
}

impl Applied {
    fn failure(error: ErrorCode, observation: Option<Observation>) -> Self {
        Self {
            result: Err(error),
            before: None,
            observation,
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) enum SystemTask {
    Inspect,
    Startup(bool),
    Oem(OemAction),
}

impl SystemTask {
    pub(crate) fn mutates(self) -> bool {
        !matches!(self, Self::Inspect)
    }
}

pub(crate) enum Work {
    Inspect {
        topology: bool,
    },
    Apply {
        change: ControlChange,
        expected: HardwareIdentity,
        lock_lighting: bool,
        fan_profile: Option<ModeFanProfile>,
    },
    ResetFanMode {
        expected: HardwareIdentity,
    },
    Sample,
    SystemTools {
        task: SystemTask,
        backup: PathBuf,
    },
}

fn allowed_work(work: &Work, inbox: &Inbox) -> bool {
    match work {
        Work::SystemTools {
            task: SystemTask::Inspect,
            ..
        } => true,
        Work::Apply {
            change,
            lock_lighting: true,
            ..
        } => inbox.can_dim_locked_keyboard() && dark_keyboard(change),
        _ => inbox.can_work(),
    }
}

pub(crate) enum Completion {
    Inspect(HalResult<Observation>, Option<u16>),
    Applied(Applied),
    Sample(HalResult<TelemetrySample>),
    SystemTools(Result<IpcReply, ErrorCode>),
    Skipped(ErrorCode),
}

const PENDING: u8 = 0;
const PREFLIGHT: u8 = 1;
const WRITING: u8 = 2;
const CANCELED: u8 = 3;

pub(crate) struct Ticket(AtomicU8);
impl Ticket {
    fn new() -> Self {
        Self(AtomicU8::new(PENDING))
    }
    fn start(&self) -> bool {
        self.0
            .compare_exchange(PENDING, PREFLIGHT, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }
    fn start_write(&self) -> bool {
        self.0
            .compare_exchange(PREFLIGHT, WRITING, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }
    pub(crate) fn cancel(&self) -> bool {
        self.0
            .compare_exchange(PENDING, CANCELED, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }
    pub(crate) fn stop_before_write(&self) -> bool {
        let mut phase = self.0.load(Ordering::Acquire);
        loop {
            if phase == WRITING {
                return false;
            }
            match self
                .0
                .compare_exchange_weak(phase, CANCELED, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => return true,
                Err(current) => phase = current,
            }
        }
    }
}

struct Job {
    work: Work,
    ticket: Arc<Ticket>,
    queue_by: Option<Instant>,
    budget_by: Instant,
}

pub(crate) struct Executor {
    factory: Option<Box<dyn FnOnce() -> HalBackends + Send>>,
    sender: Option<SyncSender<Job>>,
    receiver: Receiver<Completion>,
    receipts: Option<SyncSender<Completion>>,
    thread: Option<JoinHandle<()>>,
    port: Port,
    inbox: Arc<Inbox>,
}

impl Executor {
    pub(crate) fn new(
        port: &Port,
        inbox: Arc<Inbox>,
        factory: Box<dyn FnOnce() -> HalBackends + Send>,
    ) -> Self {
        let (receipts, receiver) = sync_channel(1);
        Self {
            factory: Some(factory),
            sender: None,
            receiver,
            receipts: Some(receipts),
            thread: None,
            port: port.clone(),
            inbox,
        }
    }

    pub(crate) fn submit(
        &mut self,
        work: Work,
        queue_by: Option<Instant>,
        budget_by: Instant,
    ) -> io::Result<Arc<Ticket>> {
        if self.sender.is_none() {
            let factory = self
                .factory
                .take()
                .ok_or_else(|| io::Error::other("HAL factory unavailable"))?;
            let result = self
                .receipts
                .take()
                .ok_or_else(|| io::Error::other("HAL completion channel unavailable"))?;
            let port = self.port.clone();
            let inbox = self.inbox.clone();
            let (sender, jobs) = sync_channel::<Job>(1);
            self.thread = Some(
                thread::Builder::new()
                    .name("msilt-hal".into())
                    .stack_size(512 * 1024)
                    .spawn(move || {
                        let mut backend = None;
                        let mut factory = Some(factory);
                        while let Ok(Job {
                            work,
                            ticket,
                            queue_by,
                            budget_by,
                        }) = jobs.recv()
                        {
                            let completion = if queue_by.is_some_and(|by| Instant::now() >= by)
                                || !ticket.start()
                            {
                                Completion::Skipped(ErrorCode::RequestExpired)
                            } else if !allowed_work(&work, &inbox) || Instant::now() >= budget_by {
                                Completion::Skipped(ErrorCode::NotReady)
                            } else {
                                let backend = backend.get_or_insert_with(|| {
                                    factory.take().expect("fresh HAL factory")()
                                });
                                match work {
                                    Work::Inspect { topology } => {
                                        let result = backend.inspect();
                                        let count = (topology
                                            && !matches!(result, Err(HalError::TimedOut)))
                                        .then(external_displays)
                                        .flatten();
                                        Completion::Inspect(result, count)
                                    }
                                    Work::Apply {
                                        change,
                                        expected,
                                        lock_lighting,
                                        fan_profile,
                                    } => Completion::Applied(backend.apply_with_fan_profile(
                                        &change,
                                        &expected,
                                        fan_profile,
                                        || {
                                            (if lock_lighting {
                                                inbox.can_dim_locked_keyboard()
                                                    && dark_keyboard(&change)
                                            } else {
                                                inbox.can_work()
                                            }) && Instant::now() < budget_by
                                                && ticket.start_write()
                                        },
                                    )),
                                    Work::ResetFanMode { expected } => {
                                        Completion::Applied(backend.reset_fan_mode(&expected, || {
                                            inbox.can_work()
                                                && Instant::now() < budget_by
                                                && ticket.start_write()
                                        }))
                                    }
                                    Work::Sample => {
                                        Completion::Sample(backend.device.sample_telemetry())
                                    }
                                    Work::SystemTools { task, backup } => {
                                        let result = if task.mutates() && backend.readonly {
                                            Err(ErrorCode::WriteNotAllowed)
                                        } else if task.mutates()
                                            && (!inbox.can_work() || !ticket.start_write())
                                        {
                                            Err(ErrorCode::NotReady)
                                        } else {
                                            system_tools(task, &backup)
                                        };
                                        Completion::SystemTools(result)
                                    }
                                }
                            };
                            if result.try_send(completion).is_err() || port.post(WAKE_HAL).is_err()
                            {
                                break;
                            }
                        }
                    })?,
            );
            self.sender = Some(sender);
        }
        let ticket = Arc::new(Ticket::new());
        self.sender
            .as_ref()
            .expect("started executor")
            .try_send(Job {
                work,
                ticket: ticket.clone(),
                queue_by,
                budget_by,
            })
            .map_err(|_| io::Error::other("HAL worker unavailable"))?;
        Ok(ticket)
    }

    pub(crate) fn completion(&self) -> Option<Completion> {
        self.receiver.try_recv().ok()
    }
}

fn system_tools(task: SystemTask, backup: &Path) -> Result<IpcReply, ErrorCode> {
    let (startup, oem) = match task {
        SystemTask::Inspect => (startup::inspect_startup(), oem_manager::inspect_oem(backup)),
        SystemTask::Startup(enabled) => (
            startup::set_startup(enabled).map_err(system_error)?,
            oem_manager::inspect_oem(backup),
        ),
        SystemTask::Oem(action) => {
            let oem = oem_manager::execute_oem_action(action, backup).map_err(system_error)?;
            (startup::inspect_startup(), oem)
        }
    };
    Ok(IpcReply::SystemTools { startup, oem })
}

fn system_error(error: Diagnostic) -> ErrorCode {
    eprintln!("msilt-daemon: system tools: {error}");
    match error.kind {
        DiagnosticKind::AccessDenied => ErrorCode::AccessDenied,
        DiagnosticKind::Unavailable => ErrorCode::BackendUnavailable,
        DiagnosticKind::Busy => ErrorCode::Busy,
        DiagnosticKind::ConflictDetected => ErrorCode::ConflictDetected,
        DiagnosticKind::Unsupported => ErrorCode::Unsupported,
        DiagnosticKind::TimedOut => ErrorCode::TimedOut,
        DiagnosticKind::InvalidData => ErrorCode::ValidationFailed,
        DiagnosticKind::Rejected => ErrorCode::VerificationFailed,
        DiagnosticKind::Io => ErrorCode::WriteFailed,
    }
}

impl Drop for Executor {
    fn drop(&mut self) {
        drop(self.sender.take());
        if let Some(thread) = self.thread.take().filter(JoinHandle::is_finished) {
            let _ = thread.join();
        }
    }
}

pub(crate) fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            elapsed.as_millis().min(u64::MAX as u128) as u64
        })
}

fn external_displays() -> Option<u16> {
    for _ in 0..3 {
        let (mut paths, mut modes) = (0, 0);
        if unsafe { GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut paths, &mut modes) }.0
            != 0
            || paths > 128
            || modes > 512
        {
            return None;
        }
        if paths == 0 {
            return Some(0);
        }
        let mut connectors = vec![DISPLAYCONFIG_PATH_INFO::default(); paths as usize];
        let mut data = vec![DISPLAYCONFIG_MODE_INFO::default(); modes as usize];
        let status = unsafe {
            QueryDisplayConfig(
                QDC_ONLY_ACTIVE_PATHS,
                &mut paths,
                connectors.as_mut_ptr(),
                &mut modes,
                data.as_mut_ptr(),
                None,
            )
        };
        if status == ERROR_INSUFFICIENT_BUFFER {
            continue;
        }
        if status.0 != 0 {
            return None;
        }
        let mut external = Vec::new();
        for path in connectors.get(..paths as usize)? {
            let target = path.targetInfo;
            if !target.targetAvailable.as_bool() {
                continue;
            }
            match target.outputTechnology {
                DISPLAYCONFIG_OUTPUT_TECHNOLOGY_INTERNAL
                | DISPLAYCONFIG_OUTPUT_TECHNOLOGY_LVDS
                | DISPLAYCONFIG_OUTPUT_TECHNOLOGY_DISPLAYPORT_EMBEDDED
                | DISPLAYCONFIG_OUTPUT_TECHNOLOGY_UDI_EMBEDDED => continue,
                DISPLAYCONFIG_OUTPUT_TECHNOLOGY_OTHER => return None,
                _ => {}
            }
            let id = (
                target.adapterId.HighPart,
                target.adapterId.LowPart,
                target.id,
            );
            if !external.contains(&id) {
                external.push(id);
            }
        }
        return Some(external.len() as u16);
    }
    None
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, rc::Rc};

    use super::*;

    const FLAG: CapabilityFlags = CapabilityFlags::COOLER_BOOST;
    const REGISTER: u8 = 0x22;

    fn identity() -> HardwareIdentity {
        HardwareIdentity {
            board_id: Some("TEST-BOARD".into()),
            ec_firmware: Some("TESTEC1.1".into()),
        }
    }

    struct TestEc {
        byte: u8,
        writes: Rc<Cell<u16>>,
    }

    impl HardwareRead for TestEc {
        fn inspect(&mut self) -> HalResult<HardwareReport> {
            Ok(HardwareReport {
                readable: FLAG,
                controls_detected: FLAG,
                state: HardwareState {
                    identity: identity(),
                    cooler_boost: Some(self.byte != 0),
                    ..Default::default()
                },
            })
        }
        fn sample_telemetry(&mut self) -> HalResult<TelemetrySample> {
            Err(HalError::Unsupported)
        }
    }

    impl EcRegisterIo for TestEc {
        fn read_byte(&mut self, address: u8) -> HalResult<u8> {
            assert_eq!(address, REGISTER);
            Ok(self.byte)
        }
        fn write_byte(&mut self, write: &AuthorizedEcWrite) -> HalResult<()> {
            assert_eq!(write.address(), REGISTER);
            self.writes.set(self.writes.get() + 1);
            self.byte = write.value();
            Ok(())
        }
    }

    impl PlatformControl for TestEc {
        fn set_display_refresh_rate(&mut self, hz: u16) -> HalResult<u16> {
            Noop.set_display_refresh_rate(hz)
        }
        fn set_display_brightness(&mut self, percent: u8) -> HalResult<u8> {
            Noop.set_display_brightness(percent)
        }
        fn set_color_profile(&mut self, id: &str) -> HalResult<()> {
            Noop.set_color_profile(id)
        }
        fn set_keyboard_lighting(&mut self, zones: &[Rgb8; KEYBOARD_ZONE_COUNT]) -> HalResult<()> {
            Noop.set_keyboard_lighting(zones)
        }
    }

    struct Policy {
        map: EcMap,
        address: u8,
    }
    impl EcWritePolicy for Policy {
        fn authorize(&mut self, _: &HardwareReport) -> Result<&EcMap, EcWriteStatus> {
            Ok(&self.map)
        }
        fn writable_targets(&self) -> CapabilityFlags {
            FLAG
        }
        fn register_intent(&self, _: &ControlChange) -> Option<EcWrite> {
            Some(EcWrite {
                address: self.address,
                value: 1,
            })
        }
    }

    fn policy(address: u8) -> Policy {
        let map = EcMap::parse_json(
            br#"{
            "schemaVersion":1,"boardId":"TEST-BOARD","firmwarePrefixes":["TESTEC1.1"],
            "writeAllowlist":[{"address":34,"values":[0,1]}]
        }"#,
        )
        .unwrap();
        Policy { map, address }
    }

    #[test]
    fn ec_writes_require_policy_and_guard_and_publish_verified_readback() {
        let writes = Rc::new(Cell::new(0));
        let start = || {
            HalBackends::new(
                TestEc {
                    byte: 0,
                    writes: writes.clone(),
                },
                CapabilityFlags::NONE,
            )
        };
        let on = ControlChange::SetCoolerBoost { enabled: true };
        assert_eq!(
            start().apply(&on, &identity(), || true).result,
            Err(ErrorCode::WriteNotAllowed)
        );
        assert_eq!(
            start()
                .with_ec_policy(policy(0x50))
                .apply(&on, &identity(), || true)
                .result,
            Err(ErrorCode::DeniedWrite)
        );
        assert_eq!(
            start()
                .with_ec_policy(policy(REGISTER))
                .apply(&on, &identity(), || false)
                .result,
            Err(ErrorCode::NotReady)
        );
        assert_eq!(writes.get(), 0);
        assert_eq!(
            start()
                .with_ec_policy(policy(REGISTER))
                .read_only()
                .apply(&on, &identity(), || true)
                .result,
            Err(ErrorCode::ReadOnlyFirmware)
        );
        assert_eq!(writes.get(), 0);
        let applied = start()
            .with_ec_policy(policy(REGISTER))
            .apply(&on, &identity(), || true);
        assert_eq!(applied.result, Ok(()));
        assert_eq!(
            applied.before,
            Some(ControlChange::SetCoolerBoost { enabled: false })
        );
        assert_eq!(
            applied.observation.unwrap().report.state.cooler_boost,
            Some(true)
        );
        assert_eq!(writes.get(), 1);
    }

    #[test]
    fn explicit_readonly_masks_even_an_injected_platform_writer() {
        let flag = CapabilityFlags::DISPLAY_REFRESH_RATE;
        let mut device = msilt_hal::mock::MockHal::default();
        device.report = HardwareReport {
            readable: flag,
            controls_detected: flag,
            state: HardwareState {
                identity: identity(),
                display_refresh_hz: Some(240),
                supported_refresh_rates: vec![60, 240],
                ..Default::default()
            },
        };
        let mut backend = HalBackends::new(device, flag).read_only();
        let before = backend.inspect().unwrap();
        assert_eq!(before.writable, CapabilityFlags::NONE);
        let result = backend.apply(
            &ControlChange::SetDisplayRefreshRate { hz: 60 },
            &identity(),
            || true,
        );
        assert_eq!(result.result, Err(ErrorCode::Unsupported));
        assert_eq!(backend.inspect().unwrap().report, before.report);
    }

    #[test]
    fn compound_scene_writes_preserve_masks_and_roll_back_on_a_settled_error() {
        use msilt_hal::{
            mock::{MockHal, MockWrite},
            ms17s3::*,
        };
        let identity = HardwareIdentity {
            board_id: Some("MS-17S3".into()),
            ec_firmware: Some("17S3EMS1.107".into()),
        };
        let map = write_map().unwrap();
        let mode = [
            (
                EcWrite {
                    address: FAN_MODE,
                    value: 0x0D,
                },
                u8::MAX,
            ),
            (
                EcWrite {
                    address: SHIFT,
                    value: 0xC2,
                },
                u8::MAX,
            ),
            (
                EcWrite {
                    address: BATTERY_MODE,
                    value: 0x0F,
                },
                BATTERY_MODE_MASK,
            ),
        ];
        let mut device = MockHal::default()
            .with_register(FAN_MODE, 0x1D)
            .with_register(SHIFT, 0xC1)
            .with_register(BATTERY_MODE, 0xB0)
            .with_register(COOLER, 0x12);
        device.script_write(MockWrite::Apply);
        device.script_write(MockWrite::FailAfter(HalError::Io));
        let mut guard = WriteGuard::new(map, &identity);
        assert_eq!(
            guarded_register_plan(&mut guard, &mut device, map, &mode, &mut || true),
            Err(ErrorCode::WriteFailed)
        );
        assert_eq!(
            [
                device.register(FAN_MODE),
                device.register(SHIFT),
                device.register(BATTERY_MODE)
            ],
            [Some(0x1D), Some(0xC1), Some(0xB0)]
        );
        assert!(!guard.recovery_required());
        let mut tickets = 0;
        guarded_register_plan(&mut guard, &mut device, map, &mode, &mut || {
            tickets += 1;
            true
        })
        .unwrap();
        assert_eq!(tickets, 1);
        assert_eq!(device.register(BATTERY_MODE), Some(0xBF));
        guarded_register_plan(
            &mut guard,
            &mut device,
            map,
            &[(
                EcWrite {
                    address: COOLER,
                    value: COOLER_MASK,
                },
                COOLER_MASK,
            )],
            &mut || true,
        )
        .unwrap();
        assert_eq!(device.register(COOLER), Some(0x92));
    }

    struct Ms17s3MockEc {
        inner: msilt_hal::mock::MockHal,
    }

    impl Ms17s3MockEc {
        fn new(mode: PerformanceMode, fan_mode: u8, cpu_duties: [u8; 6], gpu_duties: [u8; 6]) -> Self {
            use msilt_hal::ms17s3::*;
            let (shift, _, battery) = mode_registers(mode);
            let mut inner = msilt_hal::mock::MockHal::default()
                .with_register(COOLER, 0x02)
                .with_register(SHIFT, shift)
                .with_register(FAN_MODE, fan_mode)
                .with_register(BATTERY_MODE, 0xB0 | battery);
            for i in 0..6 {
                inner = inner
                    .with_register(0x72 + i as u8, cpu_duties[i])
                    .with_register(0x8A + i as u8, gpu_duties[i]);
            }
            Self { inner }
        }

        fn curve(temps: [u8; 6], duties: [u8; 6]) -> FanCurve {
            FanCurve {
                points: std::array::from_fn(|i| FanPoint {
                    temperature_c: temps[i],
                    duty_percent: duties[i],
                }),
            }
        }
    }

    impl HardwareRead for Ms17s3MockEc {
        fn inspect(&mut self) -> HalResult<HardwareReport> {
            use msilt_hal::ms17s3::*;
            let map = write_map().unwrap();
            let shift = self.inner.register(SHIFT).unwrap();
            let fan = self.inner.register(FAN_MODE).unwrap();
            let battery = self.inner.register(BATTERY_MODE).unwrap();
            let cooler = self.inner.register(COOLER).unwrap();
            let mode = match (shift, fan, battery & BATTERY_MODE_MASK) {
                (0xC1 | 0xC2 | 0xC4, 0x0D | 0x1D | 0x8D, 0x0F) => Some(PerformanceMode::SuperBattery),
                (0xC1 | 0xC2, 0x1D, 0) => Some(PerformanceMode::Silent),
                (0xC1, 0x0D | 0x8D, 0) => Some(PerformanceMode::Balanced),
                (0xC4, 0x0D | 0x8D, 0) => Some(PerformanceMode::ExtremePerformance),
                _ => None,
            };
            let cpu_duties: [u8; 6] = std::array::from_fn(|i| self.inner.register(0x72 + i as u8).unwrap());
            let gpu_duties: [u8; 6] = std::array::from_fn(|i| self.inner.register(0x8A + i as u8).unwrap());
            let flags = CapabilityFlags::PERFORMANCE_MODE
                | CapabilityFlags::FAN_CURVE
                | CapabilityFlags::COOLER_BOOST;
            Ok(HardwareReport {
                readable: flags,
                controls_detected: flags,
                state: HardwareState {
                    identity: HardwareIdentity {
                        board_id: Some("MS-17S3".into()),
                        ec_firmware: Some("17S3EMS1.107".into()),
                    },
                    supported_performance_modes: MODES.to_vec(),
                    performance_mode: mode,
                    fan_mode: decode_fan_mode(fan),
                    cpu_fan: Some(FanState {
                        curve: Self::curve([0, 48, 56, 62, 70, 78], cpu_duties),
                        limits: map.fan_curve_limits(FanChannel::Cpu),
                    }),
                    gpu_fan: Some(FanState {
                        curve: Self::curve([0, 43, 48, 56, 65, 72], gpu_duties),
                        limits: map.fan_curve_limits(FanChannel::Gpu),
                    }),
                    cooler_boost: Some(cooler & COOLER_MASK != 0),
                    ..Default::default()
                },
            })
        }
        fn sample_telemetry(&mut self) -> HalResult<TelemetrySample> {
            Err(HalError::Unsupported)
        }
    }

    impl EcRegisterIo for Ms17s3MockEc {
        fn read_byte(&mut self, address: u8) -> HalResult<u8> {
            self.inner.read_byte(address)
        }
        fn write_byte(&mut self, permit: &AuthorizedEcWrite) -> HalResult<()> {
            self.inner.write_byte(permit)
        }
    }

    impl PlatformControl for Ms17s3MockEc {
        fn set_display_refresh_rate(&mut self, hz: u16) -> HalResult<u16> {
            Noop.set_display_refresh_rate(hz)
        }
        fn set_display_brightness(&mut self, percent: u8) -> HalResult<u8> {
            Noop.set_display_brightness(percent)
        }
        fn set_color_profile(&mut self, id: &str) -> HalResult<()> {
            Noop.set_color_profile(id)
        }
        fn set_keyboard_lighting(&mut self, zones: &[Rgb8; KEYBOARD_ZONE_COUNT]) -> HalResult<()> {
            Noop.set_keyboard_lighting(zones)
        }
    }

    struct Ms17s3TestPolicy;
    impl EcWritePolicy for Ms17s3TestPolicy {
        fn authorize(&mut self, _: &HardwareReport) -> Result<&EcMap, EcWriteStatus> {
            Ok(msilt_hal::ms17s3::write_map().unwrap())
        }
        fn writable_targets(&self) -> CapabilityFlags {
            CapabilityFlags::PERFORMANCE_MODE
                | CapabilityFlags::FAN_CURVE
                | CapabilityFlags::COOLER_BOOST
        }
        fn register_intent(&self, _: &ControlChange) -> Option<EcWrite> {
            None
        }
        fn fan_mode_write(&self, mode: FanMode) -> Option<(EcWrite, u8)> {
            Some((
                EcWrite {
                    address: msilt_hal::ms17s3::FAN_MODE,
                    value: msilt_hal::ms17s3::fan_mode_byte(mode),
                },
                u8::MAX,
            ))
        }
        fn default_fan_mode(&self, mode: PerformanceMode) -> Option<FanMode> {
            Some(msilt_hal::ms17s3::default_fan_mode(mode))
        }
        fn mode_register_plan(&self, mode: PerformanceMode, custom_fan: bool) -> Vec<(EcWrite, u8)> {
            use msilt_hal::ms17s3::*;
            let entry = |address, value, mask| (EcWrite { address, value }, mask);
            let (shift, default_fan, battery) = mode_registers(mode);
            let fan = if custom_fan && FanProfileStore::supports_custom(mode) {
                FAN_MODE_ADVANCED
            } else {
                default_fan
            };
            vec![
                entry(FAN_MODE, fan, u8::MAX),
                entry(SHIFT, shift, u8::MAX),
                entry(BATTERY_MODE, battery, BATTERY_MODE_MASK),
            ]
        }
    }

    #[test]
    fn fan_curve_apply_activates_advanced_mode_rolls_back_and_blocks_silent() {
        use msilt_hal::ms17s3::*;
        let id = HardwareIdentity {
            board_id: Some("MS-17S3".into()),
            ec_firmware: Some("17S3EMS1.107".into()),
        };
        let cpu_floor = [0, 36, 42, 52, 76, 84];
        let gpu_floor = [0, 36, 46, 56, 80, 88];
        let same_curve = Ms17s3MockEc::curve([0, 48, 56, 62, 70, 78], cpu_floor);

        // Even when curve bytes already match EC RAM, applying in Auto mode must write 0xD4 = 0x8D.
        let mut backend = HalBackends::new(
            Ms17s3MockEc::new(PerformanceMode::Balanced, FAN_MODE_AUTO, cpu_floor, gpu_floor),
            CapabilityFlags::NONE,
        )
        .with_ec_policy(Ms17s3TestPolicy);
        let applied = backend.apply(
            &ControlChange::SetFanCurve {
                channel: FanChannel::Cpu,
                curve: same_curve,
            },
            &id,
            || true,
        );
        assert_eq!(applied.result, Ok(()));
        assert_eq!(
            applied.observation.unwrap().report.state.fan_mode,
            Some(FanMode::Advanced)
        );

        // Resetting fan mode returns 0xD4 to Auto (0x0D).
        std::thread::sleep(Duration::from_millis(420));
        let reset = backend.reset_fan_mode(&id, || true);
        assert_eq!(reset.result, Ok(()));
        assert_eq!(
            reset.observation.unwrap().report.state.fan_mode,
            Some(FanMode::Auto)
        );

        // Silent mode rejects SetFanCurve with ConflictDetected without touching EC.
        let mut silent_backend = HalBackends::new(
            Ms17s3MockEc::new(PerformanceMode::Silent, FAN_MODE_SILENT, cpu_floor, gpu_floor),
            CapabilityFlags::NONE,
        )
        .with_ec_policy(Ms17s3TestPolicy);
        let rejected = silent_backend.apply(
            &ControlChange::SetFanCurve {
                channel: FanChannel::Cpu,
                curve: same_curve,
            },
            &id,
            || true,
        );
        assert_eq!(rejected.result, Err(ErrorCode::ConflictDetected));

        // Failure writing 0xD4=0x8D rolls back previously written curve bytes cleanly.
        let mut failing_ec =
            Ms17s3MockEc::new(PerformanceMode::Balanced, FAN_MODE_AUTO, cpu_floor, gpu_floor);
        // 1 changed curve byte succeeds, then FAN_MODE write fails cleanly.
        failing_ec.inner.script_write(msilt_hal::mock::MockWrite::Apply);
        failing_ec
            .inner
            .script_write(msilt_hal::mock::MockWrite::FailBefore(HalError::Io));
        let mut rollback_backend = HalBackends::new(failing_ec, CapabilityFlags::NONE)
            .with_ec_policy(Ms17s3TestPolicy);
        let bumped = Ms17s3MockEc::curve([0, 48, 56, 62, 70, 78], [0, 36, 42, 52, 76, 95]);
        let failed = rollback_backend.apply(
            &ControlChange::SetFanCurve {
                channel: FanChannel::Cpu,
                curve: bumped,
            },
            &id,
            || true,
        );
        assert_eq!(failed.result, Err(ErrorCode::WriteFailed));
        let after = failed.observation.unwrap().report.state;
        assert_eq!(after.fan_mode, Some(FanMode::Auto));
        assert_eq!(after.cpu_fan.unwrap().curve, same_curve);
    }

    #[test]
    fn mode_switch_applies_memorized_curves_and_reverts_to_auto_when_disabled() {
        use msilt_hal::ms17s3::*;
        let id = HardwareIdentity {
            board_id: Some("MS-17S3".into()),
            ec_firmware: Some("17S3EMS1.107".into()),
        };
        let cpu_floor = [0, 36, 42, 52, 76, 84];
        let gpu_floor = [0, 36, 46, 56, 80, 88];
        let ext_cpu = Ms17s3MockEc::curve([0, 48, 56, 62, 70, 78], [0, 45, 55, 70, 95, 120]);
        let ext_gpu = Ms17s3MockEc::curve([0, 43, 48, 56, 65, 72], [0, 44, 58, 72, 98, 122]);

        let mut backend = HalBackends::new(
            Ms17s3MockEc::new(PerformanceMode::Balanced, FAN_MODE_AUTO, cpu_floor, gpu_floor),
            CapabilityFlags::NONE,
        )
        .with_ec_policy(Ms17s3TestPolicy);

        // Switch to ExtremePerformance with a memorized custom profile -> curves + 0xD4=0x8D applied atomically.
        let profile = ModeFanProfile {
            custom_enabled: true,
            cpu: Some(ext_cpu),
            gpu: Some(ext_gpu),
        };
        let to_ext = backend.apply_with_fan_profile(
            &ControlChange::SetPerformanceMode {
                mode: PerformanceMode::ExtremePerformance,
            },
            &id,
            Some(profile),
            || true,
        );
        assert_eq!(to_ext.result, Ok(()));
        let ext_state = to_ext.observation.unwrap().report.state;
        assert_eq!(ext_state.performance_mode, Some(PerformanceMode::ExtremePerformance));
        assert_eq!(ext_state.fan_mode, Some(FanMode::Advanced));
        assert_eq!(ext_state.cpu_fan.unwrap().curve, ext_cpu);
        assert_eq!(ext_state.gpu_fan.unwrap().curve, ext_gpu);

        // Switch back to Balanced with no custom profile -> reverts 0xD4 to Auto (0x0D).
        std::thread::sleep(Duration::from_millis(280));
        let to_bal = backend.apply_with_fan_profile(
            &ControlChange::SetPerformanceMode {
                mode: PerformanceMode::Balanced,
            },
            &id,
            None,
            || true,
        );
        assert_eq!(to_bal.result, Ok(()));
        let bal_state = to_bal.observation.unwrap().report.state;
        assert_eq!(bal_state.performance_mode, Some(PerformanceMode::Balanced));
        assert_eq!(bal_state.fan_mode, Some(FanMode::Auto));
    }
}
