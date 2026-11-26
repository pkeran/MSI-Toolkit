//! Native desktop host, v1 IPC coordinator, and an injectable single-threaded HAL executor.
#![cfg(windows)]
#![deny(unsafe_op_in_unsafe_fn)]

mod automation_rules;
mod backends;
mod config_store;
mod coordinator;
mod desktop;
mod events;
mod hal_executor;
mod iocp;
mod lease;
mod osd;
mod pipe_server;
mod qos;
mod security;
mod tray;
mod win;

#[cfg(test)]
mod pipe_integration;

use std::{io, sync::Arc, thread};

use msilt_core::PIPE_NAME;
use windows::Win32::{
    System::LibraryLoader::{LOAD_LIBRARY_SEARCH_SYSTEM32, SetDefaultDllDirectories},
    UI::HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext},
};

pub use backends::BackendOptions;
pub use hal_executor::{DaemonBackends, EcWritePolicy, HalBackend, HalBackends};

use crate::{
    config_store::ConfigStore,
    coordinator::Host,
    desktop::{Desktop, UiSender},
    events::Inbox,
    iocp::Port,
    security::Security,
    win::os_error,
};

#[derive(Clone, Debug)]
pub struct DaemonOptions {
    pub pipe_name: String,
    pub no_tray: bool,
    /// An explicit read-only custom endpoint can run at the user's medium integrity.
    pub readonly: bool,
    /// Only this live, same-logon process may act as a diagnostic UI subscriber.
    pub test_client_pid: Option<u32>,
}

impl Default for DaemonOptions {
    fn default() -> Self {
        Self {
            pipe_name: PIPE_NAME.into(),
            no_tray: false,
            readonly: false,
            test_client_pid: None,
        }
    }
}

pub struct Daemon;

impl Daemon {
    pub fn run_native(options: DaemonOptions, backend: BackendOptions) -> io::Result<()> {
        if options.readonly && backend.allow_platform_control {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "--readonly conflicts with --allow-platform-control",
            ));
        }
        if backend.enable_ec_writes && (options.readonly || !backend.acpi || backend.noop) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "--enable-ec-writes requires --acpi with the native, non-readonly backend",
            ));
        }
        Self::run_factory(options, backend.create(), true)
    }

    /// Runs on the Win32 pump thread and returns after the coordinator closes the endpoint.
    pub fn run(options: DaemonOptions, backends: DaemonBackends) -> io::Result<()> {
        Self::run_factory(options, backends, false)
    }

    fn run_factory(
        options: DaemonOptions,
        mut backends: DaemonBackends,
        load_stored_rules: bool,
    ) -> io::Result<()> {
        let name = local_pipe_name(&options.pipe_name)?;
        let diagnostic =
            options.readonly && !name.eq_ignore_ascii_case(PIPE_NAME) && options.no_tray;
        if options.test_client_pid.is_some() && !diagnostic {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "--test-client-pid requires --readonly --no-tray and a custom --pipe",
            ));
        }
        unsafe {
            SetDefaultDllDirectories(LOAD_LIBRARY_SEARCH_SYSTEM32).map_err(os_error)?;
            let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        }
        let security = if diagnostic {
            Security::readonly_endpoint(options.test_client_pid)?
        } else {
            Security::production()?
        };
        let _instance = if diagnostic {
            None
        } else {
            Some(security.instance_mutex()?)
        };
        let store =
            ConfigStore::interactive(security.interactive_token.clone(), !options.readonly)?;
        if options.readonly {
            let hal = backends.hal;
            backends.hal = Box::new(move || hal().read_only());
        }
        let port = Port::new()?;
        let (inbox, window_actions) = Inbox::new(&port);
        let (mut desktop, notifier) = Desktop::new(
            inbox.clone(),
            security.interactive_token.clone(),
            security.session_id(),
            options.no_tray,
        )?;
        inbox.session(security.session_unlocked()?);
        let notifier = Arc::new(notifier);
        let host = Host {
            port,
            name,
            security,
            store,
            backends,
            inbox: inbox.clone(),
            window_actions,
            desktop: Some(notifier.clone()),
            process_qos: true,
            readonly: options.readonly,
            load_stored_rules,
            #[cfg(test)]
            diagnostics: Arc::default(),
        };
        let coordinator = thread::Builder::new()
            .name("msilt-coordinator".into())
            .stack_size(512 * 1024)
            .spawn(move || {
                struct Finish(Arc<UiSender>);
                impl Drop for Finish {
                    fn drop(&mut self) {
                        self.0.finish();
                    }
                }
                let _finish = Finish(notifier);
                coordinator::run(host)
            })?;
        let pump = desktop.run();
        inbox.stop();
        let coordinator = coordinator
            .join()
            .map_err(|_| io::Error::other("daemon coordinator stopped unexpectedly"))?;
        pump.and(coordinator)
    }
}

fn local_pipe_name(name: &str) -> io::Result<String> {
    const PREFIX: &str = r"\\.\pipe\";
    let suffix = name.strip_prefix(PREFIX).unwrap_or(name);
    if !(1..=240).contains(&suffix.len())
        || !suffix
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.')
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "expected a local pipe name (ASCII letters, digits, ._- only)",
        ));
    }
    Ok(format!("{PREFIX}{suffix}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn custom_endpoint_cannot_escape_the_local_pipe_namespace() {
        assert_eq!(local_pipe_name(PIPE_NAME).unwrap(), PIPE_NAME);
        assert_eq!(
            local_pipe_name("isolated-01").unwrap(),
            r"\\.\pipe\isolated-01"
        );
        for name in [
            r"\\remote\pipe\MSIToolkit-IPC-v1",
            r"..\target",
            "",
            "bad\0name",
        ] {
            assert!(local_pipe_name(name).is_err());
        }
    }
}
