use std::mem::size_of;

use msilt_core::TelemetrySample;
use windows::Win32::Foundation::FILETIME;
use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
use windows::Win32::System::Threading::GetSystemTimes;

use crate::Diagnostic;

#[derive(Default)]
pub(crate) struct SystemTelemetry {
    previous: Option<(u64, u64, u64)>,
}

impl SystemTelemetry {
    pub(crate) fn sample(&mut self, sample: &mut TelemetrySample, issues: &mut Vec<Diagnostic>) {
        let (mut idle, mut kernel, mut user) = (FILETIME::default(), FILETIME::default(), FILETIME::default());
        match unsafe { GetSystemTimes(Some(&mut idle), Some(&mut kernel), Some(&mut user)) } {
            Ok(()) => {
                let ticks = |t: FILETIME| (u64::from(t.dwHighDateTime) << 32) | u64::from(t.dwLowDateTime);
                let current = (ticks(idle), ticks(kernel), ticks(user));
                sample.cpu_utilization_percent = self.previous.and_then(|previous| cpu_percent(previous, current));
                self.previous = Some(current);
            }
            Err(error) => {
                self.previous = None;
                issues.push(Diagnostic::windows("GetSystemTimes", error));
            }
        }
        let mut memory = MEMORYSTATUSEX { dwLength: size_of::<MEMORYSTATUSEX>() as u32, ..Default::default() };
        match unsafe { GlobalMemoryStatusEx(&mut memory) } {
            Ok(()) => {
                if let Some((used, total)) = memory_mb(memory.ullTotalPhys, memory.ullAvailPhys) {
                    sample.memory_used_mb = Some(used);
                    sample.memory_total_mb = Some(total);
                } else {
                    issues.push(Diagnostic::invalid("GlobalMemoryStatusEx", "invalid physical memory capacity"));
                }
            }
            Err(error) => issues.push(Diagnostic::windows("GlobalMemoryStatusEx", error)),
        }
    }
}

fn cpu_percent(previous: (u64, u64, u64), current: (u64, u64, u64)) -> Option<u8> {
    let idle = current.0.checked_sub(previous.0)?;
    // GetSystemTimes includes idle time in the kernel counter.
    let total = current.1.checked_sub(previous.1)?.checked_add(current.2.checked_sub(previous.2)?)?;
    if total == 0 || idle > total {
        return None;
    }
    Some(((total - idle).saturating_mul(100).saturating_add(total / 2) / total).min(100) as u8)
}

fn memory_mb(total: u64, available: u64) -> Option<(u32, u32)> {
    Some((
        u32::try_from(total.checked_sub(available)? / (1024 * 1024)).ok()?,
        u32::try_from(total / (1024 * 1024)).ok()?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_uses_deltas_with_idle_already_in_kernel() {
        assert_eq!(cpu_percent((100, 200, 300), (160, 280, 320)), Some(40));
        assert_eq!(cpu_percent((0, 0, 0), (100, 100, 0)), Some(0));
        assert_eq!(cpu_percent((0, 0, 0), (0, 100, 100)), Some(100));
        assert_eq!(cpu_percent((0, 0, 0), (2, 3, 0)), Some(33));
        assert_eq!(cpu_percent((100, 200, 300), (100, 200, 300)), None);
        assert_eq!(cpu_percent((100, 200, 300), (99, 300, 400)), None);
        assert_eq!(cpu_percent((0, 0, 0), (101, 100, 0)), None);
    }

    #[test]
    fn memory_is_physical_used_and_total_in_binary_megabytes() {
        assert_eq!(memory_mb(64 << 30, 16 << 30), Some((49_152, 65_536)));
        assert_eq!(memory_mb(100, 101), None);
        assert_eq!(memory_mb(u64::MAX, 0), None);
    }

    #[test]
    fn first_cpu_read_has_no_fabricated_zero() {
        let mut sampler = SystemTelemetry::default();
        let mut sample = TelemetrySample::default();
        let mut issues = Vec::new();
        sampler.sample(&mut sample, &mut issues);
        assert!(issues.is_empty());
        assert_eq!(sample.cpu_utilization_percent, None);
        assert!(sample.memory_total_mb.unwrap() > 0);
        assert!(sample.memory_used_mb.unwrap() <= sample.memory_total_mb.unwrap());
    }
}
