use serde_json::{Value, json};

use super::*;
use crate::ErrorCode;

const REGISTER: u8 = 0x20;
const FAN: [u8; FAN_POINT_COUNT] = [0x70, 0x71, 0x72, 0x73, 0x74, 0x75];
const TEMPS: [u8; FAN_POINT_COUNT] = [35, 45, 55, 65, 75, 85];
const FLOOR: [u8; FAN_POINT_COUNT] = [0, 20, 30, 45, 60, 80];
const BASELINE: [u8; FAN_POINT_COUNT] = [20, 35, 45, 50, 70, 90];
const TARGET: [u8; FAN_POINT_COUNT] = [25, 32, 48, 58, 65, 96];

fn fixture() -> Value {
    let points: [Value; FAN_POINT_COUNT] = std::array::from_fn(|index| {
        json!({
            "temperatureC": TEMPS[index],
            "minimumDutyPercent": FLOOR[index],
            "dutyRegister": FAN[index],
        })
    });
    json!({
        "schemaVersion": 1,
        "boardId": "MS-17S3",
        "firmwarePrefixes": ["17S3EMS1.1"],
        "denyWrite": ["0x60", 97],
        "writeAllowlist": [
            {"address": "0x20", "values": [1, 2, 3]},
            {"address": "0x50", "values": [1]},
            {"address": 81, "values": [1]},
            {"address": "0x60", "values": [1]},
        ],
        "fanCurves": {"cpu": {"points": points}},
    })
}

fn parse(document: Value) -> Result<EcMap, EcMapError> {
    EcMap::parse_json(&serde_json::to_vec(&document).unwrap())
}

fn identity() -> HardwareIdentity {
    HardwareIdentity {
        board_id: Some("MS-17S3".into()),
        ec_firmware: Some("17S3EMS1.107".into()),
    }
}

fn curve(duty: [u8; FAN_POINT_COUNT]) -> FanCurve {
    FanCurve {
        points: std::array::from_fn(|index| FanPoint {
            temperature_c: TEMPS[index],
            duty_percent: duty[index],
        }),
    }
}

fn register(address: u8, value: u8) -> EcWriteIntent<'static> {
    EcWriteIntent::Register(EcWrite { address, value })
}

fn cpu(curve: &FanCurve) -> EcWriteIntent<'_> {
    EcWriteIntent::FanCurve { channel: FanChannel::Cpu, curve }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Read(u8),
    Write(u8, u8),
}

#[derive(Debug, Clone, Copy)]
enum Fault {
    Error(HalError),
    IgnoreWrite,
    WriteThenError(HalError),
    AlsoWrite { address: u8, value: u8 },
}

struct MockEc {
    bytes: [u8; 256],
    ops: Vec<Op>,
    faults: Vec<(usize, Fault)>,
}

impl MockEc {
    fn new() -> Self {
        let mut bytes = [0; 256];
        bytes[usize::from(REGISTER)] = 1;
        for (address, value) in FAN.into_iter().zip(BASELINE) {
            bytes[usize::from(address)] = value;
        }
        Self { bytes, ops: Vec::new(), faults: Vec::new() }
    }

    fn record(&mut self, op: Op) -> Option<Fault> {
        let next = self.ops.len();
        self.ops.push(op);
        self.faults.iter().find(|(index, _)| *index == next).map(|(_, fault)| *fault)
    }

    fn duties(&self) -> [u8; FAN_POINT_COUNT] {
        FAN.map(|address| self.bytes[usize::from(address)])
    }

    fn writes(&self) -> Vec<Op> {
        self.ops.iter().copied().filter(|op| matches!(op, Op::Write(..))).collect()
    }
}

impl EcRegisterIo for MockEc {
    fn read_byte(&mut self, address: u8) -> Result<u8, HalError> {
        match self.record(Op::Read(address)) {
            Some(Fault::Error(error)) => Err(error),
            None => Ok(self.bytes[usize::from(address)]),
            fault => panic!("unexpected read fault: {fault:?}"),
        }
    }

    fn write_byte(&mut self, write: &AuthorizedEcWrite) -> Result<(), HalError> {
        let (address, value) = (write.address(), write.value());
        let fault = self.record(Op::Write(address, value));
        if let Some(Fault::Error(error)) = fault {
            return Err(error);
        }
        if matches!(fault, Some(Fault::IgnoreWrite)) {
            return Ok(());
        }
        self.bytes[usize::from(address)] = value;
        match fault {
            Some(Fault::WriteThenError(error)) => Err(error),
            Some(Fault::AlsoWrite { address, value }) => {
                self.bytes[usize::from(address)] = value;
                Ok(())
            }
            _ => Ok(()),
        }
    }
}

#[test]
fn read_only_map_parses_without_inventing_writable_addresses() {
    let map = EcMap::parse_json(br#"{"schemaVersion":1,"boardId":"MS-17S3","firmwarePrefixes":["17S3EMS1.107"]}"#).unwrap();
    let mut io = MockEc::new();
    assert!(map.denies_write(0x50));
    assert_eq!(map.fan_safety_floor(FanChannel::Cpu), None);
    assert_eq!(
        WriteGuard::new(&map, &identity()).execute(&mut io, register(REGISTER, 2)),
        Err(WriteGuardError::WriteNotAllowed { address: REGISTER }),
    );
    assert!(io.ops.is_empty());
}

#[test]
fn map_rejects_unsafe_schema_and_ambiguous_bindings() {
    let mut document = fixture();
    document["schemaVersion"] = json!(2);
    assert!(matches!(parse(document), Err(EcMapError::UnsupportedSchema(2))));

    for prefix in ["", "17", "17S3EMS1.", "17S3EMS1.*", "17S3EMS1.1x"] {
        let mut document = fixture();
        document["firmwarePrefixes"] = json!([prefix]);
        assert!(matches!(parse(document), Err(EcMapError::InvalidField("firmwarePrefixes"))));
    }

    for address in [json!(256), json!(-1), json!("0x100"), json!("0x+1")] {
        let mut document = fixture();
        document["denyWrite"] = json!([address]);
        assert!(matches!(parse(document), Err(EcMapError::Json(_))));
    }

    let mut document = fixture();
    document["writeAllowlist"][0]["values"] = json!([2, 2]);
    assert!(matches!(parse(document), Err(EcMapError::InvalidField("writeAllowlist.values"))));

    let mut document = fixture();
    document["writeAllowlist"][0]["address"] = json!(FAN[0]);
    assert!(matches!(parse(document), Err(EcMapError::DuplicateAddress(address)) if address == FAN[0]));

    let mut document = fixture();
    document["fanCurves"]["gpu"] = document["fanCurves"]["cpu"].clone();
    assert!(matches!(parse(document), Err(EcMapError::DuplicateAddress(address)) if address == FAN[0]));

    assert!(matches!(EcMap::parse_json(&vec![b' '; MAX_EC_MAP_BYTES + 1]), Err(EcMapError::TooLarge)));
}

#[test]
fn map_requires_a_valid_nonzero_hot_end_floor() {
    let mut document = fixture();
    for point in document["fanCurves"]["cpu"]["points"].as_array_mut().unwrap() {
        point["minimumDutyPercent"] = json!(0);
    }
    assert!(matches!(parse(document), Err(EcMapError::InvalidSafetyFloor(FanCurveError::MissingSafetyFloor))));
}

#[test]
fn unknown_board_or_firmware_refuses_before_io() {
    let map = parse(fixture()).unwrap();
    assert!(map.matches(&identity()));
    let mut io = MockEc::new();
    for firmware in [None, Some("17S3EMS1.207"), Some("17S3EMS10.107"), Some("17S3EMS1.107X")] {
        let device = HardwareIdentity { ec_firmware: firmware.map(str::to_owned), ..identity() };
        let error = WriteGuard::new(&map, &device).execute(&mut io, register(REGISTER, 2)).unwrap_err();
        assert_eq!(error, WriteGuardError::ReadOnlyFirmware);
        assert_eq!(ErrorCode::from(&error), ErrorCode::ReadOnlyFirmware);
    }
    let device = HardwareIdentity { board_id: Some("MS-FFFF".into()), ..identity() };
    assert_eq!(
        WriteGuard::new(&map, &device).execute(&mut io, register(REGISTER, 2)),
        Err(WriteGuardError::ReadOnlyFirmware),
    );
    assert!(io.ops.is_empty());
}

#[test]
fn denylist_overrides_map_and_blocks_whole_fan_transaction() {
    let mut document = fixture();
    document["fanCurves"]["cpu"]["points"][5]["dutyRegister"] = json!("0x61");
    let map = parse(document).unwrap();
    let mut io = MockEc::new();
    let mut guard = WriteGuard::new(&map, &identity());
    for address in [0x50, 0x51, 0x60, 0x61] {
        assert_eq!(guard.execute(&mut io, register(address, 1)), Err(WriteGuardError::DeniedWrite { address }));
    }
    assert_eq!(guard.execute(&mut io, cpu(&curve(TARGET))), Err(WriteGuardError::DeniedWrite { address: 0x61 }));
    assert!(io.ops.is_empty());
}

#[test]
fn no_unmapped_values_or_fan_register_bypass() {
    let map = parse(fixture()).unwrap();
    let mut io = MockEc::new();
    let mut guard = WriteGuard::new(&map, &identity());
    for (address, value) in [(REGISTER, 4), (0x90, 1), (FAN[5], 0)] {
        assert_eq!(guard.execute(&mut io, register(address, value)), Err(WriteGuardError::WriteNotAllowed { address }));
    }
    assert_eq!(
        guard.execute(&mut io, EcWriteIntent::FanCurve { channel: FanChannel::Gpu, curve: &curve(TARGET) }),
        Err(WriteGuardError::UnsupportedFan { channel: FanChannel::Gpu }),
    );
    assert!(io.ops.is_empty());
}

#[test]
fn rejects_curve_shape_unverified_knots_and_hot_floor_before_io() {
    let map = parse(fixture()).unwrap();
    let mut io = MockEc::new();
    let mut guard = WriteGuard::new(&map, &identity());
    let mut requested = curve(TARGET);
    requested.points[5].duty_percent = 79;
    assert_eq!(
        guard.execute(&mut io, cpu(&requested)),
        Err(WriteGuardError::InvalidFanCurve(FanCurveError::BelowSafetyFloor { index: 5, minimum: 80 })),
    );
    requested.points[5].duty_percent = 151;
    assert_eq!(requested.validate_shape(), Err(FanCurveError::OutOfRange { index: 5 }));
    requested.points[5].duty_percent = 64;
    assert_eq!(requested.validate_shape(), Err(FanCurveError::DutyOrder { index: 5 }));
    requested = curve(TARGET);
    requested.points[5].temperature_c = 101;
    assert_eq!(requested.validate_shape(), Err(FanCurveError::OutOfRange { index: 5 }));
    requested.points[5].temperature_c = TEMPS[4];
    assert_eq!(requested.validate_shape(), Err(FanCurveError::TemperatureOrder { index: 5 }));
    requested.points[5].temperature_c = TEMPS[5] - 1;
    assert_eq!(
        guard.execute(&mut io, cpu(&requested)),
        Err(WriteGuardError::InvalidFanCurve(FanCurveError::UnsupportedTemperature { index: 5 })),
    );
    assert!(io.ops.is_empty());
}

#[test]
fn only_a_verified_layout_can_raise_the_ceiling_and_lock_the_anchor() {
    let map = parse(fixture()).unwrap();
    let mut io = MockEc::new();
    let mut target = curve([20, 35, 45, 50, 70, 120]);
    assert_eq!(
        WriteGuard::new(&map, &identity()).execute(&mut io, cpu(&target)),
        Err(WriteGuardError::InvalidFanCurve(FanCurveError::OutOfRange { index: 5 })),
    );
    assert!(io.ops.is_empty());
    let mut document = fixture();
    document["fanCurves"]["cpu"]["maxDutyPercent"] = json!(150);
    document["fanCurves"]["cpu"]["fixedAnchor"] = json!(true);
    document["fanCurves"]["cpu"]["points"][0]["minimumDutyPercent"] = json!(BASELINE[0]);
    let map = parse(document).unwrap();
    let mut guard = WriteGuard::new(&map, &identity());
    target.points[0].duty_percent = 21;
    assert_eq!(guard.execute(&mut io, cpu(&target)), Err(WriteGuardError::InvalidFanCurve(FanCurveError::FixedAnchor)));
    assert!(io.ops.is_empty());
    target.points[0].duty_percent = BASELINE[0];
    guard.execute(&mut io, cpu(&target)).unwrap();
    assert_eq!(io.writes(), [Op::Write(FAN[5], 120)]);
}

#[test]
fn prepare_captures_snapshot_and_reverse_instructions_without_writing() {
    let map = parse(fixture()).unwrap();
    let mut io = MockEc::new();
    let mut guard = WriteGuard::new(&map, &identity());
    let prepared = guard.prepare(&mut io, register(REGISTER, 2)).unwrap();
    assert_eq!(prepared.snapshot(), &[RegisterChange { address: REGISTER, before: 1, after: 2 }]);
    assert_eq!(prepared.rollback_plan().collect::<Vec<_>>(), [EcWrite { address: REGISTER, value: 1 }]);
    drop(prepared);
    assert_eq!(io.ops, [Op::Read(REGISTER)]);

    let receipt = guard.execute(&mut io, register(REGISTER, 1)).unwrap();
    assert_eq!(receipt.rollback_plan().next(), None);
    assert!(io.writes().is_empty());
}

#[test]
fn successful_fan_apply_is_safe_at_every_intermediate_curve() {
    let map = parse(fixture()).unwrap();
    let mut io = MockEc::new();
    let mut guard = WriteGuard::new(&map, &identity());
    let receipt = guard.execute(&mut io, cpu(&curve(TARGET))).unwrap();
    let mut simulated = BASELINE;
    let writes = io.writes();
    assert_eq!(writes, [5, 3, 2, 0, 1, 4].map(|index| Op::Write(FAN[index], TARGET[index])));
    for op in writes {
        let Op::Write(address, value) = op else { unreachable!() };
        simulated[FAN.iter().position(|&item| item == address).unwrap()] = value;
        curve(simulated).validate_against(&curve(FLOOR)).unwrap();
    }
    assert_eq!(io.duties(), TARGET);
    assert_eq!(receipt.snapshot().len(), FAN_POINT_COUNT);
    assert_eq!(receipt.rollback_plan().collect::<Vec<_>>(), [4, 1, 0, 2, 3, 5].map(|index| EcWrite { address: FAN[index], value: BASELINE[index] }));
}

#[test]
fn unsafe_snapshot_latches_without_writing() {
    let map = parse(fixture()).unwrap();
    let mut io = MockEc::new();
    let mut guard = WriteGuard::new(&map, &identity());
    io.bytes[usize::from(FAN[5])] = 79;
    assert_eq!(
        guard.execute(&mut io, cpu(&curve(TARGET))),
        Err(WriteGuardError::UnsafeFanBaseline(FanCurveError::BelowSafetyFloor { index: 5, minimum: 80 })),
    );
    assert!(guard.recovery_required());
    assert!(io.writes().is_empty());
    let mut other_guard = WriteGuard::new(&map, &identity());
    io.bytes[usize::from(REGISTER)] = 0;
    assert_eq!(
        other_guard.execute(&mut io, register(REGISTER, 2)),
        Err(WriteGuardError::UnsafeRegisterBaseline { address: REGISTER }),
    );
    assert!(other_guard.recovery_required());
    assert!(io.writes().is_empty());
}

#[test]
fn snapshot_read_failure_never_enters_apply() {
    let map = parse(fixture()).unwrap();
    let mut io = MockEc::new();
    let mut guard = WriteGuard::new(&map, &identity());
    io.faults = vec![(3, Fault::Error(HalError::Io))];
    assert_eq!(
        guard.execute(&mut io, cpu(&curve(TARGET))),
        Err(WriteGuardError::SnapshotFailed { address: FAN[3], source: HalError::Io }),
    );
    assert!(!guard.recovery_required());
    assert_eq!(io.duties(), BASELINE);
    assert!(io.writes().is_empty());
}

#[test]
fn readback_mismatch_restores_original_value() {
    let map = parse(fixture()).unwrap();
    let mut io = MockEc::new();
    let mut guard = WriteGuard::new(&map, &identity());
    io.faults = vec![(1, Fault::IgnoreWrite)];
    let error = guard.execute(&mut io, register(REGISTER, 2)).unwrap_err();
    assert_eq!(error, WriteGuardError::ApplyFailed {
        failure: TransactionFailure::Verification { address: REGISTER, expected: 2, actual: 1 },
    });
    assert_eq!(ErrorCode::from(&error), ErrorCode::VerificationFailed);
    assert_eq!(io.ops, [Op::Read(REGISTER), Op::Write(REGISTER, 2), Op::Read(REGISTER), Op::Write(REGISTER, 1), Op::Read(REGISTER)]);
    assert!(!guard.recovery_required());
}

#[test]
fn rolls_back_attempted_fan_writes_even_if_failed_write_had_side_effects() {
    let map = parse(fixture()).unwrap();
    let mut io = MockEc::new();
    let mut guard = WriteGuard::new(&map, &identity());
    io.faults = vec![(10, Fault::WriteThenError(HalError::Io))];
    assert_eq!(
        guard.execute(&mut io, cpu(&curve(TARGET))),
        Err(WriteGuardError::ApplyFailed { failure: TransactionFailure::Write { address: FAN[2], source: HalError::Io } }),
    );
    assert_eq!(io.writes(), [
        Op::Write(FAN[5], TARGET[5]), Op::Write(FAN[3], TARGET[3]), Op::Write(FAN[2], TARGET[2]),
        Op::Write(FAN[2], BASELINE[2]), Op::Write(FAN[3], BASELINE[3]), Op::Write(FAN[5], BASELINE[5]),
    ]);
    assert_eq!(io.duties(), BASELINE);
    assert!(!guard.recovery_required());
}

#[test]
fn final_verification_detects_collateral_changes_and_latches() {
    let map = parse(fixture()).unwrap();
    let mut io = MockEc::new();
    let mut guard = WriteGuard::new(&map, &identity());
    let mut target = TARGET;
    target[4] = BASELINE[4];
    io.faults = vec![(6, Fault::AlsoWrite { address: FAN[4], value: 71 })];
    assert_eq!(
        guard.execute(&mut io, cpu(&curve(target))),
        Err(WriteGuardError::RollbackFailed {
            failure: TransactionFailure::Verification { address: FAN[4], expected: 70, actual: 71 },
            rollback: TransactionFailure::Verification { address: FAN[4], expected: 70, actual: 71 },
        }),
    );
    assert!(guard.recovery_required());
    assert!(!io.writes().iter().any(|op| matches!(op, Op::Write(address, _) if *address == FAN[4])));
}

#[test]
fn failed_rollback_blocks_the_next_transaction() {
    let map = parse(fixture()).unwrap();
    let mut io = MockEc::new();
    let mut guard = WriteGuard::new(&map, &identity());
    io.faults = vec![(1, Fault::IgnoreWrite), (3, Fault::Error(HalError::Io))];
    let error = guard.execute(&mut io, register(REGISTER, 2)).unwrap_err();
    assert!(matches!(error, WriteGuardError::RollbackFailed { .. }));
    assert_eq!(ErrorCode::from(&error), ErrorCode::RollbackFailed);
    assert!(guard.recovery_required());
    assert_eq!(guard.execute(&mut io, register(REGISTER, 3)), Err(WriteGuardError::RecoveryRequired));
    assert_eq!(io.ops.len(), 4);
}

#[test]
fn no_rollback_or_more_io_on_indeterminate_timeout() {
    let map = parse(fixture()).unwrap();
    for (index, fault) in [
        (0, Fault::Error(HalError::TimedOut)),
        (1, Fault::WriteThenError(HalError::TimedOut)),
        (2, Fault::Error(HalError::TimedOut)),
    ] {
        let mut io = MockEc::new();
        let mut guard = WriteGuard::new(&map, &identity());
        io.faults = vec![(index, fault)];
        let error = guard.execute(&mut io, register(REGISTER, 2)).unwrap_err();
        assert_eq!(ErrorCode::from(&error), ErrorCode::TimedOut);
        assert!(guard.recovery_required());
        assert_eq!(guard.execute(&mut io, register(REGISTER, 3)), Err(WriteGuardError::RecoveryRequired));
        assert_eq!(io.ops.len(), index + 1);
    }
}
