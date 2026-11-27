//! Exact MS-17S3 register bindings shared by the snapshot and the opt-in write policy.

use std::sync::OnceLock;

use msilt_core::{
    EcMap, FanChannel, FanCurve, FanMode, FanPoint, FanState, HardwareIdentity, PerformanceMode,
};
use serde_json::json;

use crate::ec_map_loader::{BOARD_ID, EC_FIRMWARE};

pub const COOLER: u8 = 0x98;
pub const COOLER_MASK: u8 = 0x80;
pub const SHIFT: u8 = 0xD2;
pub const FAN_MODE: u8 = 0xD4;
pub const FAN_MODE_AUTO: u8 = 0x0D;
pub const FAN_MODE_SILENT: u8 = 0x1D;
pub const FAN_MODE_ADVANCED: u8 = 0x8D;
pub const BATTERY_MODE: u8 = 0xEB;
pub const BATTERY_MODE_MASK: u8 = 0x0F;
pub const CHARGE: u8 = 0xD7;
pub const CHARGE_BASE: u8 = 0x80;
pub const CHARGE_LIMITS: [u8; 3] = [60, 80, 100];
pub const MODES: [PerformanceMode; 4] = [
    PerformanceMode::SuperBattery,
    PerformanceMode::Silent,
    PerformanceMode::Balanced,
    PerformanceMode::ExtremePerformance,
];

pub(crate) const MUX: u8 = 0xD1;
pub(crate) const CPU_TEMPERATURE: u8 = 0x69;
pub(crate) const CPU_DUTY: u8 = 0x72;
pub(crate) const GPU_TEMPERATURE: u8 = 0x81;
pub(crate) const GPU_DUTY: u8 = 0x8A;

const CPU_KNOTS: [u8; 6] = [0, 48, 56, 62, 70, 78];
const CPU_FLOOR: [u8; 6] = [0, 36, 42, 52, 76, 84];
const GPU_KNOTS: [u8; 6] = [0, 43, 48, 56, 65, 72];
const GPU_FLOOR: [u8; 6] = [0, 36, 46, 56, 80, 88];

// Kept in sync with the frozen Phase 0 envelope by a test; the daemon needs the
// executable deny list, not the schema and research metadata used by the probe.
const DENY: &[u8] = &[
    0x2F, 0x33, 0x34, 0x50, 0x51, 0x68, 0x69, 0x6A, 0x6B, 0x6C, 0x6D, 0x6E, 0x6F, 0x70, 0x71,
    CPU_DUTY, 0x78, 0x79, 0x7A, 0x7B, 0x7C, 0x7D, 0x7E, 0x7F, 0x80, 0x81, 0x82, 0x83, 0x84, 0x85,
    0x86, 0x87, 0x88, 0x89, GPU_DUTY, 0x90, 0x91, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97, 0x99, 0x9A,
    0x9B, 0x9C, 0x9D, 0x9E, 0x9F, 0xC8, 0xC9, 0xCA, 0xCB, 0xCC, 0xCD, 0xCE, 0xCF, MUX, 0xD8,
];

pub(crate) fn denies_write(address: u8) -> bool {
    DENY.contains(&address)
}

pub fn matches_identity(identity: &HardwareIdentity) -> bool {
    identity.board_id.as_deref() == Some(BOARD_ID)
        && identity.ec_firmware.as_deref() == Some(EC_FIRMWARE)
}

pub fn mode_registers(mode: PerformanceMode) -> (u8, u8, u8) {
    match mode {
        PerformanceMode::SuperBattery => (0xC2, FAN_MODE_AUTO, 0x0F),
        PerformanceMode::Silent => (0xC1, FAN_MODE_SILENT, 0),
        PerformanceMode::Balanced => (0xC1, FAN_MODE_AUTO, 0),
        PerformanceMode::ExtremePerformance => (0xC4, FAN_MODE_AUTO, 0),
    }
}

pub const fn default_fan_mode(mode: PerformanceMode) -> FanMode {
    match mode {
        PerformanceMode::Silent => FanMode::Silent,
        PerformanceMode::SuperBattery
        | PerformanceMode::Balanced
        | PerformanceMode::ExtremePerformance => FanMode::Auto,
    }
}

pub const fn fan_mode_byte(mode: FanMode) -> u8 {
    match mode {
        FanMode::Auto => FAN_MODE_AUTO,
        FanMode::Silent => FAN_MODE_SILENT,
        FanMode::Advanced => FAN_MODE_ADVANCED,
    }
}

pub fn decode_fan_mode(raw: u8) -> Option<FanMode> {
    match raw {
        FAN_MODE_AUTO => Some(FanMode::Auto),
        FAN_MODE_SILENT => Some(FanMode::Silent),
        FAN_MODE_ADVANCED => Some(FanMode::Advanced),
        _ => None,
    }
}

pub(crate) fn decode_mode(shift: u8, fan: u8, battery: u8) -> Option<PerformanceMode> {
    match (shift, fan, battery & BATTERY_MODE_MASK) {
        (0xC1 | 0xC2 | 0xC4, 0x0D | 0x1D | 0x8D, 0x0F) => Some(PerformanceMode::SuperBattery),
        (0xC1 | 0xC2, 0x1D, 0) => Some(PerformanceMode::Silent),
        (0xC1, 0x0D | 0x8D, 0) => Some(PerformanceMode::Balanced),
        (0xC4, 0x0D | 0x8D, 0) => Some(PerformanceMode::ExtremePerformance),
        _ => None,
    }
}

pub(crate) fn decode_charge(raw: u8) -> Option<u8> {
    if raw == CHARGE_BASE {
        return Some(100);
    }
    raw.checked_sub(CHARGE_BASE)
        .filter(|percent| (1..=100).contains(percent))
}

pub fn write_map() -> Option<&'static EcMap> {
    static MAP: OnceLock<Option<EcMap>> = OnceLock::new();
    MAP.get_or_init(|| {
        // The guard needs both anchor addresses to verify the complete curve. They have
        // no register rule, the fixed 0% anchor cannot change, and HAL still denies writes.
        let deny = DENY.iter().copied().filter(|&address| !matches!(address, CPU_DUTY | GPU_DUTY)).collect::<Vec<_>>();
        if ![0x50, 0x51, MUX].iter().all(|address| deny.contains(address)) { return None; }
        let curve = |temperature: [u8; 6], duty: [u8; 6], start: u8| {
            let points: [_; 6] = std::array::from_fn(|i| json!({"temperatureC": temperature[i], "minimumDutyPercent": duty[i], "dutyRegister": start + i as u8}));
            json!({"points": points, "maxDutyPercent": 150, "fixedAnchor": true})
        };
        EcMap::parse_json(&serde_json::to_vec(&json!({
            "schemaVersion": 1, "boardId": BOARD_ID, "firmwarePrefixes": [EC_FIRMWARE],
            "denyWrite": deny,
            "writeAllowlist": [
                {"address": COOLER, "values": (0..=255u8).collect::<Vec<_>>()},
                {"address": SHIFT, "values": [0xC1, 0xC2, 0xC4]},
                {"address": FAN_MODE, "values": [0x0D, 0x1D, 0x8D]},
                {"address": BATTERY_MODE, "values": (0..=255u8).filter(|&n| matches!(n & BATTERY_MODE_MASK, 0 | 0x0F)).collect::<Vec<_>>()},
                {"address": CHARGE, "values": [CHARGE_BASE, CHARGE_BASE + 60, CHARGE_BASE + 80, CHARGE_BASE + 100]}
            ],
            "fanCurves": {
                "cpu": curve(CPU_KNOTS, CPU_FLOOR, CPU_DUTY),
                "gpu": curve(GPU_KNOTS, GPU_FLOOR, GPU_DUTY)
            }
        })).ok()?).ok()
    }).as_ref()
}

pub(crate) fn fan_state(
    channel: FanChannel,
    temperatures: [u8; 6],
    duties: [u8; 6],
) -> Option<FanState> {
    let curve = FanCurve {
        points: std::array::from_fn(|i| FanPoint {
            temperature_c: temperatures[i],
            duty_percent: duties[i],
        }),
    };
    curve.validate_shape().ok()?;
    let limits = write_map()
        .and_then(|map| map.fan_curve_limits(channel))
        .filter(|limits| curve.validate_with_limits(limits).is_ok());
    Some(FanState { curve, limits })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scene_readback_uses_all_three_fields_and_rejects_unmapped_values() {
        for mode in MODES {
            let (shift, fan, battery) = mode_registers(mode);
            assert_eq!(decode_mode(shift, fan, 0xB0 | battery), Some(mode));
        }
        assert_eq!(
            decode_mode(0xC1, 0x8D, 0x10),
            Some(PerformanceMode::Balanced)
        );
        assert_eq!(decode_mode(0xC4, 0x1D, 0), None);
        assert_eq!(decode_mode(0xC1, 0x0D, 0x07), None);
        assert_eq!(decode_charge(0xD0), Some(80));
        assert_eq!(decode_charge(0x80), Some(100));
        assert_eq!(decode_charge(0x7F), None);
        assert_eq!(decode_charge(0xE5), None);
        assert_eq!(decode_fan_mode(FAN_MODE_AUTO), Some(FanMode::Auto));
        assert_eq!(decode_fan_mode(FAN_MODE_SILENT), Some(FanMode::Silent));
        assert_eq!(decode_fan_mode(FAN_MODE_ADVANCED), Some(FanMode::Advanced));
        assert_eq!(decode_fan_mode(0x00), None);
        assert_eq!(default_fan_mode(PerformanceMode::Silent), FanMode::Silent);
        assert_eq!(default_fan_mode(PerformanceMode::Balanced), FanMode::Auto);
    }

    #[test]
    fn only_factory_temperature_knots_with_safe_baselines_are_editable() {
        let map = write_map().unwrap();
        for address in [0x50, 0x51, MUX, 0x69, 0x81, 0xD8] {
            assert!(map.denies_write(address));
        }
        assert!(
            fan_state(FanChannel::Cpu, CPU_KNOTS, CPU_FLOOR)
                .unwrap()
                .limits
                .is_some()
        );
        assert!(
            fan_state(FanChannel::Gpu, GPU_KNOTS, [0, 1, 2, 3, 4, 5])
                .unwrap()
                .limits
                .is_none()
        );
    }

    #[test]
    fn compact_daemon_deny_list_matches_the_frozen_research_envelope() {
        let map = crate::ec_map_loader::load_ms_17s3().unwrap().guard_map;
        for address in 0..=255 {
            assert_eq!(
                denies_write(address),
                map.denies_write(address),
                "{address:#X}"
            );
        }
    }
}
