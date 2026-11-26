use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, Read};
use std::path::Path;

use msilt_core::ec::{EcMap, EcMapError, MAX_EC_MAP_BYTES};
use serde::{Deserialize, Deserializer, Serialize, de};
use serde_json::{Value, value::RawValue};
use thiserror::Error;

const EMBEDDED_MAP: &[u8] = include_bytes!("../../../configs/ec_maps/ms-17s3.json");
pub(crate) const BOARD_ID: &str = "MS-17S3";
pub(crate) const BOARD_REVISION: &str = "REV:1.0";
pub(crate) const BIOS_VERSION: &str = "E17S3IMS.112";
pub(crate) const EC_FIRMWARE: &str = "17S3EMS1.107";
pub(crate) const WMI_VERSION: (u8, u8) = (2, 8);
pub(crate) const DSDT_SHA256: &str =
    "d85ba1cf4eef70adcc2e98b7bf413670e90f39f815140c9c5f06c58f839d5a64";

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MapIdentity {
    pub model: String,
    pub board_id: String,
    pub board_revision: String,
    pub bios_version: String,
    pub ec_firmware: String,
    pub ec_research_prefix: String,
    pub wmi_version: String,
    pub dsdt_sha256: String,
}

#[derive(Debug)]
pub struct LoadedEcMap {
    pub identity: MapIdentity,
    pub guard_map: EcMap,
}

#[derive(Debug, Error)]
pub enum LoadError {
    #[error("EC envelope could not be read")]
    Io(#[source] io::Error),
    #[error("EC envelope exceeds {MAX_EC_MAP_BYTES} bytes")]
    TooLarge,
    #[error("invalid EC envelope JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("invalid EC envelope field: {0}")]
    Invalid(&'static str),
    #[error("EC envelope grants or claims verification for a Phase 0 write")]
    PhaseZeroPolicy,
    #[error("invalid Core guardMap: {0}")]
    Guard(#[from] EcMapError),
}

pub fn load_ms_17s3() -> Result<LoadedEcMap, LoadError> {
    parse_ms_17s3(EMBEDDED_MAP)
}

pub fn load_ms_17s3_file(path: &Path) -> Result<LoadedEcMap, LoadError> {
    let mut bytes = Vec::new();
    File::open(path)
        .map_err(LoadError::Io)?
        .take(MAX_EC_MAP_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(LoadError::Io)?;
    parse_ms_17s3(&bytes)
}

pub fn parse_ms_17s3(bytes: &[u8]) -> Result<LoadedEcMap, LoadError> {
    if bytes.len() > MAX_EC_MAP_BYTES {
        return Err(LoadError::TooLarge);
    }
    let env: Envelope<'_> = serde_json::from_slice(bytes)?;
    if env.schema_version != 1 || !env.schema.valid() {
        return Err(LoadError::Invalid("schema"));
    }
    let id = &env.identity;
    if id.model != "Vector 17 HX AI A2XWJG"
        || id.board_id != BOARD_ID
        || id.board_revision != BOARD_REVISION
        || id.bios_version != BIOS_VERSION
        || id.ec_firmware != EC_FIRMWARE
        || id.ec_research_prefix != "17S3EMS1.1"
        || id.wmi_version != format!("{}.{}", WMI_VERSION.0, WMI_VERSION.1)
        || id.dsdt_sha256 != DSDT_SHA256
    {
        return Err(LoadError::Invalid("identity"));
    }
    let guard: Guard<'_> = serde_json::from_str(env.guard_map.get())?;
    if env.verified
        || !env.write_policy.read_only()
        || !guard.write_allowlist.is_empty()
        || !guard.fan_curves.is_empty()
        || env.registers.iter().any(|entry| entry.verified)
        || env.fan_curves.verified
        || env.fan_curves.cpu.verified
        || env.fan_curves.gpu.verified
    {
        return Err(LoadError::PhaseZeroPolicy);
    }
    if guard.schema_version != 1
        || guard.board_id != id.board_id
        || guard.firmware_prefixes != [id.ec_firmware.as_str()]
        || ![0x50, 0x51, 0xD1, 0xD8]
            .iter()
            .all(|&a| guard.deny_write.contains(&Address(a)))
        || !unique(&guard.deny_write)
    {
        return Err(LoadError::Invalid("guardMap identity or deny list"));
    }
    if !env.sources.values().into_iter().all(present)
        || env.capability_evidence.is_empty()
        || env.capability_evidence.iter().any(|row| {
            !present(&row.feature) || !present(&row.observation) || !evidence(&row.evidence)
        })
        || env.registers.iter().any(|entry| !entry.valid())
        || !env.telemetry.valid()
        || !env.fan_curves.valid()
    {
        return Err(LoadError::Invalid("research evidence"));
    }
    // Keep the original JSON here so Core still sees duplicate keys and its own strict types.
    let guard_map = EcMap::parse_json(env.guard_map.get().as_bytes())?;
    Ok(LoadedEcMap {
        identity: env.identity,
        guard_map,
    })
}

fn present(value: &str) -> bool {
    !value.trim().is_empty()
}

fn unique<T: PartialEq>(items: &[T]) -> bool {
    items
        .iter()
        .enumerate()
        .all(|(i, value)| !items[..i].contains(value))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Envelope<'a> {
    schema: Schema,
    schema_version: u16,
    verified: bool,
    identity: MapIdentity,
    sources: Sources,
    #[serde(borrow)]
    guard_map: &'a RawValue,
    write_policy: WritePolicy,
    capability_evidence: Vec<EvidenceRow>,
    registers: Registers,
    telemetry: Telemetry,
    fan_curves: ResearchCurves,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Schema {
    #[serde(rename = "$schema")]
    dialect: String,
    #[serde(rename = "$id")]
    id: String,
    #[serde(rename = "$defs")]
    definitions: BTreeMap<String, Value>,
    #[serde(rename = "type")]
    kind: String,
    title: String,
    description: String,
    additional_properties: bool,
    required: Vec<String>,
    properties: BTreeMap<String, Value>,
}

impl Schema {
    fn valid(&self) -> bool {
        let fields = [
            "schema",
            "schemaVersion",
            "verified",
            "identity",
            "sources",
            "guardMap",
            "writePolicy",
            "capabilityEvidence",
            "registers",
            "telemetry",
            "fanCurves",
        ];
        self.dialect == "https://json-schema.org/draft/2020-12/schema"
            && self.id == "urn:msilt:ec-map:ms-17s3:1"
            && self.kind == "object"
            && present(&self.title)
            && present(&self.description)
            && !self.additional_properties
            && self.required.len() == fields.len()
            && self.properties.len() == fields.len()
            && fields.iter().all(|name| {
                self.required.iter().any(|field| field == name)
                    && self.properties.get(*name).is_some_and(Value::is_object)
            })
            && !self.definitions.is_empty()
            && self.definitions.values().all(Value::is_object)
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Sources {
    aml: String,
    oem_log: String,
    scenario: String,
    general_settings: String,
    #[serde(rename = "g2_10")]
    g2_10: String,
    brief: String,
}

impl Sources {
    fn values(&self) -> [&str; 6] {
        [
            &self.aml,
            &self.oem_log,
            &self.scenario,
            &self.general_settings,
            &self.g2_10,
            &self.brief,
        ]
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Guard<'a> {
    schema_version: u16,
    board_id: &'a str,
    firmware_prefixes: Vec<String>,
    deny_write: Vec<Address>,
    write_allowlist: Vec<Value>,
    fan_curves: BTreeMap<String, Value>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WritePolicy {
    enabled: bool,
    unlisted_address: String,
    identity_match: String,
    snapshot_before_write: bool,
    read_back_after_write: bool,
    unsettled_call: String,
    rollback: String,
    notes: String,
}

impl WritePolicy {
    fn read_only(&self) -> bool {
        !self.enabled
            && self.unlisted_address == "deny"
            && self.identity_match == "exact"
            && self.snapshot_before_write
            && self.read_back_after_write
            && self.unsettled_call == "quarantine"
            && self.rollback == "knownCompletedWritesToVerifiedSafeSnapshotOnly"
            && present(&self.notes)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
enum SourceId {
    Aml,
    OemLog,
    Scenario,
    GeneralSettings,
    #[serde(rename = "g2_10")]
    G2_10,
    Brief,
}

fn evidence(ids: &[SourceId]) -> bool {
    !ids.is_empty() && unique(ids)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EvidenceRow {
    feature: String,
    evidence: Vec<SourceId>,
    observation: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Address(u8);

impl<'de> Deserialize<'de> for Address {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        text.strip_prefix("0x")
            .filter(|s| {
                s.len() == 2
                    && s.bytes()
                        .all(|b| b.is_ascii_digit() || (b'A'..=b'F').contains(&b))
            })
            .and_then(|s| u8::from_str_radix(s, 16).ok())
            .map(Self)
            .ok_or_else(|| de::Error::custom("expected uppercase EC address 0xHH"))
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Registers {
    shift_mode: Register,
    fan_mode: Register,
    super_battery: Register,
    cooler_boost: Register,
    charge_limit: Register,
    webcam: Register,
    fn_win_swap: Register,
    keyboard_brightness: Register,
    microphone_mute_led: Register,
    speaker_mute_led: Register,
    touchpad: Register,
    win_key: Register,
    usb_charging: Register,
    gpu_mux_ec_indicator: Register,
}

impl Registers {
    fn iter(&self) -> impl Iterator<Item = &Register> {
        [
            &self.shift_mode,
            &self.fan_mode,
            &self.super_battery,
            &self.cooler_boost,
            &self.charge_limit,
            &self.webcam,
            &self.fn_win_swap,
            &self.keyboard_brightness,
            &self.microphone_mute_led,
            &self.speaker_mute_led,
            &self.touchpad,
            &self.win_key,
            &self.usb_charging,
            &self.gpu_mux_ec_indicator,
        ]
        .into_iter()
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
enum Encoding {
    Enum,
    Masked,
    Bit,
    BasePlusPercent,
    CandidateLevel,
    RawIndicator,
    Unknown,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Register {
    // Required, including explicit null for an unlocated register.
    address: NullableAddress,
    secondary_address: Option<Address>,
    encoding: Encoding,
    verified: bool,
    evidence: Vec<SourceId>,
    notes: String,
    modes: Option<BTreeMap<String, u8>>,
    mask: Option<u8>,
    bit: Option<u8>,
    inverted: Option<bool>,
    base: Option<u8>,
    candidate_percent_steps: Option<Vec<u8>>,
    candidate_levels: Option<Vec<u8>>,
    observed_raw: Option<Vec<u8>>,
}

#[derive(Deserialize)]
struct NullableAddress(Option<Address>);

impl Register {
    fn valid(&self) -> bool {
        let values = |entries: &Vec<u8>| !entries.is_empty() && unique(entries);
        let modes = self
            .modes
            .as_ref()
            .is_some_and(|m| !m.is_empty() && m.keys().all(|k| present(k)));
        if !evidence(&self.evidence)
            || !present(&self.notes)
            || self.address.0.is_none() != matches!(self.encoding, Encoding::Unknown)
            || self
                .secondary_address
                .is_some_and(|a| Some(a) == self.address.0)
            || self.mask == Some(0)
            || self.bit.is_some_and(|bit| bit > 7)
            || !self.observed_raw.as_ref().is_none_or(values)
            || !self.candidate_levels.as_ref().is_none_or(values)
            || !self
                .candidate_percent_steps
                .as_ref()
                .is_none_or(|steps| values(steps) && steps.iter().all(|p| (1..=100).contains(p)))
        {
            return false;
        }
        match self.encoding {
            Encoding::Enum => modes,
            Encoding::Masked => {
                modes
                    && self.mask.is_some_and(|mask| {
                        self.modes
                            .as_ref()
                            .is_some_and(|m| m.values().all(|v| v & !mask == 0))
                    })
            }
            Encoding::Bit => {
                self.bit.is_some_and(|b| self.mask == Some(1 << b)) && self.inverted.is_some()
            }
            Encoding::BasePlusPercent => self
                .base
                .zip(self.candidate_percent_steps.as_ref())
                .is_some_and(|(base, steps)| steps.iter().all(|&p| base.checked_add(p).is_some())),
            Encoding::CandidateLevel => self.candidate_levels.is_some(),
            Encoding::Unknown | Encoding::RawIndicator => true,
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Telemetry {
    temperature: Metric,
    reported_duty: Metric,
    rpm: Metric,
}

impl Telemetry {
    fn valid(&self) -> bool {
        self.temperature
            .valid("Get_Temperature", "u8", "celsius", None, [(0, 1), (0, 2)])
            && self
                .reported_duty
                .valid("Get_Fan", "u8", "percent", None, [(1, 1), (2, 1)])
            && self.rpm.valid(
                "Get_Fan",
                "u16bePeriod",
                "rpm",
                Some(480_000),
                [(0, 1), (0, 3)],
            )
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Metric {
    method: String,
    encoding: String,
    unit: String,
    status_byte: u8,
    ok_value: u8,
    rpm_numerator: Option<u32>,
    channels: Channels,
    notes: String,
}

impl Metric {
    fn valid(
        &self,
        method: &str,
        encoding: &str,
        unit: &str,
        numerator: Option<u32>,
        channels: [(u8, u8); 2],
    ) -> bool {
        self.method == method
            && self.encoding == encoding
            && self.unit == unit
            && self.status_byte == 0
            && self.ok_value == 1
            && self.rpm_numerator == numerator
            && present(&self.notes)
            && [&self.channels.cpu, &self.channels.gpu]
                .into_iter()
                .zip(channels)
                .all(|(ch, shape)| {
                    (ch.selector, ch.response_index) == shape
                        && ch.addresses.len() == if numerator.is_some() { 2 } else { 1 }
                        && unique(&ch.addresses)
                })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Channels {
    cpu: Channel,
    gpu: Channel,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Channel {
    addresses: Vec<Address>,
    selector: u8,
    response_index: u8,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ResearchCurves {
    verified: bool,
    point_count: u8,
    max_duty_percent: u8,
    fixed_anchor_index: u8,
    temperature_knots_writable: bool,
    minimum_duty_policy: String,
    cpu: ResearchCurve,
    gpu: ResearchCurve,
}

impl ResearchCurves {
    fn valid(&self) -> bool {
        self.point_count == 6
            && self.max_duty_percent == 150
            && self.fixed_anchor_index == 0
            && !self.temperature_knots_writable
            && self.minimum_duty_policy == "fixedFactoryTemperaturePerPointFactoryDutyFloor"
            && self.cpu.valid()
            && self.gpu.valid()
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ResearchCurve {
    verified: bool,
    temperature_addresses: [Address; 6],
    duty_addresses: [Address; 6],
    factory_temperature_c: [u8; 6],
    factory_duty_percent: [u8; 6],
    preserve_addresses: Vec<Address>,
}

impl ResearchCurve {
    fn valid(&self) -> bool {
        let addresses: Vec<_> = self
            .temperature_addresses
            .iter()
            .chain(&self.duty_addresses)
            .chain(&self.preserve_addresses)
            .copied()
            .collect();
        !self.preserve_addresses.is_empty()
            && unique(&addresses)
            && self.factory_temperature_c.windows(2).all(|w| w[0] < w[1])
            && self.factory_temperature_c[5] <= 125
            && self.factory_duty_percent.windows(2).all(|w| w[0] <= w[1])
            && (1..=150).contains(&self.factory_duty_percent[5])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn altered(field: &str, replacement: Value) -> Result<LoadedEcMap, LoadError> {
        load_ms_17s3().unwrap();
        let mut envelope: Value = serde_json::from_slice(EMBEDDED_MAP).unwrap();
        *envelope.pointer_mut(field).unwrap() = replacement;
        parse_ms_17s3(&serde_json::to_vec(&envelope).unwrap())
    }

    #[test]
    fn parses_full_envelope_but_only_core_guard_is_executable() {
        let loaded = load_ms_17s3().unwrap();
        assert_eq!(loaded.identity.ec_firmware, EC_FIRMWARE);
        for addr in [0x50, 0x51, 0xD1, 0xD8] {
            assert!(loaded.guard_map.denies_write(addr));
        }
        assert!(
            loaded
                .guard_map
                .fan_curve_limits(msilt_core::FanChannel::Cpu)
                .is_none()
        );
        assert!(EcMap::parse_json(EMBEDDED_MAP).is_err());
    }

    #[test]
    fn rejects_scope_escalation_and_research_prefix_promotion() {
        for (field, value) in [
            (
                "/guardMap/writeAllowlist",
                json!([{"address": "0xD2", "values": [193]}]),
            ),
            ("/guardMap/fanCurves", json!({"cpu": {}})),
            ("/writePolicy/enabled", json!(true)),
            ("/fanCurves/cpu/verified", json!(true)),
            ("/guardMap/firmwarePrefixes", json!(["17S3EMS1.1"])),
            ("/guardMap/denyWrite", json!(["0x50", "0x51", "0xD1"])),
        ] {
            assert!(altered(field, value).is_err(), "accepted {field}");
        }
    }

    #[test]
    fn rejects_malformed_identity_evidence_and_duplicate_fields() {
        for (field, value) in [
            ("/identity/boardRevision", json!("REV:2.0")),
            ("/identity/dsdtSha256", json!("00")),
            ("/telemetry/rpm/channels/cpu/responseIndex", json!(32)),
            ("/registers/webcam/bit", json!(8)),
            ("/registers/touchpad/address", json!("0x01")),
        ] {
            assert!(altered(field, value).is_err(), "accepted {field}");
        }
        assert!(matches!(
            parse_ms_17s3(&vec![b' '; MAX_EC_MAP_BYTES + 1]),
            Err(LoadError::TooLarge)
        ));
        let mut env: Value = serde_json::from_slice(EMBEDDED_MAP).unwrap();
        let guard = env.as_object_mut().unwrap().remove("guardMap").unwrap();
        let text = serde_json::to_string(&env).unwrap();
        let repeated = format!(
            "{},\"guardMap\":{},\"guardMap\":{guard}}}",
            &text[..text.len() - 1],
            guard
        );
        assert!(parse_ms_17s3(repeated.as_bytes()).is_err());
    }
}
