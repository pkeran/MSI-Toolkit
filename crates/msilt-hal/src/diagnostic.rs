use std::fmt;

use msilt_core::HalError;
use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum DiagnosticKind {
    AccessDenied,
    Unavailable,
    Busy,
    ConflictDetected,
    Unsupported,
    TimedOut,
    InvalidData,
    Rejected,
    Io,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum NativeDomain {
    Win32,
    Hresult,
    NtStatus,
    Nvml,
    DisplayChange,
    WmiReturn,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct NativeStatus {
    pub domain: NativeDomain,
    pub code: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Diagnostic {
    pub operation: &'static str,
    pub kind: DiagnosticKind,
    pub native_status: Option<NativeStatus>,
    pub detail: Option<String>,
}

#[cfg(windows)]
impl Diagnostic {
    pub(crate) fn new(operation: &'static str, kind: DiagnosticKind) -> Self {
        Self {
            operation,
            kind,
            native_status: None,
            detail: None,
        }
    }

    pub(crate) fn native(
        operation: &'static str,
        kind: DiagnosticKind,
        domain: NativeDomain,
        code: u32,
    ) -> Self {
        Self {
            native_status: Some(NativeStatus { domain, code }),
            ..Self::new(operation, kind)
        }
    }

    pub(crate) fn detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    pub(crate) fn invalid(operation: &'static str, detail: impl Into<String>) -> Self {
        Self::new(operation, DiagnosticKind::InvalidData).detail(detail)
    }

    pub(crate) fn win32(operation: &'static str, code: u32) -> Self {
        let kind = match code {
            5 | 1314 => DiagnosticKind::AccessDenied,
            2 | 3 | 1167 | 1060 | 126 => DiagnosticKind::Unavailable,
            50 | 120 => DiagnosticKind::Unsupported,
            258 | 1460 => DiagnosticKind::TimedOut,
            _ => DiagnosticKind::Io,
        };
        Self::native(operation, kind, NativeDomain::Win32, code)
    }

    pub(crate) fn windows(operation: &'static str, error: windows::core::Error) -> Self {
        Self::hresult(operation, error.code().0)
    }

    pub(crate) fn last_win32(operation: &'static str) -> Self {
        Self::win32(
            operation,
            unsafe { windows::Win32::Foundation::GetLastError() }.0,
        )
    }

    pub(crate) fn hresult(operation: &'static str, status: i32) -> Self {
        let code = status as u32;
        let kind = match code {
            0x8004_1003 => DiagnosticKind::AccessDenied,
            0x8004_1002 | 0x8004_100e | 0x8004_1010 => DiagnosticKind::Unavailable,
            0x8004_100c | 0x8004_102e => DiagnosticKind::Unsupported,
            0x0004_0004 | 0x8004_1069 => DiagnosticKind::TimedOut,
            0x8007_0000..=0x8007_ffff => Self::win32(operation, code & 0xffff).kind,
            _ => DiagnosticKind::Io,
        };
        Self::native(operation, kind, NativeDomain::Hresult, code)
    }

    pub(crate) fn file(operation: &'static str, error: std::io::Error) -> Self {
        if let Some(code) = error.raw_os_error() {
            return Self::win32(operation, code as u32);
        }
        let kind = match error.kind() {
            std::io::ErrorKind::NotFound => DiagnosticKind::Unavailable,
            std::io::ErrorKind::PermissionDenied => DiagnosticKind::AccessDenied,
            std::io::ErrorKind::TimedOut => DiagnosticKind::TimedOut,
            _ => DiagnosticKind::Io,
        };
        Self::new(operation, kind)
    }

    pub(crate) fn leaves_call_unsettled(&self) -> bool {
        matches!(self.kind, DiagnosticKind::Busy | DiagnosticKind::TimedOut)
    }

    pub(crate) fn conflict(operation: &'static str, detail: impl Into<String>) -> Self {
        Self::new(operation, DiagnosticKind::ConflictDetected).detail(detail)
    }
}

impl From<&Diagnostic> for HalError {
    fn from(error: &Diagnostic) -> Self {
        match error.kind {
            DiagnosticKind::Unsupported => Self::Unsupported,
            DiagnosticKind::Unavailable => Self::Unavailable,
            DiagnosticKind::Busy => Self::Busy,
            DiagnosticKind::ConflictDetected => Self::Busy,
            DiagnosticKind::TimedOut => Self::TimedOut,
            _ => error
                .native_status
                .map_or(Self::Io, |status| Self::NativeIo(status.code)),
        }
    }
}

impl fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {:?}", self.operation, self.kind)?;
        if let Some(status) = self.native_status {
            write!(f, " ({:?} 0x{:08X})", status.domain, status.code)?;
        }
        if let Some(detail) = &self.detail {
            write!(f, ": {detail}")?;
        }
        Ok(())
    }
}

impl std::error::Error for Diagnostic {}
