use std::fmt::{self, Display};
use std::io::{self, Write};

use msilt_core::ipc::{ErrorCode, FRAME_HEADER_LEN, IpcReply, IpcResponse, IpcResult};
use msilt_core::{
    ControlTarget, FanChannel, FanMode, FanState, ManualOverride, PerformanceMode, PowerSource,
    SystemSnapshot,
};

pub fn write_json(out: &mut impl Write, verified_frame: &[u8]) -> io::Result<()> {
    out.write_all(&verified_frame[FRAME_HEADER_LEN..])?;
    out.write_all(b"\n")
}

pub fn write(out: &mut impl Write, response: &IpcResponse) -> io::Result<()> {
    match &response.result {
        IpcResult::Ok {
            reply: IpcReply::Snapshot { snapshot },
        } => write_snapshot(out, snapshot),
        IpcResult::Ok {
            reply: IpcReply::Applied { snapshot },
        } => {
            writeln!(out, "Applied. Daemon reported:")?;
            write_snapshot(out, snapshot)
        }
        IpcResult::Ok {
            reply: IpcReply::Config { config, revision },
        } => {
            writeln!(
                out,
                "Config revision: {revision} (schema {})",
                config.schema_version
            )?;
            writeln!(out, "Theme: {:?}", config.theme)?;
            writeln!(
                out,
                "OSD: {} ({} ms)",
                on_off(config.osd.enabled),
                config.osd.duration_ms
            )?;
            writeln!(out, "Automation: {}", on_off(config.automation.enabled))?;
            match config.automation.manual_override {
                ManualOverride::UntilReleased => writeln!(out, "Manual override: until released"),
                ManualOverride::Timed { seconds } => writeln!(out, "Manual override: {seconds} s"),
            }
        }
        IpcResult::Ok {
            reply: IpcReply::SystemTools { startup, oem },
        } => {
            writeln!(
                out,
                "Startup: {} ({:?})",
                on_off(startup.enabled),
                startup.mode
            )?;
            writeln!(out, "{}", startup.detail)?;
            writeln!(out, "{}", oem.summary)
        }
        IpcResult::Ok {
            reply: IpcReply::AutomationRules { rules, summary },
        } => {
            writeln!(out, "{summary}")?;
            for rule in rules {
                writeln!(
                    out,
                    "{} [{}]: {}; {}",
                    rule.name,
                    on_off(rule.enabled),
                    rule.actions,
                    rule.diagnostic
                )?;
            }
            Ok(())
        }
        IpcResult::Ok {
            reply: IpcReply::OsdShown {},
        } => writeln!(out, "OSD shown."),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unexpected reply for display",
        )),
    }
}

fn write_snapshot(out: &mut impl Write, snapshot: &SystemSnapshot) -> io::Result<()> {
    let hw = &snapshot.hardware;
    writeln!(
        out,
        "Daemon: {:?} (snapshot revision {})",
        snapshot.status, snapshot.revision
    )?;
    writeln!(out, "EC writes: {:?}", snapshot.ec_write_status)?;
    writeln!(
        out,
        "Capabilities: {:#010x}; writable: {:#010x}",
        snapshot.capabilities.bits(),
        snapshot.writable.bits()
    )?;
    optional(out, "Board", hw.identity.board_id.as_deref(), "")?;
    optional(out, "EC firmware", hw.identity.ec_firmware.as_deref(), "")?;
    optional(out, "Mode", hw.performance_mode.map(mode_arg), "")?;
    optional(
        out,
        "Fan mode",
        hw.fan_mode.map(|mode| match mode {
            FanMode::Auto => "auto",
            FanMode::Silent => "silent",
            FanMode::Advanced => "advanced",
        }),
        "",
    )?;
    if !snapshot.custom_fan_modes.is_empty() {
        choices(
            out,
            "Custom fan profiles",
            snapshot.custom_fan_modes.iter().copied().map(mode_arg),
        )?;
    }
    choices(
        out,
        "Supported modes",
        hw.supported_performance_modes.iter().copied().map(mode_arg),
    )?;
    optional(out, "Cooler Boost", hw.cooler_boost.map(on_off), "")?;
    optional(
        out,
        "Power source",
        hw.power_source.map(|p| match p {
            PowerSource::Ac => "AC",
            PowerSource::Battery => "battery",
        }),
        "",
    )?;
    optional(out, "Battery level", hw.battery_percent, "%")?;
    optional(out, "Charge limit", hw.charge_limit_percent, "%")?;
    choices(
        out,
        "Supported charge limits (%)",
        hw.supported_charge_limits.iter(),
    )?;
    optional(out, "Refresh rate", hw.display_refresh_hz, " Hz")?;
    choices(
        out,
        "Supported refresh rates (Hz)",
        hw.supported_refresh_rates.iter(),
    )?;
    optional(out, "Brightness", hw.display_brightness_percent, "%")?;
    optional(out, "Color profile", hw.color_profile_id.as_deref(), "")?;
    choices(
        out,
        "Supported color profiles",
        hw.supported_color_profiles.iter(),
    )?;
    fan_curve(out, "CPU fan curve", hw.cpu_fan)?;
    fan_curve(out, "GPU fan curve", hw.gpu_fan)?;
    if let Some(mux) = hw.gpu_mux {
        writeln!(out, "GPU MUX: {:?}", mux.current)?;
        if let Some(pending) = mux.pending {
            writeln!(out, "GPU MUX pending: {pending:?}")?;
        }
    } else {
        writeln!(out, "GPU MUX: unknown")?;
    }
    if let Some(offset) = hw.gpu_clock_offset {
        writeln!(out, "GPU clock offset: core {:+}, VRAM {:+} MHz", offset.core_mhz, offset.mem_mhz)?;
    }
    optional(
        out,
        "Keyboard lighting (observed zones)",
        hw.keyboard_zones.map(|zones| zones.len()),
        "",
    )?;
    choices(
        out,
        "Manual overrides",
        snapshot.manual_overrides.iter().copied().map(target_arg),
    )
}

fn optional(
    out: &mut impl Write,
    label: &str,
    value: Option<impl Display>,
    unit: &str,
) -> io::Result<()> {
    match value {
        Some(value) => writeln!(out, "{label}: {value}{unit}"),
        None => writeln!(out, "{label}: unknown"),
    }
}

fn choices(
    out: &mut impl Write,
    label: &str,
    mut values: impl Iterator<Item = impl Display>,
) -> io::Result<()> {
    let Some(first) = values.next() else {
        return writeln!(out, "{label}: none");
    };
    write!(out, "{label}: {first}")?;
    for value in values {
        write!(out, ", {value}")?;
    }
    writeln!(out)
}

fn fan_curve(out: &mut impl Write, label: &str, fan: Option<FanState>) -> io::Result<()> {
    let Some(fan) = fan else {
        return writeln!(out, "{label}: unknown");
    };
    choices(
        out,
        label,
        fan.curve
            .points
            .iter()
            .map(|point| format!("{} C: {}%", point.temperature_c, point.duty_percent)),
    )
}

fn on_off(on: bool) -> &'static str {
    if on { "on" } else { "off" }
}

fn mode_arg(mode: PerformanceMode) -> &'static str {
    match mode {
        PerformanceMode::SuperBattery => "super-battery",
        PerformanceMode::Silent => "silent",
        PerformanceMode::Balanced => "balanced",
        PerformanceMode::ExtremePerformance => "extreme",
    }
}

fn target_arg(target: ControlTarget) -> &'static str {
    match target {
        ControlTarget::PerformanceMode {} => "mode",
        ControlTarget::FanCurve {
            channel: FanChannel::Cpu,
        } => "fan-curve-cpu",
        ControlTarget::FanCurve {
            channel: FanChannel::Gpu,
        } => "fan-curve-gpu",
        ControlTarget::CoolerBoost {} => "cooler-boost",
        ControlTarget::ChargeLimit {} => "charge-limit",
        ControlTarget::DisplayRefreshRate {} => "refresh",
        ControlTarget::DisplayBrightness {} => "brightness",
        ControlTarget::ColorProfile {} => "color",
        ControlTarget::KeyboardLighting {} => "keyboard-lighting",
        ControlTarget::GpuClockOffset {} => "gpu-clock-offset",
        _ => "unknown target",
    }
}

pub fn daemon_error(code: ErrorCode) -> impl Display {
    struct DaemonError(ErrorCode);
    impl Display for DaemonError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            let message = match self.0 {
                ErrorCode::InvalidRequest => "daemon rejected the IPC request",
                ErrorCode::IncompatibleVersion => "install matching versions of the CLI and daemon",
                ErrorCode::AccessDenied => "daemon denied access for this login session",
                ErrorCode::NotReady => "daemon is not ready; check status",
                ErrorCode::Unsupported => {
                    "target or value is unsupported; check the choices in status"
                }
                ErrorCode::ReadOnlyFirmware => "hardware or firmware is read-only",
                ErrorCode::DeniedWrite | ErrorCode::WriteNotAllowed => {
                    "hardware write is not authorized by the verified mapping"
                }
                ErrorCode::ValidationFailed => "requested value failed validation",
                ErrorCode::RevisionConflict => "state changed; check status before retrying",
                ErrorCode::RequestExpired => {
                    "request expired in the daemon queue without executing"
                }
                ErrorCode::Busy => "daemon is busy; the request did not execute",
                ErrorCode::ConflictDetected => {
                    "another controller is active or the hardware state conflicts; check status"
                }
                ErrorCode::TimedOut => {
                    "hardware operation timed out; outcome may be unknown, check status before retrying"
                }
                ErrorCode::BackendUnavailable => "hardware backend is unavailable",
                ErrorCode::WriteFailed | ErrorCode::VerificationFailed => {
                    "the write or readback failed; check the actual value in status"
                }
                ErrorCode::RollbackFailed | ErrorCode::RecoveryRequired => {
                    "EC writes are locked pending recovery"
                }
                ErrorCode::Internal => {
                    "daemon reported an internal error; check its local diagnostics"
                }
            };
            write!(f, "{message} ({:?})", self.0)
        }
    }
    DaemonError(code)
}

#[cfg(test)]
mod tests {
    use super::*;

    use msilt_core::AppConfig;
    use msilt_core::ipc::{IpcError, IpcMessage, encode_frame};

    #[test]
    fn json_output_is_one_unframed_v1_response() {
        let response = IpcResponse {
            id: 2,
            result: IpcResult::Error {
                error: IpcError {
                    code: ErrorCode::RevisionConflict,
                },
            },
        };
        let mut out = Vec::new();
        write_json(
            &mut out,
            &encode_frame(&IpcMessage::Response(response)).unwrap(),
        )
        .unwrap();
        assert_eq!(out, b"{\"type\":\"response\",\"message\":{\"id\":2,\"result\":{\"status\":\"error\",\"error\":{\"code\":\"revisionConflict\"}}}}\n");
        assert!(
            daemon_error(ErrorCode::RevisionConflict)
                .to_string()
                .contains("check status")
        );
    }

    #[test]
    fn config_revision_and_unknown_values_are_distinct_from_zero() {
        let mut out = Vec::new();
        let response = IpcResponse {
            id: 2,
            result: IpcResult::Ok {
                reply: IpcReply::Config {
                    config: AppConfig::default(),
                    revision: 3,
                },
            },
        };
        write(&mut out, &response).unwrap();
        assert!(
            String::from_utf8(out)
                .unwrap()
                .contains("Config revision: 3 (schema 1)")
        );
        let mut out = Vec::new();
        let mut snapshot = SystemSnapshot::default();
        snapshot.hardware.display_brightness_percent = Some(0);
        write_snapshot(&mut out, &snapshot).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("Brightness: 0%\n"));
        assert!(text.contains("Battery level: unknown\n"));
    }

    #[test]
    fn gpu_offset_output_is_compact_and_absent_when_unavailable() {
        let mut snapshot = SystemSnapshot::default();
        let mut out = Vec::new();
        write_snapshot(&mut out, &snapshot).unwrap();
        assert!(!String::from_utf8(out).unwrap().contains("GPU clock offset"));
        snapshot.hardware.gpu_clock_offset = Some(msilt_core::GpuClockOffsetState {
            core_mhz: 100, mem_mhz: 200,
            core_min_mhz: 0, core_max_mhz: 200, mem_min_mhz: 0, mem_max_mhz: 200,
        });
        let mut out = Vec::new();
        write_snapshot(&mut out, &snapshot).unwrap();
        assert!(String::from_utf8(out).unwrap().contains("GPU clock offset: core +100, VRAM +200 MHz\n"));
    }

    #[test]
    fn new_daemon_reply_kinds_have_text_output() {
        for (reply, needle) in [
            (
                IpcReply::SystemTools {
                    startup: Default::default(),
                    oem: Default::default(),
                },
                "Startup:",
            ),
            (
                IpcReply::AutomationRules {
                    rules: Vec::new(),
                    summary: "No rules".into(),
                },
                "No rules",
            ),
            (IpcReply::OsdShown {}, "OSD shown."),
        ] {
            let response = IpcResponse {
                id: 2,
                result: IpcResult::Ok { reply },
            };
            let mut out = Vec::new();
            write(&mut out, &response).unwrap();
            assert!(String::from_utf8(out).unwrap().contains(needle));
        }
    }
}
