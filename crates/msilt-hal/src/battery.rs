use std::mem::size_of;
use std::time::Instant;

use msilt_core::PowerSource;
use serde::Serialize;
use windows::Win32::Foundation::{
    ERROR_FILE_NOT_FOUND, ERROR_NO_SUCH_DEVICE, GENERIC_READ, HANDLE,
};
use windows::Win32::System::IO::DeviceIoControl;
use windows::Win32::System::Power::*;

use crate::executor::before_deadline;
use crate::native::{OwnedHandle, device_interfaces};
use crate::{Diagnostic, DiagnosticKind, NativeDomain};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum CapacityUnit {
    MilliWattHours,
    Relative,
}

#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BatteryReport {
    pub power_source: Option<PowerSource>,
    pub system_percent: Option<u8>,
    pub packs: Vec<BatteryPack>,
    pub enumeration_complete: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BatteryPack {
    pub capacity_unit: CapacityUnit,
    pub designed_capacity: Option<u32>,
    pub full_charged_capacity: Option<u32>,
    pub remaining_capacity: Option<u32>,
    pub health_percent: Option<f64>,
    pub percent: Option<f64>,
    pub cycle_count: Option<u32>,
    pub cycle_count_raw: u32,
    pub voltage_mv: Option<u32>,
    pub current_ma: Option<f64>,
    pub power_w: Option<f64>,
    pub relative_rate: Option<i32>,
    pub power_source: Option<PowerSource>,
    pub charging: Option<bool>,
    pub discharging: Option<bool>,
}

pub(crate) fn inspect(deadline: Instant, issues: &mut Vec<Diagnostic>) -> BatteryReport {
    let mut report = BatteryReport::default();
    let mut system = SYSTEM_POWER_STATUS::default();
    match unsafe { GetSystemPowerStatus(&mut system) } {
        Ok(()) => {
            report.power_source = match system.ACLineStatus {
                0 => Some(PowerSource::Battery),
                1 => Some(PowerSource::Ac),
                _ => None,
            };
            report.system_percent = (system.BatteryFlag & 0x80 == 0
                && system.BatteryLifePercent <= 100)
                .then_some(system.BatteryLifePercent);
        }
        Err(e) => issues.push(Diagnostic::windows("GetSystemPowerStatus", e)),
    }
    let mut incomplete = false;
    let enumeration = device_interfaces(&GUID_DEVICE_BATTERY, deadline, |path| {
        let result = path.and_then(|path| read_pack(&path, deadline));
        match result {
            Ok((pack, error)) => {
                if let Some(pack) = pack {
                    report.packs.push(pack);
                }
                if let Some(error) = error {
                    issues.push(error);
                    incomplete = true;
                }
            }
            Err(error) => {
                issues.push(error);
                incomplete = true;
            }
        }
    });
    if let Err(error) = enumeration {
        issues.push(error);
        incomplete = true;
    }
    report.enumeration_complete = !incomplete;
    report
}

type PackRead = (Option<BatteryPack>, Option<Diagnostic>);

fn read_pack(path: &[u16], deadline: Instant) -> Result<PackRead, Diagnostic> {
    before_deadline(deadline, "CreateFileW(battery)")?;
    let handle = OwnedHandle::open_device(path, GENERIC_READ.0)
        .map_err(|e| Diagnostic::windows("CreateFileW(battery, GENERIC_READ)", e))?;
    for retry in [false, true] {
        let tag = BatteryTag::query(handle.0, deadline)?;
        let info = tag.information(handle.0, deadline);
        let info = match info {
            Ok(info) => info,
            Err(error) if !retry && tag_lost(&error) => continue,
            Err(error) => return Err(error),
        };
        if info.Capabilities & BATTERY_SYSTEM_BATTERY == 0
            || info.Capabilities & BATTERY_IS_SHORT_TERM != 0
        {
            return Ok((None, None));
        }
        return match tag.status(handle.0, deadline) {
            Ok(status) => Ok((Some(decode(&info, Some(&status))), None)),
            Err(error) if !retry && tag_lost(&error) => continue,
            Err(error) => Ok((Some(decode(&info, None)), Some(error))),
        };
    }
    unreachable!()
}

fn tag_lost(error: &Diagnostic) -> bool {
    error.native_status.is_some_and(|status| {
        status.domain == NativeDomain::Hresult
            && [ERROR_FILE_NOT_FOUND, ERROR_NO_SUCH_DEVICE]
                .iter()
                .any(|code| code.to_hresult().0 as u32 == status.code)
    })
}

struct BatteryTag(u32);

impl BatteryTag {
    fn query(handle: HANDLE, deadline: Instant) -> Result<Self, Diagnostic> {
        const OP: &str = "IOCTL_BATTERY_QUERY_TAG";
        let tag = query::<u32, u32>(handle, IOCTL_BATTERY_QUERY_TAG, &0, deadline, OP)?;
        if tag == BATTERY_TAG_INVALID {
            return Err(
                Diagnostic::new(OP, DiagnosticKind::Unavailable).detail("no present battery tag")
            );
        }
        Ok(Self(tag))
    }

    fn information(
        &self,
        handle: HANDLE,
        deadline: Instant,
    ) -> Result<BATTERY_INFORMATION, Diagnostic> {
        let input = BATTERY_QUERY_INFORMATION {
            BatteryTag: self.0,
            InformationLevel: BatteryInformation,
            AtRate: 0,
        };
        query(
            handle,
            IOCTL_BATTERY_QUERY_INFORMATION,
            &input,
            deadline,
            "IOCTL_BATTERY_QUERY_INFORMATION",
        )
    }

    fn status(&self, handle: HANDLE, deadline: Instant) -> Result<BATTERY_STATUS, Diagnostic> {
        let input = BATTERY_WAIT_STATUS {
            BatteryTag: self.0,
            Timeout: 0,
            ..Default::default()
        };
        query(
            handle,
            IOCTL_BATTERY_QUERY_STATUS,
            &input,
            deadline,
            "IOCTL_BATTERY_QUERY_STATUS",
        )
    }
}

fn query<I, O: Default>(
    handle: HANDLE,
    code: u32,
    input: &I,
    deadline: Instant,
    operation: &'static str,
) -> Result<O, Diagnostic> {
    before_deadline(deadline, operation)?;
    let mut output = O::default();
    let mut returned = 0;
    // Only the three query IOCTLs above reach this helper, with integer-only SDK layouts.
    unsafe {
        DeviceIoControl(
            handle,
            code,
            Some((input as *const I).cast()),
            size_of::<I>() as u32,
            Some((&mut output as *mut O).cast()),
            size_of::<O>() as u32,
            Some(&mut returned),
            None,
        )
    }
    .map_err(|e| Diagnostic::windows(operation, e))?;
    if returned as usize != size_of::<O>() {
        return Err(Diagnostic::invalid(
            operation,
            format!("returned {returned} bytes; expected {}", size_of::<O>()),
        ));
    }
    Ok(output)
}

fn decode(info: &BATTERY_INFORMATION, status: Option<&BATTERY_STATUS>) -> BatteryPack {
    let absolute = info.Capabilities & BATTERY_CAPACITY_RELATIVE == 0;
    let capacity = |n| (n != BATTERY_UNKNOWN_CAPACITY).then_some(n);
    let designed = capacity(info.DesignedCapacity);
    let full = capacity(info.FullChargedCapacity);
    let remaining = status.and_then(|s| capacity(s.Capacity));
    let percent = |value: Option<u32>, total: Option<u32>| {
        value
            .zip(total.filter(|n| *n != 0))
            .map(|(v, total)| f64::from(v) * 100.0 / f64::from(total))
    };
    let voltage = status
        .map(|s| s.Voltage)
        .filter(|&v| v != BATTERY_UNKNOWN_VOLTAGE && v != 0);
    let rate = status
        .map(|s| s.Rate)
        .filter(|&rate| rate as u32 != BATTERY_UNKNOWN_RATE);
    let milliwatts = rate.filter(|_| absolute);
    let flags = status.map(|s| s.PowerState);
    BatteryPack {
        capacity_unit: if absolute {
            CapacityUnit::MilliWattHours
        } else {
            CapacityUnit::Relative
        },
        designed_capacity: designed,
        full_charged_capacity: full,
        remaining_capacity: remaining,
        health_percent: percent(full, designed),
        percent: percent(remaining, full).filter(|n| *n <= 100.0),
        cycle_count: (info.CycleCount != 0 && info.CycleCount != u32::MAX)
            .then_some(info.CycleCount),
        cycle_count_raw: info.CycleCount,
        voltage_mv: voltage,
        current_ma: milliwatts
            .zip(voltage)
            .map(|(mw, mv)| f64::from(mw) * 1000.0 / f64::from(mv)),
        power_w: milliwatts.map(|mw| f64::from(mw) / 1000.0),
        relative_rate: rate.filter(|_| !absolute),
        power_source: flags.map(|f| {
            if f & BATTERY_POWER_ON_LINE != 0 {
                PowerSource::Ac
            } else {
                PowerSource::Battery
            }
        }),
        charging: flags.map(|f| f & BATTERY_CHARGING != 0),
        discharging: flags.map(|f| f & BATTERY_DISCHARGING != 0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn actual_capacity_and_signed_rate_use_the_correct_units() {
        let info = BATTERY_INFORMATION {
            DesignedCapacity: 80_000,
            FullChargedCapacity: 60_000,
            CycleCount: 16,
            ..Default::default()
        };
        let status = BATTERY_STATUS {
            Capacity: 30_000,
            Voltage: 12_000,
            Rate: -24_000,
            PowerState: BATTERY_DISCHARGING,
        };
        let pack = decode(&info, Some(&status));
        assert_eq!(pack.health_percent, Some(75.0));
        assert_eq!(pack.percent, Some(50.0));
        assert_eq!(pack.cycle_count, Some(16));
        assert_eq!(pack.cycle_count_raw, 16);
        assert_eq!(pack.current_ma, Some(-2000.0));
        assert_eq!(pack.power_w, Some(-24.0));
        assert_eq!(pack.power_source, Some(PowerSource::Battery));
    }

    #[test]
    fn relative_and_unknown_values_never_invent_watts_or_current() {
        let info = BATTERY_INFORMATION {
            Capabilities: BATTERY_CAPACITY_RELATIVE,
            DesignedCapacity: BATTERY_UNKNOWN_CAPACITY,
            ..Default::default()
        };
        let status = BATTERY_STATUS {
            Capacity: BATTERY_UNKNOWN_CAPACITY,
            Voltage: BATTERY_UNKNOWN_VOLTAGE,
            Rate: -7,
            PowerState: BATTERY_POWER_ON_LINE,
        };
        let p = decode(&info, Some(&status));
        assert_eq!(p.capacity_unit, CapacityUnit::Relative);
        assert_eq!(p.relative_rate, Some(-7));
        assert_eq!(
            (
                p.designed_capacity,
                p.voltage_mv,
                p.cycle_count,
                p.percent,
                p.power_w,
                p.current_ma
            ),
            (None, None, None, None, None, None)
        );
        assert_eq!(p.power_source, Some(PowerSource::Ac));
        assert_eq!(p.cycle_count_raw, 0);
        assert_eq!(decode(&info, None).power_source, None);
    }
}
