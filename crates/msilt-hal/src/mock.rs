use std::collections::VecDeque;

use msilt_core::{
    AuthorizedEcWrite, ControlChange, ControlError, EcRegisterIo, HalError, HalResult,
    HardwareRead, HardwareReport, KEYBOARD_ZONE_COUNT, PlatformControl, Rgb8, TelemetrySample,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MockRegisterOp {
    Read(u8),
    Write { address: u8, value: u8 },
}

#[derive(Debug, Clone, Copy, Default)]
pub enum MockWrite {
    #[default]
    Apply,
    AckWithoutApplying,
    FailBefore(HalError),
    FailAfter(HalError),
}

/// In-memory backend. Only `AuthorizedEcWrite` can write the simulated registers through the trait.
pub struct MockHal {
    pub report: HardwareReport,
    pub telemetry: TelemetrySample,
    registers: [Option<u8>; 256],
    operations: Vec<MockRegisterOp>,
    reads: VecDeque<HalResult<()>>,
    writes: VecDeque<MockWrite>,
    inspections: VecDeque<HalResult<()>>,
    samples: VecDeque<HalResult<()>>,
    controls: VecDeque<HalResult<()>>,
}

impl Default for MockHal {
    fn default() -> Self {
        Self {
            report: HardwareReport::default(),
            telemetry: TelemetrySample::default(),
            registers: [None; 256],
            operations: Vec::new(),
            reads: VecDeque::new(),
            writes: VecDeque::new(),
            inspections: VecDeque::new(),
            samples: VecDeque::new(),
            controls: VecDeque::new(),
        }
    }
}

impl MockHal {
    pub fn with_register(mut self, address: u8, value: u8) -> Self {
        self.registers[usize::from(address)] = Some(value);
        self
    }

    pub fn register(&self, address: u8) -> Option<u8> {
        self.registers[usize::from(address)]
    }
    pub fn operations(&self) -> &[MockRegisterOp] {
        &self.operations
    }
    pub fn script_read(&mut self, outcome: HalResult<()>) {
        self.reads.push_back(outcome);
    }
    pub fn script_write(&mut self, outcome: MockWrite) {
        self.writes.push_back(outcome);
    }
    pub fn script_inspect(&mut self, outcome: HalResult<()>) {
        self.inspections.push_back(outcome);
    }
    pub fn script_sample(&mut self, outcome: HalResult<()>) {
        self.samples.push_back(outcome);
    }
    pub fn script_control(&mut self, outcome: HalResult<()>) {
        self.controls.push_back(outcome);
    }

    fn permit(&mut self, change: &ControlChange) -> HalResult<()> {
        if !self.report.controls_detected.contains(change.capability()) {
            return Err(HalError::Unsupported);
        }
        change
            .validate_against(&self.report.state)
            .map_err(|e| match e {
                ControlError::UnsupportedValue(_) => HalError::Unsupported,
                _ => HalError::Io,
            })?;
        self.controls.pop_front().unwrap_or(Ok(()))
    }
}

impl HardwareRead for MockHal {
    fn inspect(&mut self) -> HalResult<HardwareReport> {
        self.inspections.pop_front().unwrap_or(Ok(()))?;
        Ok(self.report.clone())
    }

    fn sample_telemetry(&mut self) -> HalResult<TelemetrySample> {
        self.samples.pop_front().unwrap_or(Ok(()))?;
        Ok(self.telemetry)
    }
}

impl EcRegisterIo for MockHal {
    fn read_byte(&mut self, address: u8) -> HalResult<u8> {
        self.operations.push(MockRegisterOp::Read(address));
        self.reads.pop_front().unwrap_or(Ok(()))?;
        self.register(address).ok_or(HalError::Unavailable)
    }

    fn write_byte(&mut self, token: &AuthorizedEcWrite) -> HalResult<()> {
        let (address, value) = (token.address(), token.value());
        self.operations
            .push(MockRegisterOp::Write { address, value });
        match self.writes.pop_front().unwrap_or_default() {
            MockWrite::Apply => {
                self.registers[usize::from(address)] = Some(value);
                Ok(())
            }
            MockWrite::AckWithoutApplying => Ok(()),
            MockWrite::FailBefore(error) => Err(error),
            MockWrite::FailAfter(error) => {
                self.registers[usize::from(address)] = Some(value);
                Err(error)
            }
        }
    }
}

impl PlatformControl for MockHal {
    fn set_display_refresh_rate(&mut self, hz: u16) -> HalResult<u16> {
        self.permit(&ControlChange::SetDisplayRefreshRate { hz })?;
        self.report.state.display_refresh_hz = Some(hz);
        Ok(hz)
    }

    fn set_display_brightness(&mut self, percent: u8) -> HalResult<u8> {
        self.permit(&ControlChange::SetDisplayBrightness { percent })?;
        self.report.state.display_brightness_percent = Some(percent);
        Ok(percent)
    }

    fn set_color_profile(&mut self, profile_id: &str) -> HalResult<()> {
        self.permit(&ControlChange::SetColorProfile {
            profile_id: profile_id.into(),
        })?;
        self.report.state.color_profile_id = Some(profile_id.into());
        Ok(())
    }

    fn set_keyboard_lighting(&mut self, zones: &[Rgb8; KEYBOARD_ZONE_COUNT]) -> HalResult<()> {
        self.permit(&ControlChange::SetKeyboardLighting { zones: *zones })?;
        self.report.state.keyboard_zones = Some(*zones);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use msilt_core::{
        CapabilityFlags, EcMap, EcWrite, EcWriteIntent, HardwareIdentity, WriteGuard,
        WriteGuardError,
    };

    use super::*;

    fn fixture() -> (EcMap, HardwareIdentity, MockHal) {
        let map = EcMap::parse_json(br#"{"schemaVersion":1,"boardId":"MS-17S3","firmwarePrefixes":["17S3EMS1.107"],"writeAllowlist":[{"address":"0x20","values":[1,2]},{"address":"0x50","values":[1,2]}]}"#).unwrap();
        let identity = HardwareIdentity {
            board_id: Some("MS-17S3".into()),
            ec_firmware: Some("17S3EMS1.107".into()),
        };
        (map, identity, MockHal::default().with_register(0x20, 1))
    }

    fn intent(address: u8) -> EcWriteIntent<'static> {
        EcWriteIntent::Register(EcWrite { address, value: 2 })
    }

    #[test]
    fn guard_blocks_builtin_denies_unknown_firmware_and_phase0_writes_before_io() {
        let (map, mut id, mut mock) = fixture();
        assert!(
            WriteGuard::new(&map, &id)
                .execute(&mut mock, intent(0x50))
                .is_err()
        );
        assert!(
            WriteGuard::new(&map, &id)
                .execute(&mut mock, intent(0x21))
                .is_err()
        );
        let read_only = crate::ec_map_loader::load_ms_17s3().unwrap();
        assert!(
            WriteGuard::new(&read_only.guard_map, &id)
                .execute(&mut mock, intent(0x20))
                .is_err()
        );
        id.ec_firmware = Some("17S3EMS1.200".into());
        assert!(
            WriteGuard::new(&map, &id)
                .execute(&mut mock, intent(0x20))
                .is_err()
        );
        assert!(mock.operations().is_empty());
    }

    #[test]
    fn timeout_after_apply_is_indeterminate_and_never_rolls_back() {
        let (map, id, mut mock) = fixture();
        let mut guard = WriteGuard::new(&map, &id);
        mock.script_write(MockWrite::FailAfter(HalError::TimedOut));
        assert!(matches!(
            guard.execute(&mut mock, intent(0x20)),
            Err(WriteGuardError::Indeterminate { .. })
        ));
        assert!(guard.recovery_required());
        assert_eq!(mock.register(0x20), Some(2));
        assert_eq!(
            mock.operations(),
            &[
                MockRegisterOp::Read(0x20),
                MockRegisterOp::Write {
                    address: 0x20,
                    value: 2
                }
            ]
        );
    }

    #[test]
    fn acknowledged_mismatch_rolls_back_and_fresh_read_is_required() {
        let (map, id, mut mock) = fixture();
        let mut guard = WriteGuard::new(&map, &id);
        mock.script_write(MockWrite::AckWithoutApplying);
        assert!(matches!(
            guard.execute(&mut mock, intent(0x20)),
            Err(WriteGuardError::ApplyFailed { .. })
        ));
        assert_eq!(mock.register(0x20), Some(1));
        assert!(!guard.recovery_required());
        assert!(guard.execute(&mut mock, intent(0x20)).is_ok());
        assert_eq!(mock.read_byte(0x20), Ok(2));
    }

    #[test]
    fn all_mock_controls_and_read_errors_preserve_observed_state() {
        let mut mock = MockHal::default();
        mock.report.controls_detected = CapabilityFlags::DISPLAY_REFRESH_RATE
            | CapabilityFlags::DISPLAY_BRIGHTNESS
            | CapabilityFlags::COLOR_PROFILE
            | CapabilityFlags::KEYBOARD_LIGHTING;
        mock.report.state.supported_refresh_rates = vec![60, 240];
        mock.report.state.supported_color_profiles = vec!["TrueColor_native".into()];
        mock.script_control(Err(HalError::Busy));
        assert_eq!(mock.set_display_refresh_rate(240), Err(HalError::Busy));
        assert_eq!(mock.inspect().unwrap().state.display_refresh_hz, None);
        assert_eq!(
            mock.set_display_refresh_rate(144),
            Err(HalError::Unsupported)
        );
        assert_eq!(mock.set_display_refresh_rate(240), Ok(240));
        assert_eq!(mock.set_display_brightness(38), Ok(38));
        assert_eq!(mock.set_color_profile("../path"), Err(HalError::Io));
        assert_eq!(mock.set_color_profile("TrueColor_native"), Ok(()));
        let zones = [Rgb8 { r: 1, g: 2, b: 3 }; KEYBOARD_ZONE_COUNT];
        assert_eq!(mock.set_keyboard_lighting(&zones), Ok(()));
        assert_eq!(mock.inspect().unwrap().state.keyboard_zones, Some(zones));
        mock.script_sample(Err(HalError::TimedOut));
        mock.telemetry.cpu_fan_rpm = Some(0);
        assert_eq!(mock.sample_telemetry(), Err(HalError::TimedOut));
        assert_eq!(mock.sample_telemetry().unwrap().cpu_fan_rpm, Some(0));
    }
}
