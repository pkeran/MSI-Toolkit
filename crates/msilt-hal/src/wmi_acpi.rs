use std::mem::ManuallyDrop;
use std::ptr;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use msilt_core::{AuthorizedEcWrite, TelemetrySample};
use serde::Serialize;
use sha2::{Digest, Sha256};
use windows::Win32::Foundation::{RPC_E_TOO_LATE, S_OK};
use windows::Win32::System::Com::*;
use windows::Win32::System::Ole::*;
use windows::Win32::System::SystemInformation::{FIRMWARE_TABLE_PROVIDER, GetSystemFirmwareTable};
use windows::Win32::System::Variant::*;
use windows::Win32::System::Wmi::*;
use windows::core::{BSTR, IUnknown, Interface, PCWSTR, w};

use crate::ec_map_loader::{
    BIOS_VERSION, BOARD_ID, BOARD_REVISION, DSDT_SHA256, EC_FIRMWARE, WMI_VERSION,
};
use crate::executor::{SerialExecutor, before_deadline};
use crate::identity;
use crate::ms17s3;
use crate::native::NamedMutex;
use crate::{Diagnostic, DiagnosticKind, NativeDomain};

const ACPI_CONNECT_BUDGET: Duration = Duration::from_secs(3);
const READ_BUDGET: Duration = Duration::from_millis(300);
const SNAPSHOT_BUDGET: Duration = Duration::from_millis(1_200);
const WRITE_BUDGET: Duration = Duration::from_millis(1200);
const BRIGHTNESS_BUDGET: Duration = Duration::from_secs(2);
const PACKAGE_SIZE: usize = 32;
const ARRAY_UI1: VARENUM = VARENUM(VT_ARRAY.0 | VT_UI1.0);
const NO_FLAGS: WBEM_GENERIC_FLAG_TYPE = WBEM_GENERIC_FLAG_TYPE(0);
const READ_ADDRESSES: &[u8] = &[
    0x2C, 0x2D, 0x2E, 0x2F, 0x68, 0x71, 0x80, 0x89, 0x98, 0xC8, 0xC9, 0xCA, 0xCB, 0xD1, 0xD2, 0xD3,
    0xD4, 0xD7, 0xE8, 0xEB,
];
const READ_CURVE_ADDRESSES: &[std::ops::RangeInclusive<u8>] = &[0x69..=0x77, 0x81..=0x8F];
const PROBE_ADDRESSES: &[u8] = &[0x2E, 0x2F, 0x98, 0xD1, 0xD2, 0xD3, 0xD4, 0xD7, 0xE8];

#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AcpiReport {
    pub active_instances: Option<u32>,
    pub identity_matched: bool,
    pub ec_firmware: Option<String>,
    pub wmi_version: Option<String>,
    pub methods: Vec<AcpiMethodReport>,
    pub telemetry: TelemetrySample,
    pub mux_support_indicator: Option<bool>,
    pub mux_ec_indicator: Option<u8>,
    pub cooler_boost_bit: Option<bool>,
    pub diagnostics: Vec<Diagnostic>,
    #[serde(skip)]
    pub(crate) control_bytes: Option<ControlBytes>,
}

#[derive(Debug, Default)]
pub(crate) struct ControlBytes {
    scalars: [Option<u8>; 6],
    cpu_temperature: [Option<u8>; 6],
    cpu_duty: [Option<u8>; 6],
    gpu_temperature: [Option<u8>; 6],
    gpu_duty: [Option<u8>; 6],
}

impl ControlBytes {
    const SCALAR_ADDRESSES: [u8; 6] = [
        ms17s3::COOLER,
        ms17s3::MUX,
        ms17s3::SHIFT,
        ms17s3::FAN_MODE,
        ms17s3::CHARGE,
        ms17s3::BATTERY_MODE,
    ];

    pub(crate) fn scalar(&self, address: u8) -> Option<u8> {
        Self::SCALAR_ADDRESSES
            .iter()
            .position(|&n| n == address)
            .and_then(|i| self.scalars[i])
    }

    pub(crate) fn fan(&self, channel: msilt_core::FanChannel) -> Option<msilt_core::FanState> {
        let (knots, duties) = match channel {
            msilt_core::FanChannel::Cpu => (self.cpu_temperature, self.cpu_duty),
            msilt_core::FanChannel::Gpu => (self.gpu_temperature, self.gpu_duty),
        };
        let complete = |bytes: [Option<u8>; 6]| -> Option<[u8; 6]> {
            let [a, b, c, d, e, f] = bytes;
            Some([a?, b?, c?, d?, e?, f?])
        };
        ms17s3::fan_state(channel, complete(knots)?, complete(duties)?)
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AcpiMethodReport {
    pub method: &'static str,
    pub selector: Option<u8>,
    pub elapsed_us: u64,
    pub package_bytes: Option<usize>,
    pub wmi_return_value: Option<bool>,
    pub prefix: Option<Vec<u8>>,
    pub error: Option<Diagnostic>,
}

#[derive(Clone, Copy)]
enum Method {
    Wmi,
    Ec,
    Temperature,
    FanRpm,
    FanCpu,
    FanGpu,
    Thermal,
    Ap0,
    Ap1,
    Device,
    Data(ReadAddress),
    SetData(WriteRequest),
}

impl Method {
    fn name(self) -> &'static str {
        match self {
            Self::Wmi => "Get_WMI",
            Self::Ec => "Get_EC",
            Self::Temperature => "Get_Temperature",
            Self::FanRpm | Self::FanCpu | Self::FanGpu => "Get_Fan",
            Self::Thermal => "Get_Thermal",
            Self::Ap0 | Self::Ap1 => "Get_AP",
            Self::Device => "Get_Device",
            Self::Data(_) => "Get_Data",
            Self::SetData(_) => "Set_Data",
        }
    }

    fn selector(self) -> Option<u8> {
        match self {
            Self::Wmi | Self::Ec => None,
            Self::Temperature | Self::FanRpm | Self::Ap0 => Some(0),
            Self::FanCpu | Self::Ap1 | Self::Device => Some(1),
            Self::FanGpu => Some(2),
            Self::Thermal => Some(3),
            Self::Data(addr) => Some(addr.0),
            Self::SetData(request) => Some(request.address),
        }
    }

    fn prefix_len(self) -> usize {
        match self {
            Self::Wmi => 3,
            Self::Ec => 30,
            Self::Temperature | Self::Ap1 => 4,
            Self::FanRpm | Self::FanCpu | Self::FanGpu => 9,
            Self::Thermal => 8,
            Self::Ap0 | Self::Device => 7,
            Self::Data(_) => 2,
            Self::SetData(_) => 1,
        }
    }

    fn input(self) -> Option<[u8; PACKAGE_SIZE]> {
        let mut bytes = [0; PACKAGE_SIZE];
        bytes[0] = self.selector()?;
        if let Self::SetData(request) = self {
            bytes[1] = request.value;
        }
        Some(bytes)
    }
}

#[derive(Clone, Copy)]
struct WriteRequest {
    address: u8,
    value: u8,
}

impl WriteRequest {
    fn from_permit(permit: &AuthorizedEcWrite) -> Result<Self, Diagnostic> {
        let address = permit.address();
        ReadAddress::check(address)?;
        if ms17s3::denies_write(address) {
            return Err(
                Diagnostic::new("Set_Data", DiagnosticKind::Rejected).detail(format!(
                    "address 0x{address:02X} is protected by the MS-17S3 baseline"
                )),
            );
        }
        Ok(Self {
            address,
            value: permit.value(),
        })
    }
}

#[derive(Clone, Copy)]
struct ReadAddress(u8);

impl ReadAddress {
    fn check(address: u8) -> Result<Self, Diagnostic> {
        (READ_ADDRESSES.contains(&address)
            || READ_CURVE_ADDRESSES
                .iter()
                .any(|range| range.contains(&address)))
        .then_some(Self(address))
        .ok_or_else(|| {
            Diagnostic::new("Get_Data", DiagnosticKind::Unsupported).detail(format!(
                "address 0x{address:02X} is outside the read allowlist"
            ))
        })
    }
}

struct Packet {
    bytes: [u8; PACKAGE_SIZE],
    return_value: Option<bool>,
}

impl Packet {
    fn check(
        bytes: &[u8],
        return_value: Option<bool>,
        op: &'static str,
    ) -> Result<Self, Diagnostic> {
        let bytes: [u8; PACKAGE_SIZE] = bytes.try_into().map_err(|_| {
            Diagnostic::invalid(
                op,
                format!("Package_32 length {}, expected {PACKAGE_SIZE}", bytes.len()),
            )
        })?;
        if bytes[0] != 1 {
            return Err(Diagnostic::new(op, DiagnosticKind::Rejected)
                .detail(format!("Bytes[0]=0x{:02X}, expected 0x01", bytes[0])));
        }
        Ok(Self {
            bytes,
            return_value,
        })
    }
}

#[derive(Default, Clone)]
struct FirmwareIdentity {
    ec: Option<String>,
    wmi: Option<(u8, u8)>,
}

pub(crate) struct AcpiReader {
    generation: Option<Generation>,
    firmware: FirmwareIdentity,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Generation(u64);

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Poll {
    Identity,
    Snapshot,
    Telemetry,
    Full,
}

impl AcpiReader {
    pub(crate) fn authorize() -> Result<Self, Diagnostic> {
        const OP: &str = "MSI_ACPI platform fingerprint";
        identity::require_admin()?;
        let worker = executor()?;
        worker.run(OP, ACPI_CONNECT_BUDGET, |_, deadline| {
            if identity::read_bios(w!("BaseBoardProduct"), OP)? != BOARD_ID
                || identity::read_bios(w!("BaseBoardVersion"), OP)? != BOARD_REVISION
                || identity::read_bios(w!("BIOSVersion"), OP)? != BIOS_VERSION
            {
                return Err(Diagnostic::new(OP, DiagnosticKind::Unsupported)
                    .detail("board, revision or BIOS differs from the audited baseline"));
            }
            before_deadline(deadline, OP)?;
            verify_dsdt()?;
            Ok(Self {
                generation: None,
                firmware: FirmwareIdentity::default(),
            })
        })
    }

    pub(crate) fn poll(&mut self, poll: Poll, unix_ms: u64) -> AcpiReport {
        let mut report = AcpiReport {
            telemetry: TelemetrySample {
                sampled_at_unix_ms: unix_ms,
                ..Default::default()
            },
            ..Default::default()
        };
        if let Err(error) = self.populate(&mut report, poll) {
            if !report
                .methods
                .iter()
                .any(|row| row.error.as_ref() == Some(&error))
            {
                report.diagnostics.push(error);
            }
        }
        report
    }

    fn populate(&mut self, report: &mut AcpiReport, poll: Poll) -> Result<(), Diagnostic> {
        let refresh = poll != Poll::Telemetry;
        let (count, generation) = executor()?.run(
            "MSI_ACPI session",
            ACPI_CONNECT_BUDGET,
            move |worker, deadline| worker.ensure_acpi(refresh, deadline),
        )?;
        report.active_instances = Some(count);
        let Some(generation) = generation else {
            self.generation = None;
            self.firmware = FirmwareIdentity::default();
            return Err(
                Diagnostic::new("MSI_ACPI session", DiagnosticKind::Unavailable)
                    .detail(format!("expected one active instance, observed {count}")),
            );
        };
        if self.generation != Some(generation) || !self.firmware.matches() {
            self.generation = Some(generation);
            self.firmware = FirmwareIdentity::default();
            if let Some(packet) = generation.record(report, Method::Wmi)? {
                self.firmware.wmi = Some((packet.bytes[1], packet.bytes[2]));
                report.wmi_version = self
                    .firmware
                    .wmi
                    .map(|(major, minor)| format!("{major}.{minor}"));
            }
            if let Some(packet) = generation.record(report, Method::Ec)? {
                self.firmware.ec = decode_ec_firmware(&packet.bytes);
                if self.firmware.ec.is_none() {
                    report.diagnostics.push(Diagnostic::invalid(
                        "Get_EC",
                        "unexpected EC version payload",
                    ));
                }
            }
        }
        report.ec_firmware.clone_from(&self.firmware.ec);
        report.wmi_version = self
            .firmware
            .wmi
            .map(|(major, minor)| format!("{major}.{minor}"));
        report.identity_matched = self.firmware.matches();
        if !report.identity_matched {
            return Err(
                Diagnostic::new("MSI_ACPI firmware identity", DiagnosticKind::Unsupported).detail(
                    "EC or WMI version differs from the audited baseline or is unavailable",
                ),
            );
        }
        if poll == Poll::Identity {
            return Ok(());
        }
        if poll == Poll::Snapshot {
            let (bytes, issues) = self.on_generation(generation.control_bytes())?;
            report.control_bytes = Some(bytes);
            report.diagnostics.extend(issues);
            return Ok(());
        }

        if let Some(p) = generation.record(report, Method::Temperature)? {
            report.telemetry.cpu_temperature_c = temperature(p.bytes[1]);
            report.telemetry.gpu_temperature_c = temperature(p.bytes[2]);
        }
        if let Some(p) = generation.record(report, Method::FanRpm)? {
            report.telemetry.cpu_fan_rpm = rpm(p.bytes[1], p.bytes[2]);
            report.telemetry.gpu_fan_rpm = rpm(p.bytes[3], p.bytes[4]);
        }
        if let Some(p) = generation.record(report, Method::FanCpu)? {
            report.telemetry.cpu_reported_duty_percent = duty(p.bytes[1]);
        }
        if let Some(p) = generation.record(report, Method::FanGpu)? {
            report.telemetry.gpu_reported_duty_percent = duty(p.bytes[1]);
        }
        if poll == Poll::Telemetry {
            return Ok(());
        }
        if let Some(p) = generation.record(report, Method::Thermal)? {
            report.cooler_boost_bit = Some(p.bytes[1] & 0x80 != 0);
        }
        if let Some(p) = generation.record(report, Method::Ap0)? {
            report.mux_ec_indicator = Some(p.bytes[2]);
        }
        generation.record(report, Method::Ap1)?;
        if let Some(p) = generation.record(report, Method::Device)? {
            report.mux_support_indicator = Some(p.bytes[2] & 0x40 != 0);
        }
        for &address in PROBE_ADDRESSES {
            generation.record(report, Method::Data(ReadAddress::check(address)?))?;
        }
        Ok(())
    }

    pub(crate) fn read_byte(&mut self, address: u8) -> Result<u8, Diagnostic> {
        let address = ReadAddress::check(address)?;
        let generation = self.verified_generation()?;
        self.on_generation(generation.read(Method::Data(address)))
            .map(|packet| packet.bytes[1])
    }

    pub(crate) fn write_byte(&mut self, permit: &AuthorizedEcWrite) -> Result<(), Diagnostic> {
        let request = WriteRequest::from_permit(permit)?;
        let generation = self.verified_generation()?;
        self.on_generation(
            executor()?.run("Set_Data", WRITE_BUDGET, move |worker, deadline| {
                worker.ec_method(generation, Method::SetData(request), deadline)
            }),
        )
        .map(|_| ())
    }

    fn verified_generation(&mut self) -> Result<Generation, Diagnostic> {
        if self.generation.is_none() || !self.firmware.matches() {
            let report = self.poll(Poll::Identity, 0);
            if !report.identity_matched {
                return Err(report
                    .methods
                    .into_iter()
                    .find_map(|row| row.error)
                    .or_else(|| report.diagnostics.into_iter().next())
                    .unwrap_or_else(|| {
                        Diagnostic::new("MSI_ACPI firmware identity", DiagnosticKind::Unavailable)
                    }));
            }
        }
        self.generation.ok_or_else(stale_session)
    }

    fn on_generation<T>(&mut self, result: Result<T, Diagnostic>) -> Result<T, Diagnostic> {
        match result {
            Ok(value) => Ok(value),
            Err(error) => {
                if error == stale_session() || error.kind == DiagnosticKind::TimedOut {
                    self.generation = None;
                }
                Err(error)
            }
        }
    }
}

impl FirmwareIdentity {
    fn matches(&self) -> bool {
        self.ec.as_deref() == Some(EC_FIRMWARE) && self.wmi == Some(WMI_VERSION)
    }
}

fn verify_dsdt() -> Result<(), Diagnostic> {
    const OP: &str = "GetSystemFirmwareTable(DSDT)";
    const ACPI: FIRMWARE_TABLE_PROVIDER = FIRMWARE_TABLE_PROVIDER(u32::from_be_bytes(*b"ACPI"));
    const DSDT: u32 = u32::from_le_bytes(*b"DSDT");
    let len = unsafe { GetSystemFirmwareTable(ACPI, DSDT, None) };
    if len == 0 {
        return Err(Diagnostic::last_win32(OP));
    }
    if !(36..=4 * 1024 * 1024).contains(&len) {
        return Err(Diagnostic::invalid(OP, "unexpected DSDT size"));
    }
    let mut table = vec![0u8; len as usize];
    let actual = unsafe { GetSystemFirmwareTable(ACPI, DSDT, Some(&mut table)) };
    if actual == 0 {
        return Err(Diagnostic::last_win32(OP));
    }
    if actual != len || table[..4] != *b"DSDT" || table[4..8] != len.to_le_bytes() {
        return Err(Diagnostic::invalid(OP, "DSDT header or length changed"));
    }
    if format!("{:x}", Sha256::digest(&table)) != DSDT_SHA256 {
        return Err(Diagnostic::new(OP, DiagnosticKind::Unsupported)
            .detail("DSDT SHA-256 differs from the audited AML"));
    }
    Ok(())
}

fn decode_ec_firmware(data: &[u8; PACKAGE_SIZE]) -> Option<String> {
    let build = &data[2..30];
    let dot = build.iter().position(|&b| b == b'.')?;
    if !(4..=16).contains(&dot)
        || !build[..dot]
            .iter()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
    {
        return None;
    }
    let end = dot + 4;
    let digits = build.get(dot + 1..end)?;
    if !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let tail = &build[end..];
    let date_suffix = tail.len() == 16
        && tail[..10].iter().all(u8::is_ascii_digit)
        && tail[10] == b':'
        && tail[11..13].iter().all(u8::is_ascii_digit)
        && tail[13] == b':'
        && tail[14..].iter().all(u8::is_ascii_digit);
    if !date_suffix && !tail.iter().all(|b| matches!(*b, 0 | b' ')) {
        return None;
    }
    String::from_utf8(build[..end].to_vec()).ok()
}

fn rpm(hi: u8, lo: u8) -> Option<u16> {
    let period = u32::from(u16::from_be_bytes([hi, lo]));
    match period {
        0 => Some(0),
        n => u16::try_from(480_000 / n).ok(),
    }
}
fn temperature(value: u8) -> Option<i16> {
    (1..=125).contains(&value).then_some(i16::from(value))
}
fn duty(value: u8) -> Option<u8> {
    (value <= 150).then_some(value)
}

impl Generation {
    fn control_bytes(self) -> Result<(ControlBytes, Vec<Diagnostic>), Diagnostic> {
        executor()?.run(
            "MSI_ACPI control snapshot",
            SNAPSHOT_BUDGET,
            move |worker, deadline| {
                let mut issues = Vec::new();
                let mut read = |address| {
                    let method = Method::Data(ReadAddress::check(address)?);
                    match worker.ec_method(self, method, deadline) {
                        Ok(packet) => Ok(Some(packet.bytes[1])),
                        Err(error)
                            if error.leaves_call_unsettled()
                                || error.kind == DiagnosticKind::AccessDenied =>
                        {
                            Err(error)
                        }
                        Err(error) => {
                            issues.push(error);
                            Ok(None)
                        }
                    }
                };
                let mut bytes = ControlBytes::default();
                for (address, byte) in ControlBytes::SCALAR_ADDRESSES
                    .into_iter()
                    .zip(&mut bytes.scalars)
                {
                    *byte = read(address)?;
                }
                for (base, region) in [
                    (ms17s3::CPU_TEMPERATURE, &mut bytes.cpu_temperature),
                    (ms17s3::CPU_DUTY, &mut bytes.cpu_duty),
                    (ms17s3::GPU_TEMPERATURE, &mut bytes.gpu_temperature),
                    (ms17s3::GPU_DUTY, &mut bytes.gpu_duty),
                ] {
                    for (i, byte) in region.iter_mut().enumerate() {
                        *byte = read(base + i as u8)?;
                    }
                }
                Ok((bytes, issues))
            },
        )
    }

    fn record(self, report: &mut AcpiReport, method: Method) -> Result<Option<Packet>, Diagnostic> {
        let start = Instant::now();
        let result = self.read(method);
        let mut row = AcpiMethodReport {
            method: method.name(),
            selector: method.selector(),
            elapsed_us: start.elapsed().as_micros() as u64,
            package_bytes: None,
            wmi_return_value: None,
            prefix: None,
            error: None,
        };
        let result = match result {
            Ok(packet) => {
                row.wmi_return_value = packet.return_value;
                row.package_bytes = Some(PACKAGE_SIZE);
                row.prefix = Some(packet.bytes[..method.prefix_len()].to_vec());
                Ok(Some(packet))
            }
            Err(error) => {
                let stop =
                    error.leaves_call_unsettled() || error.kind == DiagnosticKind::AccessDenied;
                row.error = Some(error.clone());
                if stop { Err(error) } else { Ok(None) }
            }
        };
        report.methods.push(row);
        result
    }

    fn read(self, method: Method) -> Result<Packet, Diagnostic> {
        executor()?.run(method.name(), READ_BUDGET, move |worker, deadline| {
            worker.ec_method(self, method, deadline)
        })
    }
}

fn brightness_executor() -> Result<&'static SerialExecutor<WmiWorker>, Diagnostic> {
    static EXECUTOR: OnceLock<Result<SerialExecutor<WmiWorker>, Diagnostic>> = OnceLock::new();
    EXECUTOR
        .get_or_init(|| SerialExecutor::spawn("msilt-wmi-brightness", WmiWorker::default))
        .as_ref()
        .map_err(Clone::clone)
}

fn executor() -> Result<&'static SerialExecutor<WmiWorker>, Diagnostic> {
    static EXECUTOR: OnceLock<Result<SerialExecutor<WmiWorker>, Diagnostic>> = OnceLock::new();
    EXECUTOR
        .get_or_init(|| SerialExecutor::spawn("msilt-wmi", WmiWorker::default))
        .as_ref()
        .map_err(Clone::clone)
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Brightness {
    pub current_percent: u8,
    pub levels: Vec<u8>,
}

pub(crate) fn brightness(key: String) -> Result<Brightness, Diagnostic> {
    brightness_executor()?.run(
        "WmiMonitorBrightness",
        BRIGHTNESS_BUDGET,
        move |worker, deadline| {
            let (_, row) = brightness_row(worker.services(deadline)?, &key, deadline)?;
            let current_percent = current_brightness(&row)?;
            let (levels, cim) = property(&row, w!("Level"), "WmiMonitorBrightness.Level")?;
            let mut levels = levels.bytes(cim, 1..=256, "WmiMonitorBrightness.Level")?;
            if levels.iter().any(|&level| level > 100) {
                return Err(Diagnostic::invalid(
                    "WmiMonitorBrightness.Level",
                    "percentage outside 0..=100",
                ));
            }
            if uint32(&row, w!("Levels"), "WmiMonitorBrightness.Levels")? as usize != levels.len() {
                return Err(Diagnostic::invalid(
                    "WmiMonitorBrightness.Levels",
                    "level count does not match the array",
                ));
            }
            levels.sort_unstable();
            levels.dedup();
            Ok(Brightness {
                current_percent,
                levels,
            })
        },
    )
}

const BRIGHTNESS_METHOD: &str = "WmiSetBrightness";

pub(crate) fn set_brightness(key: String, percent: u8) -> Result<u8, Diagnostic> {
    validate_brightness(percent)?;
    brightness_executor()?.run(
        BRIGHTNESS_METHOD,
        BRIGHTNESS_BUDGET,
        move |worker, deadline| worker.set_brightness(&key, percent, deadline),
    )
}

pub(crate) fn validate_brightness(percent: u8) -> Result<(), Diagnostic> {
    if percent > 100 {
        Err(Diagnostic::invalid(
            BRIGHTNESS_METHOD,
            "percentage outside 0..=100",
        ))
    } else {
        Ok(())
    }
}

fn brightness_row(
    services: &IWbemServices,
    key: &str,
    deadline: Instant,
) -> Result<(String, IWbemClassObject), Diagnostic> {
    monitor_row(
        services,
        key,
        "SELECT * FROM WmiMonitorBrightness",
        "WmiMonitorBrightness",
        deadline,
    )
}

fn monitor_row(
    services: &IWbemServices,
    key: &str,
    wql: &'static str,
    op: &'static str,
    deadline: Instant,
) -> Result<(String, IWbemClassObject), Diagnostic> {
    let mut found = None;
    for row in query(services, wql, deadline)? {
        if !boolean(&row, w!("Active"), op)? {
            continue;
        }
        let instance = String::from_utf16(&text(&row, w!("InstanceName"), op)?)
            .map_err(|_| Diagnostic::invalid(op, "invalid monitor instance name"))?;
        if matches_monitor(key, &instance) {
            if found.replace((instance, row)).is_some() {
                return Err(Diagnostic::invalid(
                    op,
                    "multiple active instances for the internal panel",
                ));
            }
        }
    }
    found.ok_or_else(|| {
        Diagnostic::new(op, DiagnosticKind::Unavailable)
            .detail("no active instance for the internal panel")
    })
}

fn current_brightness(row: &IWbemClassObject) -> Result<u8, Diagnostic> {
    const OP: &str = "WmiMonitorBrightness.CurrentBrightness";
    let value = uint8(row, w!("CurrentBrightness"), OP)?;
    if value > 100 {
        Err(Diagnostic::invalid(OP, "percentage outside 0..=100"))
    } else {
        Ok(value)
    }
}

fn same_monitor_instance(expected: &str, actual: &str) -> Result<(), Diagnostic> {
    if expected.eq_ignore_ascii_case(actual) {
        Ok(())
    } else {
        Err(
            Diagnostic::new(BRIGHTNESS_METHOD, DiagnosticKind::Unavailable)
                .detail("brightness query and method do not bind to the same instance"),
        )
    }
}

fn matches_monitor(key: &str, instance: &str) -> bool {
    instance
        .get(..key.len())
        .is_some_and(|p| p.eq_ignore_ascii_case(key))
        && instance.get(key.len()..).is_some_and(|tail| {
            tail.is_empty()
                || tail
                    .strip_prefix('_')
                    .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
        })
}

#[derive(Default)]
struct WmiWorker {
    acpi: Option<AcpiSession>,
    generation_counter: u64,
    services: Option<IWbemServices>,
    quarantined: bool,
    unsettled_ec: Option<NamedMutex>,
    apartment: Option<Apartment>,
}

struct Apartment;
impl Drop for Apartment {
    fn drop(&mut self) {
        unsafe { CoUninitialize() };
    }
}

fn init_security() -> Result<(), Diagnostic> {
    static STATUS: OnceLock<Result<(), Diagnostic>> = OnceLock::new();
    STATUS
        .get_or_init(|| {
            let result = unsafe {
                CoInitializeSecurity(
                    None,
                    -1,
                    None,
                    None,
                    RPC_C_AUTHN_LEVEL_DEFAULT,
                    RPC_C_IMP_LEVEL_IMPERSONATE,
                    None,
                    EOAC_NONE,
                    None,
                )
            };
            match result {
                Ok(()) => Ok(()),
                Err(e) if e.code() == RPC_E_TOO_LATE => Ok(()),
                Err(e) => Err(Diagnostic::windows("CoInitializeSecurity", e)),
            }
        })
        .clone()
}

impl WmiWorker {
    fn services(&mut self, deadline: Instant) -> Result<&IWbemServices, Diagnostic> {
        if self.services.is_none() {
            if self.apartment.is_none() {
                let status = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
                status
                    .ok()
                    .map_err(|e| Diagnostic::windows("CoInitializeEx", e))?;
                self.apartment = Some(Apartment);
            }
            init_security()?;
            let locator: IWbemLocator =
                unsafe { CoCreateInstance(&WbemLocator, None, CLSCTX_INPROC_SERVER) }
                    .map_err(|e| Diagnostic::windows("CoCreateInstance(WbemLocator)", e))?;
            before_deadline(deadline, "IWbemLocator::ConnectServer")?;
            let empty = BSTR::new();
            let services = unsafe {
                locator.ConnectServer(
                    &BSTR::from(r"ROOT\WMI"),
                    &empty,
                    &empty,
                    &empty,
                    WBEM_FLAG_CONNECT_USE_MAX_WAIT.0,
                    &empty,
                    None,
                )
            }
            .map_err(|e| Diagnostic::windows("IWbemLocator::ConnectServer(root\\WMI)", e))?;
            // RPC_C_AUTHN_WINNT / RPC_C_AUTHZ_NONE; the proxy never carries explicit credentials.
            unsafe {
                CoSetProxyBlanket(
                    &services,
                    10,
                    0,
                    PCWSTR::null(),
                    RPC_C_AUTHN_LEVEL_CALL,
                    RPC_C_IMP_LEVEL_IMPERSONATE,
                    None,
                    EOAC_NONE,
                )
            }
            .map_err(|e| Diagnostic::windows("CoSetProxyBlanket(WMI)", e))?;
            self.services = Some(services);
        }
        Ok(self.services.as_ref().expect("initialized WMI service"))
    }

    fn ensure_acpi(
        &mut self,
        refresh: bool,
        deadline: Instant,
    ) -> Result<(u32, Option<Generation>), Diagnostic> {
        if self.quarantined {
            return Err(quarantined("MSI_ACPI session"));
        }
        if let Some(session) = self.acpi.as_ref().filter(|_| !refresh) {
            return Ok((1, Some(session.generation)));
        }
        let _ownership = NamedMutex::msi_software("MSI_ACPI session")?;
        self.acpi = None;
        let services = self.services(deadline)?;
        let mut active = Vec::new();
        for row in query(services, "SELECT * FROM MSI_ACPI", deadline)? {
            if boolean(&row, w!("Active"), "MSI_ACPI.Active")? {
                text(&row, w!("InstanceName"), "MSI_ACPI.InstanceName")?;
                active.push(text(&row, w!("__RELPATH"), "MSI_ACPI.__RELPATH")?);
            }
        }
        if active.len() != 1 {
            return Ok((active.len() as u32, None));
        }
        let path = active.pop().expect("one WMI instance");
        let class = get_class(services, "MSI_ACPI", deadline)?;
        let package = get_class(services, "Package_32", deadline)?;
        self.generation_counter = self.generation_counter.checked_add(1).ok_or_else(|| {
            Diagnostic::invalid("MSI_ACPI session", "generation counter exhausted")
        })?;
        let generation = Generation(self.generation_counter);
        self.acpi = Some(AcpiSession {
            generation,
            path,
            class,
            package,
        });
        Ok((1, Some(generation)))
    }

    fn ec_method(
        &mut self,
        generation: Generation,
        method: Method,
        deadline: Instant,
    ) -> Result<Packet, Diagnostic> {
        if self.quarantined {
            return Err(quarantined(method.name()));
        }
        let ownership = NamedMutex::msi_software(method.name())?;
        let session = self
            .acpi
            .as_ref()
            .filter(|session| session.generation == generation)
            .ok_or_else(stale_session)?;
        let input = session.input(method)?;
        let path = session.path.clone();
        if matches!(method, Method::SetData(_)) {
            verify_ec_ownership()?;
        }
        let result = self.invoke_method(&path, method.name(), input.as_ref(), deadline);
        if self.quarantined {
            self.unsettled_ec = Some(ownership);
        }
        let output = result?.ok_or_else(|| {
            Diagnostic::invalid(method.name(), "WMI method returned no output object")
        })?;
        decode_packet(&output, method.name())
    }

    fn set_brightness(
        &mut self,
        key: &str,
        percent: u8,
        deadline: Instant,
    ) -> Result<u8, Diagnostic> {
        if self.quarantined {
            return Err(quarantined(BRIGHTNESS_METHOD));
        }
        let services = self.services(deadline)?;
        let (expected, _) = brightness_row(services, key, deadline)?;
        let (instance, row) = monitor_row(
            services,
            key,
            "SELECT * FROM WmiMonitorBrightnessMethods",
            BRIGHTNESS_METHOD,
            deadline,
        )?;
        same_monitor_instance(&expected, &instance)?;
        let path = text(&row, w!("__RELPATH"), BRIGHTNESS_METHOD)?;
        let class = get_class(services, "WmiMonitorBrightnessMethods", deadline)?;
        let input = method_input(&class, BRIGHTNESS_METHOD)?;
        let [timeout, brightness] = brightness_args(percent);
        unsafe {
            input
                .Put(w!("Timeout"), 0, &timeout.0, 0)
                .and_then(|()| input.Put(w!("Brightness"), 0, &brightness.0, 0))
        }
        .map_err(|e| Diagnostic::windows(BRIGHTNESS_METHOD, e))?;
        if let Some(output) = self.invoke_method(&path, BRIGHTNESS_METHOD, Some(&input), deadline)?
        {
            let mut status = OwnedVariant::default();
            let mut cim = 0;
            match unsafe {
                output.Get(w!("ReturnValue"), 0, &mut status.0, Some(&mut cim), None)
            } {
                Ok(()) if matches!(status.ty(), VT_EMPTY | VT_NULL) => {}
                Ok(()) => check_brightness_status(&status, cim)?,
                Err(e) if e.code().0 == WBEM_E_NOT_FOUND.0 => {}
                Err(e) => return Err(Diagnostic::windows(BRIGHTNESS_METHOD, e)),
            }
        }
        let mut observed = 0;
        for attempt in 0..4 {
            let (instance, row) = brightness_row(self.services(deadline)?, key, deadline)?;
            same_monitor_instance(&expected, &instance)?;
            observed = current_brightness(&row)?;
            if observed == percent || attempt == 3 {
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        Ok(observed)
    }

    fn invoke_method(
        &mut self,
        path: &BSTR,
        op: &'static str,
        input: Option<&IWbemClassObject>,
        deadline: Instant,
    ) -> Result<Option<IWbemClassObject>, Diagnostic> {
        before_deadline(deadline, op)?;
        let mut result = None;
        let services = self.services.as_ref().expect("connected WMI service");
        unsafe {
            services.ExecMethod(
                path,
                &BSTR::from(op),
                WBEM_FLAG_RETURN_IMMEDIATELY,
                None,
                input,
                None,
                Some(&mut result),
            )
        }
        .map_err(|e| Diagnostic::windows(op, e))?;
        let Some(call) = result else {
            self.quarantined = true;
            return Err(Diagnostic::new(op, DiagnosticKind::TimedOut)
                .detail("WMI did not return an operation completion handle"));
        };
        let mut status = 0;
        // The caller's deadline is external. Keep this slot until the provider actually completes.
        let wait =
            unsafe { (call.vtable().GetCallStatus)(call.as_raw(), WBEM_INFINITE, &mut status) };
        if wait != S_OK {
            self.quarantined = true;
            return Err(Diagnostic::native(
                op,
                DiagnosticKind::TimedOut,
                NativeDomain::Hresult,
                wait.0 as u32,
            )
            .detail("WMI completion could not be established"));
        }
        if status != 0 {
            return Err(Diagnostic::hresult(op, status));
        }
        let mut raw = ptr::null_mut();
        let hr = unsafe { (call.vtable().GetResultObject)(call.as_raw(), WBEM_INFINITE, &mut raw) };
        if hr.is_err() {
            return Err(Diagnostic::hresult(op, hr.0));
        }
        Ok((!raw.is_null()).then(|| unsafe { IWbemClassObject::from_raw(raw) }))
    }
}

fn check_brightness_status(status: &OwnedVariant, cim: i32) -> Result<(), Diagnostic> {
    if matches!(status.ty(), VT_EMPTY | VT_NULL) {
        return Ok(());
    }
    if cim == CIM_BOOLEAN.0 {
        // The local WmiProv schema exposes a Boolean, not the documented numeric status.
        // The provider's HRESULT and the monitor readback establish the result on this schema.
        return status.as_bool(cim, BRIGHTNESS_METHOD).map(|_| ());
    }
    let status = status.as_uint32(cim, BRIGHTNESS_METHOD)?;
    if status == 0 {
        Ok(())
    } else {
        Err(Diagnostic::native(
            BRIGHTNESS_METHOD,
            DiagnosticKind::Rejected,
            NativeDomain::WmiReturn,
            status,
        ))
    }
}

fn brightness_args(percent: u8) -> [OwnedVariant; 2] {
    [
        OwnedVariant::tagged(VT_I4, VARIANT_0_0_0 { lVal: 0 }),
        OwnedVariant::tagged(VT_UI1, VARIANT_0_0_0 { bVal: percent }),
    ]
}

pub fn verify_ec_ownership() -> Result<(), Diagnostic> {
    const OP: &str = "MSI_ACPI write ownership";
    let conflicts = identity::ec_conflicts(&mut Vec::new());
    if !conflicts.running_services.is_empty()
        || conflicts.running_processes.iter().any(|p| {
            matches!(
                *p,
                "MSI.CentralServer.exe"
                    | "MSI_Central_Service.exe"
                    | "MSIAPService.exe"
                    | "Sendevsvc.exe"
                    | "MSIService.exe"
                    | "OmApSvcBroker.exe"
                    | "MSI.Center.Server.exe"
                    | "MSI Center.exe"
                    | "gpuControl.exe"
            )
        })
    {
        return Err(Diagnostic::conflict(
            OP,
            "MSI Center EC consumers or brokers are active",
        ));
    }
    if !conflicts.process_enumeration_complete || !conflicts.service_query_complete {
        return Err(Diagnostic::new(OP, DiagnosticKind::Unavailable)
            .detail("could not verify OEM ownership"));
    }
    Ok(())
}

fn quarantined(op: &'static str) -> Diagnostic {
    Diagnostic::new(op, DiagnosticKind::Busy)
        .detail("the previous provider call has no confirmed completion")
}

fn stale_session() -> Diagnostic {
    Diagnostic::new("MSI_ACPI session", DiagnosticKind::Busy)
        .detail("ACPI session changed; refresh identity")
}

fn get_class(
    services: &IWbemServices,
    name: &'static str,
    deadline: Instant,
) -> Result<IWbemClassObject, Diagnostic> {
    before_deadline(deadline, name)?;
    let mut result = None;
    unsafe { services.GetObject(&BSTR::from(name), NO_FLAGS, None, Some(&mut result), None) }
        .map_err(|e| Diagnostic::windows(name, e))?;
    result.ok_or_else(|| Diagnostic::invalid(name, "class object missing"))
}

fn query(
    services: &IWbemServices,
    wql: &'static str,
    deadline: Instant,
) -> Result<Vec<IWbemClassObject>, Diagnostic> {
    before_deadline(deadline, wql)?;
    let flags = WBEM_GENERIC_FLAG_TYPE(WBEM_FLAG_FORWARD_ONLY.0 | WBEM_FLAG_RETURN_IMMEDIATELY.0);
    let rows = unsafe { services.ExecQuery(&BSTR::from("WQL"), &BSTR::from(wql), flags, None) }
        .map_err(|e| Diagnostic::windows(wql, e))?;
    let mut result = Vec::new();
    for _ in 0..=64 {
        before_deadline(deadline, wql)?;
        let timeout = deadline
            .saturating_duration_since(Instant::now())
            .as_millis()
            .clamp(1, i32::MAX as u128) as i32;
        let mut got = 0;
        let mut row = [None];
        let status = unsafe { rows.Next(timeout, &mut row, &mut got) };
        match (status.0, got, row[0].take()) {
            (0, 1, Some(object)) if result.len() < 64 => result.push(object),
            (s, 0, None) if s == WBEM_S_FALSE.0 => return Ok(result),
            (s, _, _) if s < 0 || s == WBEM_S_TIMEDOUT.0 => {
                return Err(Diagnostic::hresult(wql, s));
            }
            _ => {
                return Err(Diagnostic::invalid(
                    wql,
                    "unexpected enumerator result or too many instances",
                ));
            }
        }
    }
    unreachable!()
}

struct AcpiSession {
    generation: Generation,
    path: BSTR,
    class: IWbemClassObject,
    package: IWbemClassObject,
}

impl AcpiSession {
    fn input(&self, method: Method) -> Result<Option<IWbemClassObject>, Diagnostic> {
        let Some(data) = method.input() else {
            return Ok(None);
        };
        let op = method.name();
        let params = method_input(&self.class, op)?;
        let package =
            unsafe { self.package.SpawnInstance(0) }.map_err(|e| Diagnostic::windows(op, e))?;
        let bytes = OwnedVariant::from_bytes(&data, op)?;
        unsafe { package.Put(w!("Bytes"), 0, &bytes.0, 0) }
            .map_err(|e| Diagnostic::windows(op, e))?;
        let data =
            OwnedVariant::from_object(package.cast().map_err(|e| Diagnostic::windows(op, e))?);
        unsafe { params.Put(w!("Data"), 0, &data.0, 0) }.map_err(|e| Diagnostic::windows(op, e))?;
        Ok(Some(params))
    }
}

fn method_input(
    class: &IWbemClassObject,
    op: &'static str,
) -> Result<IWbemClassObject, Diagnostic> {
    let mut signature = None;
    let name: Vec<u16> = op.encode_utf16().chain([0]).collect();
    unsafe { class.GetMethod(PCWSTR(name.as_ptr()), 0, &mut signature, ptr::null_mut()) }
        .map_err(|e| Diagnostic::windows(op, e))?;
    let signature = signature.ok_or_else(|| Diagnostic::invalid(op, "input signature missing"))?;
    unsafe { signature.SpawnInstance(0) }.map_err(|e| Diagnostic::windows(op, e))
}

fn decode_packet(out: &IWbemClassObject, op: &'static str) -> Result<Packet, Diagnostic> {
    let mut ret = OwnedVariant::default();
    let mut ty = 0;
    let ret_value = match unsafe { out.Get(w!("ReturnValue"), 0, &mut ret.0, Some(&mut ty), None) }
    {
        Ok(()) if matches!(ret.ty(), VT_EMPTY | VT_NULL) => None,
        Ok(()) => Some(ret.as_bool(ty, op)?),
        Err(e) if e.code().0 == WBEM_E_NOT_FOUND.0 => None,
        Err(e) => return Err(Diagnostic::windows(op, e)),
    };
    let (data, cim) = property(out, w!("Data"), op)?;
    if cim != CIM_OBJECT.0 || data.ty() != VT_UNKNOWN {
        return Err(Diagnostic::invalid(
            op,
            "Data is not a CIM_OBJECT/VT_UNKNOWN",
        ));
    }
    let obj = unsafe { &*data.parts().Anonymous.punkVal }
        .as_ref()
        .ok_or_else(|| Diagnostic::invalid(op, "Data object is null"))?;
    let obj: IWbemClassObject = obj.cast().map_err(|e| Diagnostic::windows(op, e))?;
    if !String::from_utf16_lossy(&text(&obj, w!("__CLASS"), op)?).eq_ignore_ascii_case("Package_32")
    {
        return Err(Diagnostic::invalid(op, "Data is not a Package_32 instance"));
    }
    let (bytes, cim) = property(&obj, w!("Bytes"), op)?;
    let bytes = bytes.bytes(cim, PACKAGE_SIZE..=PACKAGE_SIZE, op)?;
    Packet::check(&bytes, ret_value, op)
}

fn property(
    object: &IWbemClassObject,
    prop: PCWSTR,
    op: &'static str,
) -> Result<(OwnedVariant, i32), Diagnostic> {
    let mut variant = OwnedVariant::default();
    let mut ty = 0;
    unsafe { object.Get(prop, 0, &mut variant.0, Some(&mut ty), None) }
        .map_err(|e| Diagnostic::windows(op, e))?;
    Ok((variant, ty))
}

fn boolean(object: &IWbemClassObject, prop: PCWSTR, op: &'static str) -> Result<bool, Diagnostic> {
    let (value, ty) = property(object, prop, op)?;
    value.as_bool(ty, op)
}

fn uint8(object: &IWbemClassObject, prop: PCWSTR, op: &'static str) -> Result<u8, Diagnostic> {
    let (value, ty) = property(object, prop, op)?;
    if ty == CIM_UINT8.0 && value.ty() == VT_UI1 {
        Ok(unsafe { value.parts().Anonymous.bVal })
    } else {
        Err(Diagnostic::invalid(op, "expected CIM_UINT8/VT_UI1"))
    }
}

fn uint32(object: &IWbemClassObject, prop: PCWSTR, op: &'static str) -> Result<u32, Diagnostic> {
    let (value, ty) = property(object, prop, op)?;
    value.as_uint32(ty, op)
}

fn text(object: &IWbemClassObject, prop: PCWSTR, op: &'static str) -> Result<BSTR, Diagnostic> {
    let (value, ty) = property(object, prop, op)?;
    if ty != CIM_STRING.0 || value.ty() != VT_BSTR {
        return Err(Diagnostic::invalid(op, "expected CIM_STRING/VT_BSTR"));
    }
    let text = unsafe { &*value.parts().Anonymous.bstrVal };
    if text.is_empty() || text.len() > 1024 || text.contains(&0) {
        return Err(Diagnostic::invalid(
            op,
            "invalid WMI string length or contents",
        ));
    }
    Ok(text.clone())
}

#[derive(Default)]
struct OwnedVariant(VARIANT);

impl Drop for OwnedVariant {
    fn drop(&mut self) {
        let _ = unsafe { VariantClear(&mut self.0) };
    }
}

impl OwnedVariant {
    fn parts(&self) -> &VARIANT_0_0 {
        unsafe { &self.0.Anonymous.Anonymous }
    }
    fn ty(&self) -> VARENUM {
        self.parts().vt
    }

    fn tagged(vt: VARENUM, value: VARIANT_0_0_0) -> Self {
        Self(VARIANT {
            Anonymous: VARIANT_0 {
                Anonymous: ManuallyDrop::new(VARIANT_0_0 {
                    vt,
                    Anonymous: value,
                    ..Default::default()
                }),
            },
        })
    }

    fn from_object(object: IUnknown) -> Self {
        Self::tagged(
            VT_UNKNOWN,
            VARIANT_0_0_0 {
                punkVal: ManuallyDrop::new(Some(object)),
            },
        )
    }

    fn from_bytes(data: &[u8; PACKAGE_SIZE], op: &'static str) -> Result<Self, Diagnostic> {
        let array = unsafe { SafeArrayCreateVector(VT_UI1, 0, PACKAGE_SIZE as u32) };
        if array.is_null() {
            return Err(
                Diagnostic::new(op, DiagnosticKind::Io).detail("SAFEARRAY allocation failed")
            );
        }
        let variant = Self::tagged(ARRAY_UI1, VARIANT_0_0_0 { parray: array });
        with_array(array, op, |p| unsafe {
            ptr::copy_nonoverlapping(data.as_ptr(), p, data.len())
        })?;
        Ok(variant)
    }

    fn as_bool(&self, ty: i32, op: &'static str) -> Result<bool, Diagnostic> {
        if ty == CIM_BOOLEAN.0 && self.ty() == VT_BOOL {
            match unsafe { self.parts().Anonymous.boolVal.0 } {
                0 => return Ok(false),
                -1 => return Ok(true),
                _ => (),
            }
        }
        Err(Diagnostic::invalid(op, "expected CIM_BOOLEAN/VT_BOOL"))
    }

    fn as_uint32(&self, ty: i32, op: &'static str) -> Result<u32, Diagnostic> {
        if ty == CIM_UINT32.0 && self.ty() == VT_I4 {
            Ok(unsafe { self.parts().Anonymous.lVal } as u32)
        } else {
            Err(Diagnostic::invalid(op, "expected CIM_UINT32/VT_I4"))
        }
    }

    fn bytes(
        &self,
        cim: i32,
        expected: std::ops::RangeInclusive<usize>,
        op: &'static str,
    ) -> Result<Vec<u8>, Diagnostic> {
        if cim != CIM_UINT8.0 | CIM_FLAG_ARRAY.0 || self.ty() != ARRAY_UI1 {
            return Err(Diagnostic::invalid(
                op,
                "expected CIM_UINT8 array/VT_ARRAY | VT_UI1",
            ));
        }
        let array = unsafe { self.parts().Anonymous.parray };
        if array.is_null()
            || unsafe { SafeArrayGetDim(array) } != 1
            || unsafe { SafeArrayGetElemsize(array) } != 1
        {
            return Err(Diagnostic::invalid(
                op,
                "expected a one-dimensional byte SAFEARRAY",
            ));
        }
        let dim = unsafe { SafeArrayGetVartype(array) }.map_err(|e| Diagnostic::windows(op, e))?;
        let low =
            unsafe { SafeArrayGetLBound(array, 1) }.map_err(|e| Diagnostic::windows(op, e))?;
        let high =
            unsafe { SafeArrayGetUBound(array, 1) }.map_err(|e| Diagnostic::windows(op, e))?;
        let len = usize::try_from(i64::from(high) - i64::from(low) + 1).unwrap_or(0);
        if low != 0 || dim != VT_UI1 || !expected.contains(&len) {
            return Err(Diagnostic::invalid(
                op,
                format!("unexpected SAFEARRAY type or bounds ({low}..={high}; {len} bytes)"),
            ));
        }
        let mut result = vec![0; len];
        with_array(array, op, |p| unsafe {
            ptr::copy_nonoverlapping(p, result.as_mut_ptr(), len)
        })?;
        Ok(result)
    }
}

fn with_array(
    array: *mut SAFEARRAY,
    op: &'static str,
    access: impl FnOnce(*mut u8),
) -> Result<(), Diagnostic> {
    let mut data = ptr::null_mut();
    unsafe { SafeArrayAccessData(array, &mut data) }.map_err(|e| Diagnostic::windows(op, e))?;
    if !data.is_null() {
        access(data.cast());
    }
    unsafe { SafeArrayUnaccessData(array) }.map_err(|e| Diagnostic::windows(op, e))?;
    if data.is_null() {
        return Err(Diagnostic::invalid(op, "SAFEARRAY data pointer is null"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn package_abi_checks_size_type_and_entire_status() {
        let data = Method::Data(ReadAddress::check(0xD1).unwrap())
            .input()
            .unwrap();
        assert_eq!(data[0], 0xD1);
        assert!(data[1..].iter().all(|&b| b == 0));
        assert!(Method::Ec.input().is_none());
        assert!(ReadAddress::check(0xD8).is_err());
        let mut response = [0; 32];
        response[0] = 0x81;
        assert_eq!(
            Packet::check(&response, None, "Get_Data")
                .err()
                .unwrap()
                .kind,
            DiagnosticKind::Rejected
        );
        response[0] = 1;
        assert!(Packet::check(&response[..31], None, "Get_Data").is_err());
        let variant = OwnedVariant::from_bytes(&response, "test").unwrap();
        assert_eq!(
            variant
                .bytes(CIM_UINT8.0 | CIM_FLAG_ARRAY.0, 32..=32, "test")
                .unwrap(),
            response
        );
        assert!(variant.bytes(CIM_UINT8.0, 32..=32, "test").is_err());
        assert!(
            variant
                .bytes(CIM_UINT8.0 | CIM_FLAG_ARRAY.0, 31..=31, "test")
                .is_err()
        );
    }

    #[test]
    fn exact_firmware_and_sensor_values() {
        let mut data = [0; 32];
        data[2..30].copy_from_slice(b"17S3EMS1.1071013202514:29:49");
        assert_eq!(decode_ec_firmware(&data).as_deref(), Some(EC_FIRMWARE));
        data[18] = 0xFF;
        assert!(decode_ec_firmware(&data).is_none());
        assert_eq!(
            (rpm(0, 0x7B), rpm(0, 0xAB), rpm(0, 0), rpm(0, 1)),
            (Some(3902), Some(2807), Some(0), None)
        );
        assert_eq!(
            (temperature(0), temperature(125), duty(0), duty(151)),
            (None, Some(125), Some(0), None)
        );
        assert!(matches_monitor(
            r"DISPLAY\AUO84A6\PANEL",
            r"display\Auo84a6\panel_0"
        ));
        assert!(!matches_monitor(
            r"DISPLAY\AUO84A6\PANEL",
            r"display\Auo84a6\panel2_0"
        ));
    }

    #[test]
    fn set_data_has_a_token_gated_address_value_packet() {
        use msilt_core::{
            EcMap, EcRegisterIo, EcWrite, EcWriteIntent, HalError, HalResult, HardwareIdentity,
            WriteGuard,
        };

        struct Wire([u8; PACKAGE_SIZE]);
        impl EcRegisterIo for Wire {
            fn read_byte(&mut self, _: u8) -> HalResult<u8> {
                Ok(self.0[1])
            }
            fn write_byte(&mut self, permit: &AuthorizedEcWrite) -> HalResult<()> {
                let request = WriteRequest::from_permit(permit).map_err(|e| HalError::from(&e))?;
                self.0 = Method::SetData(request).input().unwrap();
                Ok(())
            }
        }
        let map = EcMap::parse_json(
            br#"{
            "schemaVersion":1,"boardId":"MS-17S3","firmwarePrefixes":["17S3EMS1.107"],
            "writeAllowlist":[{"address":"0xD7","values":[188,208]}]
        }"#,
        )
        .unwrap();
        let identity = HardwareIdentity {
            board_id: Some(BOARD_ID.into()),
            ec_firmware: Some(EC_FIRMWARE.into()),
        };
        let mut io = Wire([0; PACKAGE_SIZE]);
        io.0[1] = 188;
        WriteGuard::new(&map, &identity)
            .execute(
                &mut io,
                EcWriteIntent::Register(EcWrite {
                    address: 0xD7,
                    value: 208,
                }),
            )
            .unwrap();
        assert_eq!(&io.0[..2], &[0xD7, 208]);
        assert_eq!(&io.0[2..], &[0; 30]);
        for unsuccessful in [0, 2, 0x81, 0xff] {
            let mut response = [0; PACKAGE_SIZE];
            response[0] = unsuccessful;
            assert_eq!(
                Packet::check(&response, None, "Set_Data")
                    .err()
                    .unwrap()
                    .kind,
                DiagnosticKind::Rejected
            );
        }
    }

    #[test]
    fn brightness_validates_arguments_provider_result_and_exact_instance() {
        for valid in [0, 1, 100] {
            assert!(validate_brightness(valid).is_ok());
        }
        assert_eq!(
            validate_brightness(101).unwrap_err().kind,
            DiagnosticKind::InvalidData
        );
        let [timeout, percent] = brightness_args(81);
        assert_eq!(timeout.ty(), VT_I4);
        assert_eq!(unsafe { timeout.parts().Anonymous.lVal }, 0);
        assert_eq!(percent.ty(), VT_UI1);
        assert_eq!(unsafe { percent.parts().Anonymous.bVal }, 81);
        let empty = OwnedVariant::default();
        let null = OwnedVariant::tagged(VT_NULL, VARIANT_0_0_0::default());
        assert!(check_brightness_status(&empty, CIM_BOOLEAN.0).is_ok());
        assert!(check_brightness_status(&null, CIM_BOOLEAN.0).is_ok());
        let numeric = |l_val| OwnedVariant::tagged(VT_I4, VARIANT_0_0_0 { lVal: l_val });
        let boolean = |value| {
            OwnedVariant::tagged(
                VT_BOOL,
                VARIANT_0_0_0 {
                    boolVal: windows::Win32::Foundation::VARIANT_BOOL(value),
                },
            )
        };
        assert!(check_brightness_status(&numeric(0), CIM_UINT32.0).is_ok());
        assert_eq!(
            check_brightness_status(&numeric(5), CIM_UINT32.0)
                .unwrap_err()
                .native_status
                .unwrap()
                .code,
            5
        );
        assert!(check_brightness_status(&boolean(0), CIM_BOOLEAN.0).is_ok());
        assert!(check_brightness_status(&boolean(-1), CIM_BOOLEAN.0).is_ok());
        assert!(check_brightness_status(&boolean(1), CIM_BOOLEAN.0).is_err());
        assert!(check_brightness_status(&numeric(0), CIM_BOOLEAN.0).is_err());
        assert!(same_monitor_instance("instance_0", "Instance_0").is_ok());
        assert!(same_monitor_instance("instance_0", "instance_1").is_err());
    }
}
