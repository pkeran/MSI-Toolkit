use std::{
    ffi::c_void,
    io,
    os::windows::io::OwnedHandle,
    ptr,
    sync::Arc,
    time::{Duration, Instant},
};

use windows::Win32::{
    Foundation::{FILETIME, HANDLE, INVALID_HANDLE_VALUE},
    System::{IO::*, Threading::*},
};

use crate::win::{os_error, own, raw};

pub(crate) const WAKE_WINDOW: usize = 1;
pub(crate) const WAKE_HAL: usize = 2;
pub(crate) const WAKE_STOP: usize = 3;
pub(crate) const WAKE_TIMER: usize = 4;
pub(crate) const PIPE_KEY: usize = 16;

#[derive(Clone)]
pub(crate) struct Port(Arc<OwnedHandle>);

pub(crate) struct Packet {
    pub(crate) key: usize,
    pub(crate) overlapped: *mut OVERLAPPED,
    pub(crate) bytes: usize,
    pub(crate) ok: bool,
}

impl Port {
    pub(crate) fn new() -> io::Result<Self> {
        let handle = unsafe { CreateIoCompletionPort(INVALID_HANDLE_VALUE, None, 0, 1) }
            .map_err(os_error)?;
        Ok(Self(Arc::new(unsafe { own(handle) })))
    }

    pub(crate) fn associate(&self, handle: HANDLE, key: usize) -> io::Result<()> {
        unsafe { CreateIoCompletionPort(handle, Some(raw(&self.0)), key, 0) }.map_err(os_error)?;
        Ok(())
    }

    pub(crate) fn post(&self, key: usize) -> io::Result<()> {
        unsafe { PostQueuedCompletionStatus(raw(&self.0), 0, key, None) }.map_err(os_error)
    }

    pub(crate) fn wait(&self) -> io::Result<Packet> {
        let (mut bytes, mut key, mut overlapped) = (0, 0, ptr::null_mut());
        let result = unsafe {
            GetQueuedCompletionStatus(
                raw(&self.0),
                &mut bytes,
                &mut key,
                &mut overlapped,
                INFINITE,
            )
        };
        if overlapped.is_null() {
            result.as_ref().map_err(|error| os_error(error.clone()))?;
        }
        Ok(Packet {
            key,
            overlapped,
            bytes: bytes as usize,
            ok: result.is_ok(),
        })
    }
}

pub(crate) struct DeadlineTimer {
    timer: PTP_TIMER,
    _port: Box<Port>,
    armed: Option<Instant>,
}

impl DeadlineTimer {
    pub(crate) fn new(port: &Port) -> io::Result<Self> {
        let context = Box::new(port.clone());
        let timer = unsafe {
            CreateThreadpoolTimer(
                Some(timer_elapsed),
                Some(ptr::from_ref(&*context).cast_mut().cast()),
                None,
            )
        }
        .map_err(os_error)?;
        Ok(Self {
            timer,
            _port: context,
            armed: None,
        })
    }

    pub(crate) fn set(&mut self, next: Option<Instant>) {
        if self.armed == next {
            return;
        }
        self.armed = next;
        let due = next.map(|next| {
            let delay = next.saturating_duration_since(Instant::now());
            let ticks = delay.as_nanos().div_ceil(100).clamp(1, i64::MAX as u128) as i64;
            let due = (-ticks) as u64;
            FILETIME {
                dwLowDateTime: due as u32,
                dwHighDateTime: (due >> 32) as u32,
            }
        });
        let tolerance = if next.is_some_and(|next| {
            next.saturating_duration_since(Instant::now()) >= Duration::from_millis(100)
        }) {
            16
        } else {
            0
        };
        unsafe {
            SetThreadpoolTimer(
                self.timer,
                due.as_ref().map(ptr::from_ref),
                0,
                Some(tolerance),
            );
        }
    }

    pub(crate) fn fired(&mut self) {
        // A canceled callback may already have queued a packet. The coordinator rechecks all deadlines.
        self.armed = None;
    }
}

unsafe extern "system" fn timer_elapsed(
    _: PTP_CALLBACK_INSTANCE,
    context: *mut c_void,
    _: PTP_TIMER,
) {
    let port = unsafe { &*context.cast::<Port>() };
    let _ = port.post(WAKE_TIMER);
}

impl Drop for DeadlineTimer {
    fn drop(&mut self) {
        unsafe {
            SetThreadpoolTimer(self.timer, None, 0, None);
            WaitForThreadpoolTimerCallbacks(self.timer, true);
            CloseThreadpoolTimer(self.timer);
        }
    }
}
