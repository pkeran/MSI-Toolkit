use std::ffi::OsString;
use std::str::FromStr;

use msilt_core::{ControlChange, ControlTarget, FanChannel, PerformanceMode};

pub const USAGE: &str = "Usage: msilt-cli [--json] <command>

Commands:
  status
  mode <super-battery|silent|balanced|extreme>
  cooler-boost <on|off>
  charge-limit <percent>      Choose a limit listed by status (1-100).
  refresh <hz>                Choose a refresh rate listed by status.
  brightness <0-100>
  color <profile-id>          Use an ID listed by status, not a file path.
  release <target>            Return a manual override to automatic control.
  config get

Release targets: mode, fan-curve-cpu, fan-curve-gpu, cooler-boost,
                 charge-limit, refresh, brightness, color, keyboard-lighting.

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
    if words.next().is_some() {
        return Err("too many arguments");
    }
    let action = match (cmd, value) {
        ("status", None) => Action::Status,
        ("config", Some("get")) => Action::ConfigGet,
        ("mode", Some(value)) => Action::Control(ControlChange::SetPerformanceMode {
            mode: match value {
                "super-battery" => PerformanceMode::SuperBattery,
                "silent" => PerformanceMode::Silent,
                "balanced" => PerformanceMode::Balanced,
                "extreme" => PerformanceMode::ExtremePerformance,
                _ => return Err("mode expects super-battery, silent, balanced, or extreme"),
            },
        }),
        ("cooler-boost", Some(value)) => Action::Control(ControlChange::SetCoolerBoost {
            enabled: match value {
                "on" => true,
                "off" => false,
                _ => return Err("cooler-boost expects on or off"),
            },
        }),
        ("charge-limit", Some(value)) => {
            let error = "charge-limit expects a whole percent from 1 to 100";
            control(
                ControlChange::SetChargeLimit {
                    percent: number(value, error)?,
                },
                error,
            )?
        }
        ("refresh", Some(value)) => {
            let error = "refresh expects a whole rate from 1 to 65535 Hz";
            control(
                ControlChange::SetDisplayRefreshRate {
                    hz: number(value, error)?,
                },
                error,
            )?
        }
        ("brightness", Some(value)) => {
            let error = "brightness expects a whole percent from 0 to 100";
            control(
                ControlChange::SetDisplayBrightness {
                    percent: number(value, error)?,
                },
                error,
            )?
        }
        ("color", Some(value)) => control(
            ControlChange::SetColorProfile {
                profile_id: value.into(),
            },
            "color expects an ID of 1-64 ASCII letters, digits, underscores or hyphens",
        )?,
        ("release", Some(value)) => Action::Release(match value {
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
            _ => return Err("unknown release target"),
        }),
        ("status", Some(_)) => return Err("status takes no argument"),
        ("config", _) => return Err("use config get"),
        (
            "mode" | "cooler-boost" | "charge-limit" | "refresh" | "brightness" | "color"
            | "release",
            None,
        ) => {
            return Err("the command requires an argument");
        }
        _ => return Err("unknown command or option"),
    };
    Ok(Invocation::Run { action, json })
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
