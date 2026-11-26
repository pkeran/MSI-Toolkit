use serde::{Deserialize, Deserializer};
use thiserror::Error;

use crate::{DEFAULT_MAX_FAN_DUTY_PERCENT, EcRegisterIo, FAN_POINT_COUNT, FanChannel, FanCurve, FanCurveError, FanCurveLimits, FanPoint, HalError, HardwareIdentity};

pub const EC_MAP_SCHEMA_VERSION: u16 = 1;
pub const MAX_EC_MAP_BYTES: usize = 64 * 1024;
pub const BUILTIN_DENY_WRITE: [u8; 2] = [0x50, 0x51];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
enum AddressRepr<'a> {
    Number(u8),
    Hex(#[serde(borrow)] &'a str),
}

fn address_from_json<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u8, D::Error> {
    match AddressRepr::deserialize(deserializer)? {
        AddressRepr::Number(address) => Ok(address),
        AddressRepr::Hex(value) => parse_hex_address(value).ok_or_else(|| {
            serde::de::Error::custom("expected an EC byte address in 0xHH notation")
        }),
    }
}

fn parse_hex_address(value: &str) -> Option<u8> {
    let hex = value.strip_prefix("0x")?;
    if hex.len() != 2 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    u8::from_str_radix(hex, 16).ok()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
struct Address(#[serde(deserialize_with = "address_from_json")] u8);

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RegisterRule {
    address: Address,
    values: Vec<u8>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct FanRegister {
    temperature_c: u8,
    minimum_duty_percent: u8,
    duty_register: Address,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct FanLayout {
    points: [FanRegister; FAN_POINT_COUNT],
    #[serde(default = "default_max_duty_percent")]
    max_duty_percent: u8,
    #[serde(default)]
    fixed_anchor: bool,
}

fn default_max_duty_percent() -> u8 {
    DEFAULT_MAX_FAN_DUTY_PERCENT
}

impl FanLayout {
    fn limits(&self) -> FanCurveLimits {
        FanCurveLimits {
            floor: FanCurve {
                points: self.points.map(|point| FanPoint {
                    temperature_c: point.temperature_c,
                    duty_percent: point.minimum_duty_percent,
                }),
            },
            max_duty_percent: self.max_duty_percent,
            fixed_anchor: self.fixed_anchor,
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FanLayouts {
    cpu: Option<FanLayout>,
    gpu: Option<FanLayout>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct MapDocument {
    schema_version: u16,
    board_id: String,
    firmware_prefixes: Vec<String>,
    #[serde(default)]
    deny_write: Vec<Address>,
    #[serde(default)]
    write_allowlist: Vec<RegisterRule>,
    #[serde(default)]
    fan_curves: FanLayouts,
}

#[derive(Debug)]
pub struct EcMap {
    board_id: String,
    firmware_prefixes: Vec<String>,
    deny_write: [bool; 256],
    write_allowlist: Vec<RegisterRule>,
    fan_curves: FanLayouts,
}

impl EcMap {
    pub fn parse_json(bytes: &[u8]) -> Result<Self, EcMapError> {
        if bytes.len() > MAX_EC_MAP_BYTES {
            return Err(EcMapError::TooLarge);
        }
        let document: MapDocument = serde_json::from_slice(bytes)?;
        if document.schema_version != EC_MAP_SCHEMA_VERSION {
            return Err(EcMapError::UnsupportedSchema(document.schema_version));
        }
        if document.board_id.is_empty()
            || document.board_id.len() > 32
            || !document.board_id.bytes().all(|byte| {
                byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'-'
            })
        {
            return Err(EcMapError::InvalidField("boardId"));
        }
        if document.firmware_prefixes.is_empty()
            || document.firmware_prefixes.len() > 16
            || document.firmware_prefixes.iter().any(|prefix| firmware_parts(prefix).is_none())
        {
            return Err(EcMapError::InvalidField("firmwarePrefixes"));
        }

        let mut deny_write = [false; 256];
        for address in BUILTIN_DENY_WRITE.into_iter().chain(document.deny_write.iter().map(|a| a.0)) {
            deny_write[usize::from(address)] = true;
        }
        let mut addresses_seen = [false; 256];
        for rule in &document.write_allowlist {
            let mut values_seen = [false; 256];
            if rule.values.is_empty() || rule.values.iter().any(|&value| {
                std::mem::replace(&mut values_seen[usize::from(value)], true)
            }) {
                return Err(EcMapError::InvalidField("writeAllowlist.values"));
            }
            if std::mem::replace(&mut addresses_seen[usize::from(rule.address.0)], true) {
                return Err(EcMapError::DuplicateAddress(rule.address.0));
            }
        }
        for layout in [&document.fan_curves.cpu, &document.fan_curves.gpu].into_iter().flatten() {
            let limits = layout.limits();
            limits.floor.validate_with_limits(&limits).map_err(EcMapError::InvalidSafetyFloor)?;
            for point in &layout.points {
                let address = point.duty_register.0;
                if std::mem::replace(&mut addresses_seen[usize::from(address)], true) {
                    return Err(EcMapError::DuplicateAddress(address));
                }
            }
        }
        Ok(Self {
            board_id: document.board_id,
            firmware_prefixes: document.firmware_prefixes,
            deny_write,
            write_allowlist: document.write_allowlist,
            fan_curves: document.fan_curves,
        })
    }

    pub fn matches(&self, identity: &HardwareIdentity) -> bool {
        if identity.board_id.as_deref() != Some(self.board_id.as_str()) {
            return false;
        }
        let Some((family, version)) = identity.ec_firmware.as_deref().and_then(firmware_parts) else {
            return false;
        };
        self.firmware_prefixes.iter().filter_map(|p| firmware_parts(p)).any(|(f, v)| {
            family == f && version.starts_with(v)
        })
    }

    pub fn denies_write(&self, address: u8) -> bool {
        self.deny_write[usize::from(address)]
    }

    pub fn fan_curve_limits(&self, channel: FanChannel) -> Option<FanCurveLimits> {
        self.fan_layout(channel).map(FanLayout::limits)
    }

    pub fn fan_safety_floor(&self, channel: FanChannel) -> Option<FanCurve> {
        self.fan_curve_limits(channel).map(|limits| limits.floor)
    }

    fn fan_layout(&self, channel: FanChannel) -> Option<&FanLayout> {
        match channel {
            FanChannel::Cpu => self.fan_curves.cpu.as_ref(),
            FanChannel::Gpu => self.fan_curves.gpu.as_ref(),
        }
    }

    fn allows_register_value(&self, address: u8, value: u8) -> bool {
        self.write_allowlist.iter().any(|rule| {
            rule.address.0 == address && rule.values.contains(&value)
        })
    }
}

fn firmware_parts(firmware: &str) -> Option<(&str, &str)> {
    let (family, version) = firmware.split_once('.')?;
    if !(4..=16).contains(&family.len())
        || !(1..=8).contains(&version.len())
        || !family.bytes().all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit())
        || !version.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    Some((family, version))
}

#[derive(Debug, Error)]
pub enum EcMapError {
    #[error("EC map exceeds the size limit")]
    TooLarge,
    #[error("unsupported EC map schema {0}")]
    UnsupportedSchema(u16),
    #[error("invalid EC map field: {0}")]
    InvalidField(&'static str),
    #[error("EC address 0x{0:02X} has multiple write bindings")]
    DuplicateAddress(u8),
    #[error("invalid mapped fan safety floor: {0}")]
    InvalidSafetyFloor(#[source] FanCurveError),
    #[error("invalid EC map JSON: {0}")]
    Json(#[from] serde_json::Error),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EcWrite {
    pub address: u8,
    pub value: u8,
}

/// Only a guarded apply or rollback can issue this single-operation token.
///
/// ```compile_fail
/// use msilt_core::{AuthorizedEcWrite, EcWrite};
/// let bypass = AuthorizedEcWrite(EcWrite { address: 0x50, value: 1 });
/// ```
pub struct AuthorizedEcWrite(EcWrite);

impl AuthorizedEcWrite {
    pub fn address(&self) -> u8 {
        self.0.address
    }

    pub fn value(&self) -> u8 {
        self.0.value
    }
}

#[derive(Debug, Clone, Copy)]
pub enum EcWriteIntent<'a> {
    Register(EcWrite),
    FanCurve { channel: FanChannel, curve: &'a FanCurve },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RegisterChange {
    pub address: u8,
    pub before: u8,
    pub after: u8,
}

#[derive(Debug)]
pub struct WriteGuard<'map> {
    map: &'map EcMap,
    firmware_matches: bool,
    recovery_required: bool,
}

impl<'map> WriteGuard<'map> {
    pub fn new(map: &'map EcMap, identity: &HardwareIdentity) -> Self {
        Self { map, firmware_matches: map.matches(identity), recovery_required: false }
    }

    pub fn recovery_required(&self) -> bool {
        self.recovery_required
    }

    pub fn prepare<'txn, I: EcRegisterIo + ?Sized>(
        &'txn mut self,
        io: &'txn mut I,
        intent: EcWriteIntent<'_>,
    ) -> Result<PreparedWrite<'txn, I>, WriteGuardError> {
        if self.recovery_required {
            return Err(WriteGuardError::RecoveryRequired);
        }
        if !self.firmware_matches {
            return Err(WriteGuardError::ReadOnlyFirmware);
        }

        let mut changes = Vec::with_capacity(match intent {
            EcWriteIntent::Register(_) => 1,
            EcWriteIntent::FanCurve { .. } => FAN_POINT_COUNT,
        });
        match intent {
            EcWriteIntent::Register(write) => {
                check_denied(self.map, write.address)?;
                if !self.map.allows_register_value(write.address, write.value) {
                    return Err(WriteGuardError::WriteNotAllowed { address: write.address });
                }
                let before = snapshot_byte(io, write.address, &mut self.recovery_required)?;
                if !self.map.allows_register_value(write.address, before) {
                    self.recovery_required = true;
                    return Err(WriteGuardError::UnsafeRegisterBaseline { address: write.address });
                }
                changes.push(RegisterChange { address: write.address, before, after: write.value });
            }
            EcWriteIntent::FanCurve { channel, curve } => {
                let layout = self.map.fan_layout(channel)
                    .ok_or(WriteGuardError::UnsupportedFan { channel })?;
                for point in &layout.points {
                    check_denied(self.map, point.duty_register.0)?;
                }
                let limits = layout.limits();
                curve.validate_with_limits(&limits)?;
                let mut baseline = *curve;
                for (index, (point, knot)) in curve.points.iter().zip(layout.points).enumerate() {
                    let address = knot.duty_register.0;
                    let before = snapshot_byte(io, address, &mut self.recovery_required)?;
                    baseline.points[index].duty_percent = before;
                    changes.push(RegisterChange { address, before, after: point.duty_percent });
                }
                if let Err(error) = baseline.validate_with_limits(&limits) {
                    self.recovery_required = true;
                    return Err(WriteGuardError::UnsafeFanBaseline(error));
                }
                // Raise from the hot end, then lower from the cold end to preserve monotonicity.
                let mut knots: [usize; FAN_POINT_COUNT] = std::array::from_fn(|index| index);
                knots.sort_unstable_by_key(|&index| {
                    let item = changes[index];
                    if item.after > item.before {
                        FAN_POINT_COUNT - 1 - index
                    } else if item.after < item.before {
                        FAN_POINT_COUNT + index
                    } else {
                        2 * FAN_POINT_COUNT + index
                    }
                });
                let ordered = knots.map(|index| changes[index]);
                changes.copy_from_slice(&ordered);
            }
        }
        Ok(PreparedWrite { io, changes, recovery_required: &mut self.recovery_required })
    }

    pub fn execute<I: EcRegisterIo + ?Sized>(
        &mut self,
        io: &mut I,
        intent: EcWriteIntent<'_>,
    ) -> Result<WriteReceipt, WriteGuardError> {
        self.prepare(io, intent)?.commit()
    }
}

fn check_denied(map: &EcMap, address: u8) -> Result<(), WriteGuardError> {
    if map.denies_write(address) {
        return Err(WriteGuardError::DeniedWrite { address });
    }
    Ok(())
}

fn snapshot_byte<I: EcRegisterIo + ?Sized>(
    io: &mut I,
    address: u8,
    recovery_required: &mut bool,
) -> Result<u8, WriteGuardError> {
    io.read_byte(address).map_err(|source| {
        if source == HalError::TimedOut {
            *recovery_required = true;
        }
        WriteGuardError::SnapshotFailed { address, source }
    })
}

pub struct PreparedWrite<'txn, I: EcRegisterIo + ?Sized> {
    io: &'txn mut I,
    changes: Vec<RegisterChange>,
    recovery_required: &'txn mut bool,
}

impl<I: EcRegisterIo + ?Sized> PreparedWrite<'_, I> {
    pub fn snapshot(&self) -> &[RegisterChange] {
        &self.changes
    }

    pub fn rollback_plan(&self) -> impl Iterator<Item = EcWrite> + '_ {
        rollback_plan(&self.changes)
    }

    pub fn commit(self) -> Result<WriteReceipt, WriteGuardError> {
        let mut attempted = 0;
        for change in self.changes.iter().filter(|change| change.before != change.after) {
            attempted += 1;
            if let Err(failure) = write_and_verify(self.io, change.address, change.after) {
                return Err(self.handle_failure(attempted, failure));
            }
        }
        if self.changes.len() > 1 && attempted > 0 {
            for change in &self.changes {
                if let Err(failure) = verify(self.io, change.address, change.after) {
                    return Err(self.handle_failure(attempted, failure));
                }
            }
        }
        Ok(WriteReceipt { changes: self.changes })
    }

    fn handle_failure(mut self, attempted: usize, failure: TransactionFailure) -> WriteGuardError {
        if failure.is_timeout() {
            *self.recovery_required = true;
            return WriteGuardError::Indeterminate { failure };
        }
        match self.restore(attempted) {
            Ok(()) => WriteGuardError::ApplyFailed { failure },
            Err(rollback) => {
                *self.recovery_required = true;
                WriteGuardError::RollbackFailed { failure, rollback }
            }
        }
    }

    fn restore(&mut self, attempted: usize) -> Result<(), TransactionFailure> {
        for write in rollback_plan(&self.changes[..attempted]) {
            write_and_verify(self.io, write.address, write.value)?;
        }
        if self.changes.len() > 1 {
            for change in &self.changes {
                verify(self.io, change.address, change.before)?;
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteReceipt {
    changes: Vec<RegisterChange>,
}

impl WriteReceipt {
    pub fn snapshot(&self) -> &[RegisterChange] {
        &self.changes
    }

    pub fn rollback_plan(&self) -> impl Iterator<Item = EcWrite> + '_ {
        rollback_plan(&self.changes)
    }
}

fn rollback_plan(changes: &[RegisterChange]) -> impl Iterator<Item = EcWrite> + '_ {
    changes.iter().rev().filter(|change| change.before != change.after).map(|change| EcWrite {
        address: change.address,
        value: change.before,
    })
}

fn write_and_verify<I: EcRegisterIo + ?Sized>(
    io: &mut I,
    address: u8,
    value: u8,
) -> Result<(), TransactionFailure> {
    io.write_byte(&AuthorizedEcWrite(EcWrite { address, value }))
        .map_err(|source| TransactionFailure::Write { address, source })?;
    verify(io, address, value)
}

fn verify<I: EcRegisterIo + ?Sized>(
    io: &mut I,
    address: u8,
    expected: u8,
) -> Result<(), TransactionFailure> {
    let actual = io.read_byte(address).map_err(|source| TransactionFailure::Read { address, source })?;
    if actual != expected {
        return Err(TransactionFailure::Verification { address, expected, actual });
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum TransactionFailure {
    #[error("read failed at EC address 0x{address:02X}: {source}")]
    Read { address: u8, source: HalError },
    #[error("write failed at EC address 0x{address:02X}: {source}")]
    Write { address: u8, source: HalError },
    #[error("EC verification mismatch at 0x{address:02X} (expected {expected}, read {actual})")]
    Verification { address: u8, expected: u8, actual: u8 },
}

impl TransactionFailure {
    fn is_timeout(self) -> bool {
        matches!(self,
            Self::Read { source: HalError::TimedOut, .. } |
            Self::Write { source: HalError::TimedOut, .. }
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum WriteGuardError {
    #[error("the board or EC firmware is not covered by the EC map")]
    ReadOnlyFirmware,
    #[error("EC writes require recovery after an indeterminate transaction")]
    RecoveryRequired,
    #[error("EC address 0x{address:02X} is denied")]
    DeniedWrite { address: u8 },
    #[error("EC write at address 0x{address:02X} is not allowlisted")]
    WriteNotAllowed { address: u8 },
    #[error("no verified fan layout for {channel:?}")]
    UnsupportedFan { channel: FanChannel },
    #[error(transparent)]
    InvalidFanCurve(#[from] FanCurveError),
    #[error("EC baseline is not an allowlisted value at 0x{address:02X}")]
    UnsafeRegisterBaseline { address: u8 },
    #[error("EC fan baseline is not safe to restore: {0}")]
    UnsafeFanBaseline(#[source] FanCurveError),
    #[error("could not take the EC snapshot at 0x{address:02X}: {source}")]
    SnapshotFailed { address: u8, source: HalError },
    #[error("EC write failed; original values restored: {failure}")]
    ApplyFailed { failure: TransactionFailure },
    #[error("EC transaction timed out; rollback cannot start while I/O may still be in flight: {failure}")]
    Indeterminate { failure: TransactionFailure },
    #[error("EC rollback failed ({rollback}) after an apply failure ({failure})")]
    RollbackFailed { failure: TransactionFailure, rollback: TransactionFailure },
}

#[cfg(test)]
mod tests;
