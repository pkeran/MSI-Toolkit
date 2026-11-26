//! Windows hardware inspection, explicit control backends and deterministic contract tests.

mod diagnostic;
pub mod ec_map_loader;
pub mod mock;

#[cfg(windows)]
pub mod battery;
#[cfg(windows)]
pub mod daemon_wire;
#[cfg(windows)]
pub mod disabler;
#[cfg(windows)]
pub mod display;
#[cfg(windows)]
mod executor;
#[cfg(windows)]
pub mod hid_rgb;
#[cfg(windows)]
pub mod identity;
#[cfg(windows)]
pub mod ms17s3;
#[cfg(windows)]
mod native;
#[cfg(windows)]
pub mod nvml;
#[cfg(windows)]
pub mod oem_manager;
#[cfg(windows)]
pub mod startup;
#[cfg(windows)]
mod windows_hal;
#[cfg(windows)]
pub mod wmi_acpi;

pub use diagnostic::{Diagnostic, DiagnosticKind, NativeDomain, NativeStatus};
#[cfg(windows)]
pub use disabler::SoftwareDisabler;
pub use msilt_core::{EcRegisterIo, HardwareRead, PlatformControl};
#[cfg(windows)]
pub use windows_hal::{ControlOptions, EcMapReport, ProbeOptions, ProbeReport, WindowsHal};
