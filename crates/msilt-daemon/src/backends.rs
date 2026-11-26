use msilt_core::{CapabilityFlags, ControlChange, EcMap, EcWrite, EcWriteStatus, HardwareReport};
use msilt_hal::{
    ControlOptions, Diagnostic, DiagnosticKind, ProbeOptions, WindowsHal, ms17s3, wmi_acpi,
};

use crate::{DaemonBackends, EcWritePolicy, HalBackends};

#[derive(Clone, Copy, Debug, Default)]
pub struct BackendOptions {
    pub acpi: bool,
    pub allow_platform_control: bool,
    pub enable_ec_writes: bool,
    pub noop: bool,
}

impl BackendOptions {
    pub(crate) fn create(self) -> DaemonBackends {
        if self.noop {
            return DaemonBackends::noop();
        }
        DaemonBackends::new(move || self.hal(), msilt_automation::RuleEngine::new())
    }

    pub(crate) fn hal(self) -> HalBackends {
        if self.noop {
            return HalBackends::noop();
        }
        let ec_writes = self.acpi && self.enable_ec_writes;
        let device = if ec_writes {
            WindowsHal::for_control(ControlOptions {
                acpi: true,
                ..Default::default()
            })
        } else if self.allow_platform_control {
            WindowsHal::for_platform_control(self.acpi)
        } else {
            WindowsHal::new(ProbeOptions {
                acpi: self.acpi,
                include_nvml: false,
            })
        };
        let writable = if self.allow_platform_control {
            CapabilityFlags::DISPLAY_REFRESH_RATE
                | CapabilityFlags::DISPLAY_BRIGHTNESS
                | CapabilityFlags::COLOR_PROFILE
                | CapabilityFlags::KEYBOARD_LIGHTING
        } else {
            CapabilityFlags::NONE
        };
        let backend = HalBackends::new(device, writable);
        if ec_writes {
            return backend.with_ec_policy(Ms17s3EcPolicy {
                verify_ownership: wmi_acpi::verify_ec_ownership,
            });
        }
        backend.with_ec_policy(PhaseZeroPolicy)
    }
}

struct Ms17s3EcPolicy {
    verify_ownership: fn() -> Result<(), Diagnostic>,
}

impl EcWritePolicy for Ms17s3EcPolicy {
    fn authorize(&mut self, report: &HardwareReport) -> Result<&EcMap, EcWriteStatus> {
        if !ms17s3::matches_identity(&report.state.identity) {
            return Err(EcWriteStatus::ReadOnlyFirmware);
        }
        let map = ms17s3::write_map().ok_or(EcWriteStatus::UnverifiedMapping)?;
        (self.verify_ownership)().map_err(|error| match error.kind {
            DiagnosticKind::ConflictDetected => EcWriteStatus::ConflictDetected,
            _ => EcWriteStatus::Unavailable,
        })?;
        Ok(map)
    }

    fn writable_targets(&self) -> CapabilityFlags {
        CapabilityFlags::COOLER_BOOST
            | CapabilityFlags::CHARGE_LIMIT
            | CapabilityFlags::PERFORMANCE_MODE
            | CapabilityFlags::FAN_CURVE
    }

    fn register_intent(&self, change: &ControlChange) -> Option<EcWrite> {
        match change {
            ControlChange::SetChargeLimit { percent }
                if ms17s3::CHARGE_LIMITS.contains(percent) =>
            {
                Some(EcWrite {
                    address: ms17s3::CHARGE,
                    value: ms17s3::CHARGE_BASE + percent,
                })
            }
            _ => None,
        }
    }

    fn register_plan(&self, change: &ControlChange) -> Vec<(EcWrite, u8)> {
        let entry = |address, value, mask| (EcWrite { address, value }, mask);
        match *change {
            ControlChange::SetCoolerBoost { enabled } => vec![entry(
                ms17s3::COOLER,
                if enabled { ms17s3::COOLER_MASK } else { 0 },
                ms17s3::COOLER_MASK,
            )],
            ControlChange::SetPerformanceMode { mode } => {
                let (shift, fan, battery) = ms17s3::mode_registers(mode);
                vec![
                    entry(ms17s3::FAN_MODE, fan, u8::MAX),
                    entry(ms17s3::SHIFT, shift, u8::MAX),
                    entry(ms17s3::BATTERY_MODE, battery, ms17s3::BATTERY_MODE_MASK),
                ]
            }
            _ => self
                .register_intent(change)
                .into_iter()
                .map(|write| (write, u8::MAX))
                .collect(),
        }
    }
}

struct PhaseZeroPolicy;

impl EcWritePolicy for PhaseZeroPolicy {
    fn authorize(&mut self, _report: &HardwareReport) -> Result<&EcMap, EcWriteStatus> {
        // The checked-in envelope has no verified writes. Keep it deny-only until
        // identity, mapping verification and exclusive OEM ownership are reviewed.
        Err(EcWriteStatus::UnverifiedMapping)
    }

    fn writable_targets(&self) -> CapabilityFlags {
        CapabilityFlags::NONE
    }

    fn register_intent(&self, _change: &msilt_core::ControlChange) -> Option<EcWrite> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phase_zero_mapping_never_grants_a_write() {
        let mut policy = PhaseZeroPolicy;
        assert!(matches!(
            policy.authorize(&HardwareReport::default()),
            Err(EcWriteStatus::UnverifiedMapping)
        ));
        assert_eq!(policy.writable_targets(), CapabilityFlags::NONE);
        assert!(
            policy
                .register_intent(&msilt_core::ControlChange::SetCoolerBoost { enabled: true })
                .is_none()
        );
    }

    #[test]
    fn opt_in_policy_requires_the_full_firmware_and_proven_ownership() {
        use msilt_core::{
            EcRegisterIo, EcWriteIntent, HardwareIdentity, WriteGuard, WriteGuardError,
        };
        let mut report = HardwareReport::default();
        report.state.identity = HardwareIdentity {
            board_id: Some("MS-17S3".into()),
            ec_firmware: Some("17S3EMS1.1070".into()),
        };
        let mut policy = Ms17s3EcPolicy {
            verify_ownership: || panic!("wrong firmware must be refused before querying ownership"),
        };
        assert!(matches!(
            policy.authorize(&report),
            Err(EcWriteStatus::ReadOnlyFirmware)
        ));
        report.state.identity.ec_firmware = Some("17S3EMS1.107".into());
        policy.verify_ownership = || {
            Err(Diagnostic {
                operation: "mock ownership",
                kind: DiagnosticKind::ConflictDetected,
                native_status: None,
                detail: None,
            })
        };
        assert!(matches!(
            policy.authorize(&report),
            Err(EcWriteStatus::ConflictDetected)
        ));
        policy.verify_ownership = || Ok(());
        let map = policy.authorize(&report).unwrap();
        let mut device = msilt_hal::mock::MockHal::default();
        let mut guard = WriteGuard::new(map, &report.state.identity);
        for address in [0x50, 0x51, 0xD1] {
            assert!(
                matches!(guard.execute(&mut device, EcWriteIntent::Register(EcWrite { address, value: 0 })),
                Err(WriteGuardError::DeniedWrite { address: blocked }) if blocked == address)
            );
        }
        assert!(device.operations().is_empty());
        assert_eq!(
            device.read_byte(0x98),
            Err(msilt_core::HalError::Unavailable)
        );
    }
}
