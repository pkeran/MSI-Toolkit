//! Phase 0 Windows hardware diagnostics.

use std::ffi::OsString;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;

#[cfg(windows)]
mod summary;

const HELP: &str = "\
MSI Toolkit Phase 0 read-only diagnostics

Usage: msilt-probe [--json] [--out <path>] [--acpi] [--include-nvml]

  --json          Print the structured report as JSON instead of a text table
  --out <path>    Also write the chosen format to a new file (no overwrite)
  --acpi          Read MSI_ACPI on the audited MS-17S3; requires an elevated administrator
  --include-nvml  Allow one NVML sample; this can wake the NVIDIA GPU
  -h, --help      Show this help

Exit codes: 0 complete, 1 diagnostic or output failure, 2 usage error.
";

#[derive(Debug, Default, PartialEq, Eq)]
struct Options {
    json: bool,
    out: Option<PathBuf>,
    acpi: bool,
    include_nvml: bool,
}

fn parse(args: impl IntoIterator<Item = OsString>) -> Result<Option<Options>, &'static str> {
    let mut opts = Options::default();
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("--json") if !opts.json => opts.json = true,
            Some("--acpi") if !opts.acpi => opts.acpi = true,
            Some("--include-nvml") if !opts.include_nvml => opts.include_nvml = true,
            Some("--out") if opts.out.is_none() => {
                let path = args
                    .next()
                    .filter(|path| !path.is_empty() && !is_option(path));
                opts.out = Some(path.ok_or("--out requires a file path")?.into());
            }
            Some("--help" | "-h") => return Ok(None),
            Some("--json" | "--acpi" | "--include-nvml" | "--out") => {
                return Err("duplicate option");
            }
            _ => return Err("unrecognized argument"),
        }
    }
    Ok(Some(opts))
}

fn is_option(arg: &OsString) -> bool {
    arg.to_str().is_some_and(|text| text.starts_with('-'))
}

fn main() -> ExitCode {
    match parse(std::env::args_os().skip(1)) {
        Ok(Some(options)) => match run(options) {
            Ok(code) => code,
            Err(error) => {
                eprintln!("msilt-probe: {error}");
                ExitCode::FAILURE
            }
        },
        Ok(None) => match io::stdout().lock().write_all(HELP.as_bytes()) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("msilt-probe: stdout: {error}");
                ExitCode::FAILURE
            }
        },
        Err(error) => {
            eprintln!("msilt-probe: {error}\n\n{HELP}");
            ExitCode::from(2)
        }
    }
}

#[cfg(windows)]
fn run(options: Options) -> Result<ExitCode, Box<dyn std::error::Error>> {
    use std::fs::File;

    use msilt_hal::{ProbeOptions, WindowsHal};

    let mut output = options
        .out
        .as_ref()
        .map(File::create_new)
        .transpose()
        .map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("--out could not create report: {error}"),
            )
        })?;
    let report = WindowsHal::new(ProbeOptions {
        acpi: options.acpi,
        include_nvml: options.include_nvml,
    })
    .diagnose();
    let payload = if options.json {
        let mut json = serde_json::to_string_pretty(&report)?;
        json.push('\n');
        json
    } else {
        summary::render(&report)
    };
    if let Some(file) = &mut output {
        file.write_all(payload.as_bytes())
            .and_then(|()| file.sync_all())
            .map_err(|error| {
                io::Error::new(
                    error.kind(),
                    format!("--out could not finish report: {error}"),
                )
            })?;
    }
    io::stdout().lock().write_all(payload.as_bytes())?;
    Ok(if report.has_failures() {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

#[cfg(not(windows))]
fn run(_: Options) -> Result<ExitCode, Box<dyn std::error::Error>> {
    Err("the native read-only probe requires Windows".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args<'a>(values: &'a [&'a str]) -> impl Iterator<Item = OsString> + 'a {
        values.iter().map(OsString::from)
    }

    #[test]
    fn optional_native_channels_require_explicit_flags() {
        assert_eq!(parse(args(&[])).unwrap(), Some(Options::default()));
        assert_eq!(
            parse(args(&[
                "--include-nvml",
                "--out",
                "diagnostic report.json",
                "--json",
                "--acpi"
            ]))
            .unwrap(),
            Some(Options {
                json: true,
                out: Some("diagnostic report.json".into()),
                acpi: true,
                include_nvml: true
            })
        );
    }

    #[test]
    fn rejects_invalid_options_before_opening_hardware() {
        for flags in [
            &["--out"][..],
            &["--out", "--acpi"],
            &["--json", "--json"],
            &["--unknown"],
        ] {
            assert!(parse(args(flags)).is_err());
        }
        assert_eq!(parse(args(&["--help"])).unwrap(), None);
    }
}
