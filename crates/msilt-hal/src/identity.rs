use std::ffi::c_void;
use std::mem::size_of;

use serde::Serialize;
use windows::Win32::Foundation::{
    ERROR_FILE_NOT_FOUND, ERROR_NO_MORE_FILES, ERROR_PATH_NOT_FOUND, ERROR_SERVICE_DOES_NOT_EXIST,
    ERROR_SUCCESS, HANDLE,
};
use windows::Win32::Security::{
    CheckTokenMembership, CreateWellKnownSid, GetTokenInformation, PSID, TOKEN_ELEVATION,
    TOKEN_QUERY, TokenElevation, WinBuiltinAdministratorsSid,
};
use windows::Win32::System::Diagnostics::ToolHelp::*;
use windows::Win32::System::Registry::{
    HKEY_LOCAL_MACHINE, RRF_RT_REG_DWORD, RRF_RT_REG_SZ, RegGetValueW,
};
use windows::Win32::System::Services::*;
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows::core::{BOOL, PCWSTR, w};

use crate::native::{OwnedHandle, wide_str};
use crate::{Diagnostic, DiagnosticKind, NativeDomain};

#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SystemIdentity {
    pub model: Option<String>,
    pub board_id: Option<String>,
    pub board_revision: Option<String>,
    pub bios_version: Option<String>,
    pub elevated: Option<bool>,
    pub secure_boot_enabled: Option<bool>,
    pub hvci_running: Option<bool>,
    pub vulnerable_driver_blocklist_policy_enabled: Option<bool>,
}

#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConflictReport {
    pub running_processes: Vec<&'static str>,
    pub running_services: Vec<&'static str>,
    pub process_enumeration_complete: bool,
    pub service_query_complete: bool,
}

const BIOS_KEY: PCWSTR = w!(r"HARDWARE\DESCRIPTION\System\BIOS");
const SECURE_BOOT_KEY: PCWSTR = w!(r"SYSTEM\CurrentControlSet\Control\SecureBoot\State");
const CI_KEY: PCWSTR = w!(r"SYSTEM\CurrentControlSet\Control\CI\Config");

const MSI_PROCESSES: &[&str] = &[
    "LEDKeeper2.exe",
    "LightKeeperService.exe",
    "Mystic_Light_Service.exe",
    "MsiTrueColorService.exe",
    "MSI.CentralServer.exe",
    "MSI_Central_Service.exe",
    "MSIAPService.exe",
    "Sendevsvc.exe",
    "MSIService.exe",
    "OmApSvcBroker.exe",
    "MSI.Center.Server.exe",
    "MSI Center.exe",
    "gpuControl.exe",
];
const MSI_SERVICES: &[&str] = &[
    "MSI_Center_Service",
    "MSI Foundation Service",
    "MSI Sendevsvc",
    "Micro Star SCM",
    "Mystic_Light_Service",
    "LightKeeperService",
    "MSITrueColorService",
    "LEDKeeper2",
];
const LIGHTING_SERVICES: &[&str] = &["LEDKeeper2", "Mystic_Light_Service", "LightKeeperService"];
const EC_SERVICES: &[&str] = &[
    "MSI_Center_Service",
    "MSI Foundation Service",
    "MSI Sendevsvc",
    "Micro Star SCM",
];

pub(crate) fn inspect(issues: &mut Vec<Diagnostic>) -> SystemIdentity {
    let mut collect = |result: Result<Option<bool>, Diagnostic>| match result {
        Ok(value) => value,
        Err(error) => {
            issues.push(error);
            None
        }
    };
    let elevated = collect(is_elevated().map(Some));
    let secure_boot_enabled = collect(read_bool(
        SECURE_BOOT_KEY,
        w!("UEFISecureBootEnabled"),
        "Secure Boot",
    ));
    let hvci_running = collect(hvci_running().map(Some));
    let vulnerable_driver_blocklist_policy_enabled = collect(read_bool(
        CI_KEY,
        w!("VulnerableDriverBlocklistEnable"),
        "VulnerableDriverBlocklist policy",
    ));

    let mut collect = |value, op| match read_bios(value, op) {
        Ok(text) => Some(text),
        Err(error) => {
            issues.push(error);
            None
        }
    };
    SystemIdentity {
        model: collect(w!("SystemProductName"), "SMBIOS system product"),
        board_id: collect(w!("BaseBoardProduct"), "SMBIOS board ID"),
        board_revision: collect(w!("BaseBoardVersion"), "SMBIOS board revision"),
        bios_version: collect(w!("BIOSVersion"), "SMBIOS BIOS version"),
        elevated,
        secure_boot_enabled,
        hvci_running,
        vulnerable_driver_blocklist_policy_enabled,
    }
}

pub(crate) fn read_bios(value: PCWSTR, operation: &'static str) -> Result<String, Diagnostic> {
    let mut buffer = [0u16; 256];
    let mut len = size_of_val(&buffer) as u32;
    let code = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            BIOS_KEY,
            value,
            RRF_RT_REG_SZ,
            None,
            Some(buffer.as_mut_ptr().cast()),
            Some(&mut len),
        )
    };
    if code != ERROR_SUCCESS {
        return Err(Diagnostic::win32(operation, code.0));
    }
    if len == 0 || len % 2 != 0 || len > size_of_val(&buffer) as u32 {
        return Err(Diagnostic::invalid(operation, "invalid REG_SZ length"));
    }
    wide_str(&buffer[..len as usize / 2])
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty() && !s.chars().any(char::is_control))
        .ok_or_else(|| Diagnostic::invalid(operation, "missing or invalid SMBIOS string"))
}

fn read_bool(
    key: PCWSTR,
    value: PCWSTR,
    operation: &'static str,
) -> Result<Option<bool>, Diagnostic> {
    let mut flag = 0u32;
    let mut len = size_of::<u32>() as u32;
    match unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            key,
            value,
            RRF_RT_REG_DWORD,
            None,
            Some((&mut flag as *mut u32).cast()),
            Some(&mut len),
        )
    } {
        ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND => Ok(None),
        ERROR_SUCCESS if len == size_of::<u32>() as u32 && flag <= 1 => Ok(Some(flag == 1)),
        ERROR_SUCCESS => Err(Diagnostic::invalid(operation, "expected a boolean DWORD")),
        status => Err(Diagnostic::win32(operation, status.0)),
    }
}

pub(crate) fn is_elevated() -> Result<bool, Diagnostic> {
    let mut token = HANDLE::default();
    unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) }
        .map_err(|e| Diagnostic::windows("OpenProcessToken", e))?;
    let token = OwnedHandle(token);
    let mut elevation = TOKEN_ELEVATION::default();
    let mut size = 0;
    unsafe {
        GetTokenInformation(
            token.0,
            TokenElevation,
            Some((&mut elevation as *mut TOKEN_ELEVATION).cast()),
            size_of::<TOKEN_ELEVATION>() as u32,
            &mut size,
        )
    }
    .map_err(|e| Diagnostic::windows("GetTokenInformation(TokenElevation)", e))?;
    if size as usize != size_of::<TOKEN_ELEVATION>() {
        return Err(Diagnostic::invalid(
            "GetTokenInformation(TokenElevation)",
            "unexpected token length",
        ));
    }
    Ok(elevation.TokenIsElevated != 0)
}

pub(crate) fn require_admin() -> Result<(), Diagnostic> {
    let denied = || {
        Diagnostic::new("MSI_ACPI authorization", DiagnosticKind::AccessDenied)
            .detail("--acpi requires an elevated administrator process")
    };
    if !is_elevated()? {
        return Err(denied());
    }
    let mut sid_words = [0u32; 17];
    let sid = PSID(sid_words.as_mut_ptr().cast());
    let mut bytes = size_of_val(&sid_words) as u32;
    unsafe { CreateWellKnownSid(WinBuiltinAdministratorsSid, None, Some(sid), &mut bytes) }
        .map_err(|e| Diagnostic::windows("CreateWellKnownSid(Administrators)", e))?;
    let mut member = BOOL(0);
    unsafe { CheckTokenMembership(None, sid, &mut member) }
        .map_err(|e| Diagnostic::windows("CheckTokenMembership(Administrators)", e))?;
    if member.as_bool() {
        Ok(())
    } else {
        Err(denied())
    }
}

#[repr(C)]
struct CodeIntegrityInformation {
    length: u32,
    options: u32,
}

// The winternl.h SystemCodeIntegrityInformation ABI is not in the Win32 bindings.
#[link(name = "ntdll")]
unsafe extern "system" {
    fn NtQuerySystemInformation(
        class: i32,
        info: *mut c_void,
        size: u32,
        result_size: *mut u32,
    ) -> i32;
}

fn hvci_running() -> Result<bool, Diagnostic> {
    const OP: &str = "NtQuerySystemInformation(SystemCodeIntegrityInformation)";
    const SYSTEM_CODE_INTEGRITY_INFORMATION: i32 = 103;
    const HVCI_KMCI_ENABLED: u32 = 0x400;
    let size = size_of::<CodeIntegrityInformation>() as u32;
    let mut info = CodeIntegrityInformation {
        length: size,
        options: 0,
    };
    let mut returned = 0;
    let status = unsafe {
        NtQuerySystemInformation(
            SYSTEM_CODE_INTEGRITY_INFORMATION,
            (&mut info as *mut CodeIntegrityInformation).cast(),
            size,
            &mut returned,
        )
    };
    if status != 0 {
        let kind = if status as u32 == 0xc000_0022 {
            DiagnosticKind::AccessDenied
        } else {
            DiagnosticKind::Io
        };
        return Err(Diagnostic::native(
            OP,
            kind,
            NativeDomain::NtStatus,
            status as u32,
        ));
    }
    if returned != size || info.length != size {
        return Err(Diagnostic::invalid(
            OP,
            "unexpected code integrity structure length",
        ));
    }
    Ok(info.options & HVCI_KMCI_ENABLED != 0)
}

pub(crate) fn conflicts(issues: &mut Vec<Diagnostic>) -> ConflictReport {
    query_conflicts(MSI_SERVICES, issues)
}

pub(crate) fn lighting_conflicts(issues: &mut Vec<Diagnostic>) -> ConflictReport {
    query_conflicts(LIGHTING_SERVICES, issues)
}

pub(crate) fn ec_conflicts(issues: &mut Vec<Diagnostic>) -> ConflictReport {
    query_conflicts(EC_SERVICES, issues)
}

fn query_conflicts(services: &[&'static str], issues: &mut Vec<Diagnostic>) -> ConflictReport {
    let mut found = ConflictReport::default();
    match running_processes(&mut found.running_processes) {
        Ok(()) => found.process_enumeration_complete = true,
        Err(error) => issues.push(error),
    }
    found.service_query_complete = running_services(services, &mut found.running_services, issues);
    found
}

fn running_processes(list: &mut Vec<&'static str>) -> Result<(), Diagnostic> {
    let snapshot = OwnedHandle(
        unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }
            .map_err(|e| Diagnostic::windows("CreateToolhelp32Snapshot(processes)", e))?,
    );
    let mut entry = PROCESSENTRY32W {
        dwSize: size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    let mut next = unsafe { Process32FirstW(snapshot.0, &mut entry) };
    for _ in 0..65_536 {
        match next {
            Err(e) if e.code() == ERROR_NO_MORE_FILES.to_hresult() => {
                list.sort_unstable();
                return Ok(());
            }
            Err(e) => return Err(Diagnostic::windows("ToolHelp process enumeration", e)),
            Ok(()) => (),
        }
        if let Some(name) = wide_str(&entry.szExeFile) {
            if let Some(&known) = MSI_PROCESSES
                .iter()
                .find(|known| name.eq_ignore_ascii_case(known))
            {
                if !list.contains(&known) {
                    list.push(known);
                }
            }
        }
        next = unsafe { Process32NextW(snapshot.0, &mut entry) };
    }
    Err(Diagnostic::invalid(
        "ToolHelp process enumeration",
        "too many processes",
    ))
}

struct ScHandle(SC_HANDLE);

impl Drop for ScHandle {
    fn drop(&mut self) {
        let _ = unsafe { CloseServiceHandle(self.0) };
    }
}

fn running_services(
    names: &[&'static str],
    list: &mut Vec<&'static str>,
    issues: &mut Vec<Diagnostic>,
) -> bool {
    let manager =
        match unsafe { OpenSCManagerW(PCWSTR::null(), PCWSTR::null(), SC_MANAGER_CONNECT) } {
            Ok(handle) => ScHandle(handle),
            Err(e) => {
                issues.push(Diagnostic::windows("OpenSCManagerW(query only)", e));
                return false;
            }
        };
    let mut complete = true;
    for &name in names {
        let wide: Vec<u16> = name.encode_utf16().chain([0]).collect();
        let handle =
            match unsafe { OpenServiceW(manager.0, PCWSTR(wide.as_ptr()), SERVICE_QUERY_STATUS) } {
                Ok(handle) => ScHandle(handle),
                Err(e) if e.code() == ERROR_SERVICE_DOES_NOT_EXIST.to_hresult() => continue,
                Err(e) => {
                    complete = false;
                    issues.push(Diagnostic::windows("OpenServiceW(query only)", e).detail(name));
                    continue;
                }
            };
        let mut status = SERVICE_STATUS::default();
        match unsafe { QueryServiceStatus(handle.0, &mut status) } {
            Ok(()) if status.dwCurrentState != SERVICE_STOPPED => list.push(name),
            Ok(()) => (),
            Err(e) => {
                complete = false;
                issues.push(Diagnostic::windows("QueryServiceStatus", e).detail(name));
            }
        }
    }
    complete
}
