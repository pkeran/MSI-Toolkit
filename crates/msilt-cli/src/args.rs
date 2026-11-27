use std::ffi::OsString;
use std::str::FromStr;

use msilt_core::{ControlChange, ControlTarget, FanChannel, PerformanceMode};

pub const USAGE: &str = "Usage: msilt-cli [--json] <command>

Commands:
  status
  mode <super-battery|silent|balanced|extreme>
  cooler-boost <on|off>
  fan-curve <cpu|gpu> <d1,d2,d3,d4,d5>
                              Set the 5 editable fan duties (0-150%), keeping fixed temps.
  fan-auto                    Return current performance mode to motherboard auto fan.
  gpu-offset <core> <mem>     Set NVIDIA P0 core & memory clock offsets (0-200 MHz).
  charge-limit <percent>      Choose a limit listed by status (1-100).
  refresh <hz>                Choose a refresh rate listed by status.
  brightness <0-100>
  color <profile-id>          Use an ID listed by status, not a file path.
  release <target>            Return a manual override to automatic control.
  config get

Release targets: mode, fan-curve-cpu, fan-curve-gpu, cooler-boost,
                 charge-limit, refresh, brightness, color, keyboard-lighting,
                 gpu-clock-offset.

Options:
  --json     Emit one IPC v1 response as JSON (without the frame length).
  --         Treat the remaining arguments as values.
  -h, --help Show this help.
";

#[derive(Debug, PartialEq, Eq)]
pub enum Invocation {
    Help,
    Run { action: Action, json: bool },
}

#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    Status,
    Control(ControlChange),
    FanCurve { channel: FanChannel, duties: [u8; 5] },
    FanAuto,
    Release(ControlTarget),
    ConfigGet,
}

pub fn parse(args: impl IntoIterator<Item = OsString>) -> Result<Invocation, &'static str> {
    let mut json = false;
    let mut help = false;
    let mut options = true;
    let mut words = Vec::new();
    for arg in args {
        let arg = arg.into_string().map_err(|_| "arguments must be Unicode")?;
        if !options {
            words.push(arg);
            continue;
        }
        match arg.as_str() {
            "--" => options = false,
            "--json" if json => return Err("--json was specified more than once"),
            "--json" => json = true,
            "-h" | "--help" => help = true,
            _ => words.push(arg),
        }
    }
    if help {
        return if words.is_empty() {
            Ok(Invocation::Help)
        } else {
            Err("use --help on its own")
        };
    }

    let mut words = words.iter().map(String::as_str);
    let cmd = words.next().ok_or("a command is required")?;
    let value = words.next();
    let extra = words.next();
    if words.next().is_some() {
        return Err("too many arguments");
    }
    let action = match (cmd, value, extra) {
        ("status", None, None) => Action::Status,
        ("fan-auto", None, None) => Action::FanAuto,
        ("config", Some("get"), None) => Action::ConfigGet,
        ("fan-curve", Some(ch), Some(raw)) => {
            let channel = match ch {
                "cpu" => FanChannel::Cpu,
                "gpu" => FanChannel::Gpu,
                _ => return Err("fan-curve channel must be cpu or gpu"),
            };
            Action::FanCurve {
                channel,
                duties: parse_fan_duties(raw)?,
            }
        }
        ("gpu-offset", Some(core), Some(mem)) => {
            let error = "gpu-offset expects two whole offsets from 0 to 200 MHz";
            let core_mhz: i16 = number(core, error)?;
            let mem_mhz: i16 = number(mem, error)?;
            if !(0..=200).contains(&core_mhz) || !(0..=200).contains(&mem_mhz) {
                return Err(error);
            }
            control(ControlChange::SetGpuClockOffset { core_mhz, mem_mhz }, error)?
        }
        ("mode", Some(value), None) => Action::Control(ControlChange::SetPerformanceMode {
            mode: match value {
                "super-battery" => PerformanceMode::SuperBattery,
                "silent" => PerformanceMode::Silent,
                "balanced" => PerformanceMode::Balanced,
                "extreme" => PerformanceMode::ExtremePerformance,
                _ => return Err("mode expects super-battery, silent, balanced, or extreme"),
            },
        }),
        ("cooler-boost", Some(value), None) => Action::Control(ControlChange::SetCoolerBoost {
            enabled: match value {
                "on" => true,
                "off" => false,
                _ => return Err("cooler-boost expects on or off"),
            },
        }),
        ("charge-limit", Some(value), None) => {
            let error = "charge-limit expects a whole percent from 1 to 100";
            control(
                ControlChange::SetChargeLimit {
                    percent: number(value, error)?,
                },
                error,
            )?
        }
        ("refresh", Some(value), None) => {
            let error = "refresh expects a whole rate from 1 to 65535 Hz";
            control(
                ControlChange::SetDisplayRefreshRate {
                    hz: number(value, error)?,
                },
                error,
            )?
        }
        ("brightness", Some(value), None) => {
            let error = "brightness expects a whole percent from 0 to 100";
            control(
                ControlChange::SetDisplayBrightness {
                    percent: number(value, error)?,
                },
                error,
            )?
        }
        ("color", Some(value), None) => control(
            ControlChange::SetColorProfile {
                profile_id: value.into(),
            },
            "color expects an ID of 1-64 ASCII letters, digits, underscores or hyphens",
        )?,
        ("release", Some(value), None) => Action::Release(match value {
            "mode" | "performance-mode" => ControlTarget::PerformanceMode {},
            "fan-curve-cpu" => ControlTarget::FanCurve {
                channel: FanChannel::Cpu,
            },
            "fan-curve-gpu" => ControlTarget::FanCurve {
                channel: FanChannel::Gpu,
            },
            "cooler-boost" => ControlTarget::CoolerBoost {},
            "charge-limit" => ControlTarget::ChargeLimit {},
            "refresh" | "display-refresh-rate" => ControlTarget::DisplayRefreshRate {},
            "brightness" | "display-brightness" => ControlTarget::DisplayBrightness {},
            "color" | "color-profile" => ControlTarget::ColorProfile {},
            "keyboard-lighting" => ControlTarget::KeyboardLighting {},
            "gpu-clock-offset" | "gpu-offset" => ControlTarget::GpuClockOffset {},
            _ => return Err("unknown release target"),
        }),
        ("status" | "fan-auto", Some(_), _) => return Err("this command takes no arguments"),
        ("config", _, _) => return Err("use config get"),
        ("fan-curve", _, _) => return Err("fan-curve expects <cpu|gpu> <d1,d2,d3,d4,d5>"),
        ("gpu-offset", _, _) => return Err("gpu-offset expects <core-mhz> <mem-mhz>"),
        (
            "mode" | "cooler-boost" | "charge-limit" | "refresh" | "brightness" | "color"
            | "release",
            None,
            _,
        ) => {
            return Err("the command requires an argument");
        }
        (
            "mode" | "cooler-boost" | "charge-limit" | "refresh" | "brightness" | "color"
            | "release",
            Some(_),
            Some(_),
        ) => {
            return Err("too many arguments");
        }
        _ => return Err("unknown command or option"),
    };
    Ok(Invocation::Run { action, json })
}

fn parse_fan_duties(raw: &str) -> Result<[u8; 5], &'static str> {
    let error = "fan-curve expects 5 non-decreasing duties (0-150) separated by commas";
    let parts: Vec<&str> = raw.split(',').collect();
    if parts.len() != 5 {
        return Err(error);
    }
    let mut duties = [0u8; 5];
    for (slot, part) in duties.iter_mut().zip(parts) {
        let v: u8 = number(part, error)?;
        if v > msilt_core::MAX_FAN_DUTY_PERCENT {
            return Err(error);
        }
        *slot = v;
    }
    if duties.windows(2).any(|pair| pair[0] > pair[1]) {
        return Err(error);
    }
    Ok(duties)
}

fn number<T: FromStr>(value: &str, error: &'static str) -> Result<T, &'static str> {
    if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
        return Err(error);
    }
    value.parse().map_err(|_| error)
}

fn control(change: ControlChange, error: &'static str) -> Result<Action, &'static str> {
    change.validate_parameters().map_err(|_| error)?;
    Ok(Action::Control(change))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_words(words: &[&str]) -> Result<Invocation, &'static str> {
        parse(words.iter().copied().map(OsString::from))
    }

    #[test]
    fn parses_read_commands_and_json_flag() {
        for (args, action) in [
            (&["--json", "status"][..], Action::Status),
            (&["config", "get", "--json"][..], Action::ConfigGet),
            (&["config", "--json", "get"][..], Action::ConfigGet),
        ] {
            assert_eq!(
                parse_words(args),
                Ok(Invocation::Run { action, json: true })
            );
        }
        assert_eq!(parse_words(&["-h"]), Ok(Invocation::Help));
        assert_eq!(
            parse_words(&["--json", "color", "--", "--json"]),
            Ok(Invocation::Run {
                action: Action::Control(ControlChange::SetColorProfile {
                    profile_id: "--json".into()
                }),
                json: true,
            }),
        );
    }

    #[test]
    fn parses_all_performance_modes() {
        for (arg, mode) in [
            ("super-battery", PerformanceMode::SuperBattery),
            ("silent", PerformanceMode::Silent),
            ("balanced", PerformanceMode::Balanced),
            ("extreme", PerformanceMode::ExtremePerformance),
        ] {
            assert_eq!(
                parse_words(&["mode", arg]),
                Ok(Invocation::Run {
                    action: Action::Control(ControlChange::SetPerformanceMode { mode }),
                    json: false
                }),
            );
        }
    }

    #[test]
    fn parses_control_values_and_boundaries() {
        for (cmd, value, change) in [
            (
                "cooler-boost",
                "on",
                ControlChange::SetCoolerBoost { enabled: true },
            ),
            (
                "cooler-boost",
                "off",
                ControlChange::SetCoolerBoost { enabled: false },
            ),
            (
                "charge-limit",
                "1",
                ControlChange::SetChargeLimit { percent: 1 },
            ),
            (
                "charge-limit",
                "100",
                ControlChange::SetChargeLimit { percent: 100 },
            ),
            (
                "refresh",
                "240",
                ControlChange::SetDisplayRefreshRate { hz: 240 },
            ),
            (
                "refresh",
                "65535",
                ControlChange::SetDisplayRefreshRate { hz: 65_535 },
            ),
            (
                "brightness",
                "0",
                ControlChange::SetDisplayBrightness { percent: 0 },
            ),
            (
                "brightness",
                "100",
                ControlChange::SetDisplayBrightness { percent: 100 },
            ),
            (
                "color",
                "Factory_sRGB-1",
                ControlChange::SetColorProfile {
                    profile_id: "Factory_sRGB-1".into(),
                },
            ),
        ] {
            assert_eq!(
                parse_words(&[cmd, value]),
                Ok(Invocation::Run {
                    action: Action::Control(change),
                    json: false
                })
            );
        }
    }

    #[test]
    fn parses_all_release_targets() {
        for (arg, target) in [
            ("mode", ControlTarget::PerformanceMode {}),
            (
                "fan-curve-cpu",
                ControlTarget::FanCurve {
                    channel: FanChannel::Cpu,
                },
            ),
            (
                "fan-curve-gpu",
                ControlTarget::FanCurve {
                    channel: FanChannel::Gpu,
                },
            ),
            ("cooler-boost", ControlTarget::CoolerBoost {}),
            ("charge-limit", ControlTarget::ChargeLimit {}),
            ("refresh", ControlTarget::DisplayRefreshRate {}),
            ("brightness", ControlTarget::DisplayBrightness {}),
            ("color", ControlTarget::ColorProfile {}),
            ("keyboard-lighting", ControlTarget::KeyboardLighting {}),
            ("gpu-clock-offset", ControlTarget::GpuClockOffset {}),
        ] {
            assert_eq!(
                parse_words(&["release", arg]),
                Ok(Invocation::Run {
                    action: Action::Release(target),
                    json: false
                })
            );
        }
    }

    #[test]
    fn parses_fan_curve_fan_auto_and_gpu_offset() {
        assert_eq!(
            parse_words(&["fan-auto"]),
            Ok(Invocation::Run { action: Action::FanAuto, json: false })
        );
        assert_eq!(
            parse_words(&["fan-curve", "cpu", "36,42,56,88,120"]),
            Ok(Invocation::Run {
                action: Action::FanCurve {
                    channel: FanChannel::Cpu,
                    duties: [36, 42, 56, 88, 120],
                },
                json: false,
            })
        );
        assert_eq!(
            parse_words(&["fan-curve", "gpu", "36,46,62,92,120"]),
            Ok(Invocation::Run {
                action: Action::FanCurve {
                    channel: FanChannel::Gpu,
                    duties: [36, 46, 62, 92, 120],
                },
                json: false,
            })
        );
        assert_eq!(
            parse_words(&["gpu-offset", "150", "200"]),
            Ok(Invocation::Run {
                action: Action::Control(ControlChange::SetGpuClockOffset {
                    core_mhz: 150,
                    mem_mhz: 200,
                }),
                json: false,
            })
        );
        for bad in [
            &["fan-auto", "extra"][..],
            &["fan-curve", "cpu"][..],
            &["fan-curve", "aux", "36,42,56,88,120"][..],
            &["fan-curve", "cpu", "36,42,56,88"][..],
            &["fan-curve", "cpu", "50,40,60,80,100"][..],
            &["fan-curve", "cpu", "36,42,56,88,151"][..],
            &["gpu-offset", "150"][..],
            &["gpu-offset", "205", "0"][..],
            &["gpu-offset", "-10", "0"][..],
        ] {
            assert!(parse_words(bad).is_err(), "accepted {bad:?}");
        }
    }

    #[test]
    fn rejects_invalid_arguments_without_contacting_the_daemon() {
        for words in [
            &[][..],
            &["--bogus"],
            &["--json"],
            &["status", "extra"],
            &["--json", "status", "--json"],
            &["status", "--help"],
            &["config"],
            &["config", "set"],
            &["config", "get", "extra"],
            &["mode"],
            &["mode", "custom"],
            &["cooler-boost", "true"],
            &["charge-limit", "0"],
            &["charge-limit", "101"],
            &["refresh", "0"],
            &["refresh", "65536"],
            &["refresh", "60.0"],
            &["brightness", "101"],
            &["brightness", "-1"],
            &["brightness", "+1"],
            &["brightness", ""],
            &["color", r"C:\factory.icm"],
            &["color", "../factory"],
            &["color", ""],
            &["color", "色彩"],
            &["release", "fan-curve"],
            &["release", "gpu-mux"],
        ] {
            assert!(parse_words(words).is_err(), "accepted {words:?}");
        }
        assert!(parse_words(&["color", &"a".repeat(65)]).is_err());
    }
}
