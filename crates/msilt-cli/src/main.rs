//! One-shot local IPC v1 client.

mod args;
#[cfg(any(windows, test))]
mod client;
#[cfg(any(windows, test))]
mod output;
#[cfg(windows)]
mod pipe;

use std::io::{self, Write};
use std::process::ExitCode;

fn main() -> ExitCode {
    match args::parse(std::env::args_os().skip(1)) {
        Ok(args::Invocation::Help) => match io::stdout().lock().write_all(args::USAGE.as_bytes()) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => fail(error),
        },
        Ok(args::Invocation::Run { action, json }) => run(action, json),
        Err(error) => {
            let _ = writeln!(io::stderr().lock(), "msilt-cli: {error}\n\n{}", args::USAGE);
            ExitCode::from(2)
        }
    }
}

#[cfg(windows)]
fn run(action: args::Action, json: bool) -> ExitCode {
    use msilt_core::ipc::IpcResult;

    let pipe = match pipe::Pipe::connect() {
        Ok(pipe) => pipe,
        Err(error) => return fail(error),
    };
    match client::execute_with_frame(pipe, action) {
        Ok((response, frame)) => {
            let written = if json {
                output::write_json(&mut io::stdout().lock(), &frame)
            } else if matches!(&response.result, IpcResult::Ok { .. }) {
                output::write(&mut io::stdout().lock(), &response)
            } else {
                Ok(())
            };
            if let Err(error) = written {
                return fail(error);
            }
            match response.result {
                IpcResult::Ok { .. } => ExitCode::SUCCESS,
                IpcResult::Error { error } => fail(output::daemon_error(error.code)),
            }
        }
        Err(error) => fail(error),
    }
}

#[cfg(not(windows))]
fn run(_: args::Action, _: bool) -> ExitCode {
    fail("the MSI-Toolkit daemon pipe is available only on Windows")
}

fn fail(error: impl std::fmt::Display) -> ExitCode {
    let _ = writeln!(io::stderr().lock(), "msilt-cli: {error}");
    ExitCode::FAILURE
}
