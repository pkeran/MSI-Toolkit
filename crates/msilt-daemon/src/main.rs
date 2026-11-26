#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

#[cfg(windows)]
fn main() -> std::process::ExitCode {
    use msilt_daemon::{BackendOptions, Daemon, DaemonOptions};

    let mut options = DaemonOptions::default();
    let mut backend = BackendOptions::default();
    const USAGE: &str = "msilt-daemon [--pipe <name>] [--no-tray] [--readonly] [--acpi] [--allow-platform-control] [--enable-ec-writes] [--noop-backend]\n\
        Default: real read-only Windows HAL and automation.toml; no NVML or EC writes.\n\
        --acpi enables audited read-only ACPI telemetry.\n\
        --allow-platform-control permits display, brightness, ICC and keyboard lighting controls.\n\
        --enable-ec-writes requires --acpi and the exact audited firmware with no OEM conflict.\n\
        Diagnostic UI: --readonly --no-tray --pipe <custom> --test-client-pid <pid>.";
    let mut args = std::env::args_os().skip(1);
    while let Some(argument) = args.next() {
        match argument.to_str() {
            Some("--pipe") => match args.next().and_then(|value| value.into_string().ok()) {
                Some(name) => options.pipe_name = name,
                None => {
                    eprintln!("msilt-daemon: --pipe requires a local pipe name");
                    return std::process::ExitCode::from(2);
                }
            },
            Some("--no-tray") => options.no_tray = true,
            Some("--readonly") => options.readonly = true,
            Some("--acpi") => backend.acpi = true,
            Some("--allow-platform-control") => backend.allow_platform_control = true,
            Some("--enable-ec-writes") => backend.enable_ec_writes = true,
            Some("--noop-backend") => backend.noop = true,
            Some("--test-client-pid") => match args
                .next()
                .and_then(|value| value.into_string().ok())
                .and_then(|value| value.parse::<u32>().ok())
                .filter(|&pid| pid != 0)
            {
                Some(pid) => options.test_client_pid = Some(pid),
                None => {
                    eprintln!("msilt-daemon: --test-client-pid requires a nonzero process ID");
                    return std::process::ExitCode::from(2);
                }
            },
            Some("--help" | "-h") => {
                println!("{USAGE}");
                return std::process::ExitCode::SUCCESS;
            }
            _ => {
                eprintln!("usage: {USAGE}");
                return std::process::ExitCode::from(2);
            }
        }
    }
    match Daemon::run_native(options, backend) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("msilt-daemon: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}

#[cfg(not(windows))]
fn main() -> std::process::ExitCode {
    eprintln!("msilt-daemon requires Windows");
    std::process::ExitCode::FAILURE
}
