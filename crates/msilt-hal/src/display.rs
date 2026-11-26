use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::Read;
use std::mem::size_of;
use std::os::windows::ffi::OsStringExt;
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::ptr;
use std::time::Instant;

use serde::Serialize;
use sha2::{Digest, Sha256};
use windows::Win32::Devices::Display::*;
use windows::Win32::Foundation::{
    ERROR_FILE_NOT_FOUND, ERROR_INSUFFICIENT_BUFFER, ERROR_INVALID_PARAMETER, ERROR_NOT_FOUND,
    ERROR_SUCCESS, HLOCAL, LocalFree,
};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::ColorSystem::*;
use windows::core::{PCWSTR, PWSTR};

use crate::executor::before_deadline;
use crate::native::wide_str;
use crate::wmi_acpi::{self, Brightness};
use crate::{Diagnostic, DiagnosticKind, NativeDomain};

const FACTORY_DIR: &str = r"C:\ProgramData\Portrait Displays\TrueColor";
const MAX_ICC_BYTES: usize = 8 * 1024 * 1024;
const MAX_DIRECTORY_ENTRIES: usize = 2048;

#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DisplayReport {
    pub panel: Option<InternalPanel>,
    pub brightness: Option<Brightness>,
    pub color: ColorReport,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InternalPanel {
    pub edid_id: Option<String>,
    pub adapter_vendor_id: Option<u16>,
    pub ccd_output_technology: i32,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub refresh: Option<RefreshRate>,
    pub signal_refresh: Option<RefreshRate>,
    pub supported_refresh_hz: Vec<u16>,
    pub shared_source: bool,
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct RefreshRate {
    pub numerator: u32,
    pub denominator: u32,
}

impl RefreshRate {
    fn checked(value: DISPLAYCONFIG_RATIONAL) -> Option<Self> {
        if value.Numerator == 0 || value.Denominator == 0 {
            return None;
        }
        let refresh = Self {
            numerator: value.Numerator,
            denominator: value.Denominator,
        };
        refresh.rounded_hz().map(|_| refresh)
    }

    pub fn rounded_hz(self) -> Option<u16> {
        if self.denominator == 0 {
            return None;
        }
        let denominator = u64::from(self.denominator);
        u16::try_from((u64::from(self.numerator) + denominator / 2) / denominator)
            .ok()
            .filter(|&hz| hz > 0)
    }
}

#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ColorReport {
    pub association_scope: Option<&'static str>,
    pub associated_profile_ids: Option<Vec<String>>,
    pub default_profile_id: Option<String>,
    pub installed_profile_ids: Option<Vec<String>>,
    pub factory_library_present: Option<bool>,
    pub factory_profiles: Vec<FactoryProfile>,
    #[serde(skip)]
    pub(crate) settable_profile_ids: Vec<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FactoryProfile {
    pub id: String,
    pub has_mhc2: bool,
}

pub(crate) fn inspect(
    deadline: Instant,
    issues: &mut Vec<Diagnostic>,
    inspect_controls: bool,
) -> DisplayReport {
    let mut report = DisplayReport::default();
    let mut settable_color_target = false;
    match panel_path(deadline) {
        Ok((binding, mut panel)) => {
            match gdi_modes(&binding, panel.shared_source, deadline) {
                Ok((width, height, rates)) => {
                    panel.width = Some(width);
                    panel.height = Some(height);
                    panel.supported_refresh_hz = rates;
                }
                Err(error) => issues.push(error),
            }
            match adapter_vendor(&binding) {
                Ok(vendor) => panel.adapter_vendor_id = vendor,
                Err(error) => issues.push(error),
            }
            if let Some(key) = binding.monitor_key.clone() {
                match before_deadline(deadline, "WmiMonitorBrightness")
                    .and_then(|()| wmi_acpi::brightness(key))
                {
                    Ok(value) => report.brightness = Some(value),
                    Err(error) => issues.push(error),
                }
            } else {
                issues.push(Diagnostic::invalid(
                    "CCD monitor identity",
                    "no PnP instance key for brightness matching",
                ));
            }
            let result = before_deadline(deadline, "Windows color association")
                .and_then(|()| color_associations(&binding, &mut report.color, issues));
            if let Err(error) = result {
                issues.push(error);
            } else {
                settable_color_target =
                    inspect_controls && !panel.shared_source && wcs_monitor_name(&binding).is_ok();
            }
            report.panel = Some(panel);
        }
        Err(error) => issues.push(error),
    }
    match color_directory().and_then(|dir| Ok((installed_profiles(&dir, deadline)?, dir))) {
        Ok((profiles, dir)) => {
            if settable_color_target {
                report.color.settable_profile_ids = profiles
                    .iter()
                    .filter_map(|(id, name)| {
                        (Instant::now() < deadline && read_icm(&dir.join(name)).is_ok())
                            .then(|| id.clone())
                    })
                    .collect();
            }
            report.color.installed_profile_ids = Some(profiles.into_keys().collect());
        }
        Err(error) => issues.push(error),
    }
    match factory_profiles(deadline, &mut report.color.factory_profiles, issues) {
        Ok(present) => report.color.factory_library_present = Some(present),
        Err(error) => issues.push(error),
    }
    report
}

struct Binding {
    path: DISPLAYCONFIG_PATH_INFO,
    source: DISPLAYCONFIG_SOURCE_DEVICE_NAME,
    monitor_key: Option<String>,
    monitor_path: [u16; 128],
}

fn panel_path(deadline: Instant) -> Result<(Binding, InternalPanel), Diagnostic> {
    let (paths, modes, virtual_mode) = active_topology(deadline)?;
    let mut panels = paths.iter().filter(|p| {
        p.targetInfo.targetAvailable.as_bool()
            && p.flags & DISPLAYCONFIG_PATH_ACTIVE != 0
            && internal(p.targetInfo.outputTechnology)
    });
    let path = *panels.next().ok_or_else(|| {
        Diagnostic::new("CCD internal panel", DiagnosticKind::Unavailable)
            .detail("no active internal display path")
    })?;
    if panels.next().is_some() {
        return Err(Diagnostic::invalid(
            "CCD internal panel",
            "more than one active internal display path",
        ));
    }
    let target = path.targetInfo;
    let source = path.sourceInfo;
    let shared_source = paths
        .iter()
        .filter(|p| {
            p.flags & DISPLAYCONFIG_PATH_ACTIVE != 0
                && p.sourceInfo.adapterId == source.adapterId
                && p.sourceInfo.id == source.id
        })
        .count()
        > 1;
    let mut name = DISPLAYCONFIG_TARGET_DEVICE_NAME {
        header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
            r#type: DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME,
            size: size_of::<DISPLAYCONFIG_TARGET_DEVICE_NAME>() as u32,
            adapterId: target.adapterId,
            id: target.id,
        },
        ..Default::default()
    };
    device_info(&mut name.header, "CCD GET_TARGET_NAME")?;
    let mut gdi_name = DISPLAYCONFIG_SOURCE_DEVICE_NAME {
        header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
            r#type: DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
            size: size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32,
            adapterId: source.adapterId,
            id: source.id,
        },
        ..Default::default()
    };
    device_info(&mut gdi_name.header, "CCD GET_SOURCE_NAME")?;
    let monitor_key = wide_str(&name.monitorDevicePath).and_then(|path| monitor_key(&path));
    let edid_id = monitor_key
        .as_deref()
        .and_then(|key| key.split('\\').nth(1))
        .map(str::to_owned);
    let target_index = if virtual_mode && path.flags & DISPLAYCONFIG_PATH_SUPPORT_VIRTUAL_MODE != 0
    {
        unsafe { (target.Anonymous.Anonymous._bitfield >> 16) as usize }
    } else {
        unsafe { target.Anonymous.modeInfoIdx as usize }
    };
    let signal_refresh = modes
        .get(target_index)
        .filter(|mode| {
            mode.infoType == DISPLAYCONFIG_MODE_INFO_TYPE_TARGET
                && mode.id == target.id
                && mode.adapterId == target.adapterId
        })
        .and_then(|mode| {
            RefreshRate::checked(unsafe {
                mode.Anonymous.targetMode.targetVideoSignalInfo.vSyncFreq
            })
        });
    let panel = InternalPanel {
        edid_id,
        adapter_vendor_id: None,
        ccd_output_technology: target.outputTechnology.0,
        width: None,
        height: None,
        refresh: RefreshRate::checked(target.refreshRate),
        signal_refresh,
        supported_refresh_hz: Vec::new(),
        shared_source,
    };
    Ok((
        Binding {
            path,
            source: gdi_name,
            monitor_key,
            monitor_path: name.monitorDevicePath,
        },
        panel,
    ))
}

fn active_topology(
    deadline: Instant,
) -> Result<
    (
        Vec<DISPLAYCONFIG_PATH_INFO>,
        Vec<DISPLAYCONFIG_MODE_INFO>,
        bool,
    ),
    Diagnostic,
> {
    let flags = [
        QDC_ONLY_ACTIVE_PATHS | QDC_VIRTUAL_MODE_AWARE | QDC_VIRTUAL_REFRESH_RATE_AWARE,
        QDC_ONLY_ACTIVE_PATHS | QDC_VIRTUAL_MODE_AWARE,
        QDC_ONLY_ACTIVE_PATHS,
    ];
    'flags: for (index, flags) in flags.into_iter().enumerate() {
        for _ in 0..3 {
            before_deadline(deadline, "QueryDisplayConfig")?;
            let mut path_count = 0;
            let mut mode_count = 0;
            let status =
                unsafe { GetDisplayConfigBufferSizes(flags, &mut path_count, &mut mode_count) };
            if status == ERROR_INVALID_PARAMETER && index < 2 {
                continue 'flags;
            }
            if status != ERROR_SUCCESS {
                return Err(Diagnostic::win32("GetDisplayConfigBufferSizes", status.0));
            }
            if path_count > 128 || mode_count > 512 {
                return Err(Diagnostic::invalid(
                    "QueryDisplayConfig",
                    "display topology exceeds bounds",
                ));
            }
            if path_count == 0 {
                return Ok((Vec::new(), Vec::new(), false));
            }
            let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); path_count as usize];
            let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); mode_count as usize];
            let status = unsafe {
                QueryDisplayConfig(
                    flags,
                    &mut path_count,
                    paths.as_mut_ptr(),
                    &mut mode_count,
                    modes.as_mut_ptr(),
                    None,
                )
            };
            match status {
                ERROR_INSUFFICIENT_BUFFER => continue,
                ERROR_INVALID_PARAMETER if index < 2 => continue 'flags,
                ERROR_SUCCESS
                    if path_count as usize <= paths.len() && mode_count as usize <= modes.len() =>
                {
                    paths.truncate(path_count as usize);
                    modes.truncate(mode_count as usize);
                    return Ok((paths, modes, index < 2));
                }
                ERROR_SUCCESS => {
                    return Err(Diagnostic::invalid(
                        "QueryDisplayConfig",
                        "returned counts exceed buffers",
                    ));
                }
                e => return Err(Diagnostic::win32("QueryDisplayConfig", e.0)),
            }
        }
        return Err(Diagnostic::new("QueryDisplayConfig", DiagnosticKind::Busy)
            .detail("display topology changed during all three attempts"));
    }
    unreachable!()
}

fn internal(technology: DISPLAYCONFIG_VIDEO_OUTPUT_TECHNOLOGY) -> bool {
    matches!(
        technology,
        DISPLAYCONFIG_OUTPUT_TECHNOLOGY_INTERNAL
            | DISPLAYCONFIG_OUTPUT_TECHNOLOGY_LVDS
            | DISPLAYCONFIG_OUTPUT_TECHNOLOGY_DISPLAYPORT_EMBEDDED
            | DISPLAYCONFIG_OUTPUT_TECHNOLOGY_UDI_EMBEDDED
    )
}

fn device_info(
    header: *mut DISPLAYCONFIG_DEVICE_INFO_HEADER,
    operation: &'static str,
) -> Result<(), Diagnostic> {
    let status = unsafe { DisplayConfigGetDeviceInfo(header) };
    if status == 0 {
        Ok(())
    } else {
        Err(Diagnostic::win32(operation, status as u32))
    }
}

fn monitor_key(path: &str) -> Option<String> {
    let mut fields = path.strip_prefix(r"\\?\")?.split('#');
    let kind = fields.next()?;
    let edid = fields.next()?;
    let instance = fields.next()?;
    if !kind.eq_ignore_ascii_case("DISPLAY")
        || edid.len() != 7
        || !edid.bytes().all(|b| b.is_ascii_alphanumeric())
        || instance.is_empty()
        || !instance
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'&' | b'_'))
    {
        return None;
    }
    Some(format!(
        "DISPLAY\\{}\\{instance}",
        edid.to_ascii_uppercase()
    ))
}

fn adapter_vendor(binding: &Binding) -> Result<Option<u16>, Diagnostic> {
    let mut adapter = DISPLAYCONFIG_ADAPTER_NAME {
        header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
            r#type: DISPLAYCONFIG_DEVICE_INFO_GET_ADAPTER_NAME,
            size: size_of::<DISPLAYCONFIG_ADAPTER_NAME>() as u32,
            adapterId: binding.path.targetInfo.adapterId,
            id: 0,
        },
        ..Default::default()
    };
    device_info(&mut adapter.header, "CCD GET_ADAPTER_NAME")?;
    Ok(wide_str(&adapter.adapterDevicePath).and_then(|path| {
        path.to_ascii_uppercase()
            .split(['#', '&'])
            .find_map(|part| {
                part.strip_prefix("VEN_")
                    .filter(|id| id.len() == 4)
                    .and_then(|id| u16::from_str_radix(id, 16).ok())
            })
    }))
}

fn gdi_modes(
    binding: &Binding,
    shared: bool,
    deadline: Instant,
) -> Result<(u32, u32, Vec<u16>), Diagnostic> {
    let source = binding.gdi_name()?;
    let current = current_gdi_mode(source)?;
    let rates = if shared {
        BTreeSet::new()
    } else {
        rate_candidates(source, &current, deadline)?
    };
    Ok((
        current.dmPelsWidth,
        current.dmPelsHeight,
        rates.into_iter().collect(),
    ))
}

impl Binding {
    fn gdi_name(&self) -> Result<PCWSTR, Diagnostic> {
        let source = &self.source.viewGdiDeviceName;
        if wide_str(source).is_some_and(|name| !name.is_empty()) {
            Ok(PCWSTR(source.as_ptr()))
        } else {
            Err(Diagnostic::invalid(
                "CCD source name",
                "empty or unterminated GDI device name",
            ))
        }
    }

    fn same_panel(&self, other: &Self) -> bool {
        self.path.sourceInfo.adapterId == other.path.sourceInfo.adapterId
            && self.path.sourceInfo.id == other.path.sourceInfo.id
            && self.path.targetInfo.adapterId == other.path.targetInfo.adapterId
            && self.path.targetInfo.id == other.path.targetInfo.id
            && self.source.viewGdiDeviceName == other.source.viewGdiDeviceName
            && self.monitor_path == other.monitor_path
    }
}

fn current_gdi_mode(source: PCWSTR) -> Result<DEVMODEW, Diagnostic> {
    const OP: &str = "EnumDisplaySettingsW";
    let mode = gdi_mode(source, ENUM_CURRENT_SETTINGS).ok_or_else(|| {
        Diagnostic::new(OP, DiagnosticKind::Unavailable).detail("no current internal display mode")
    })?;
    if mode.dmPelsWidth == 0 || mode.dmPelsHeight == 0 || mode.dmBitsPerPel == 0 {
        return Err(Diagnostic::invalid(OP, "invalid current source mode"));
    }
    Ok(mode)
}

fn same_mode_geometry(a: &DEVMODEW, b: &DEVMODEW) -> bool {
    let geometry = |mode: &DEVMODEW| {
        (
            mode.dmPelsWidth,
            mode.dmPelsHeight,
            mode.dmBitsPerPel,
            unsafe { mode.Anonymous1.Anonymous2.dmDisplayOrientation },
            unsafe { mode.Anonymous2.dmDisplayFlags } & DM_INTERLACED.0,
        )
    };
    geometry(a) == geometry(b)
}

fn mode_rate(mode: &DEVMODEW) -> Option<u16> {
    u16::try_from(mode.dmDisplayFrequency)
        .ok()
        .filter(|&hz| hz > 1)
}

fn rate_candidates(
    source: PCWSTR,
    current: &DEVMODEW,
    deadline: Instant,
) -> Result<BTreeSet<u16>, Diagnostic> {
    const OP: &str = "EnumDisplaySettingsW";
    let mut rates = BTreeSet::new();
    for index in 0..=4096 {
        before_deadline(deadline, OP)?;
        let Some(mode) = gdi_mode(source, ENUM_DISPLAY_SETTINGS_MODE(index)) else {
            return Ok(rates);
        };
        if index == 4096 {
            return Err(Diagnostic::invalid(OP, "too many display modes"));
        }
        if same_mode_geometry(&mode, current) {
            rates.extend(mode_rate(&mode));
        }
    }
    unreachable!()
}

pub(crate) fn set_display_refresh_rate(hz: u16, deadline: Instant) -> Result<u16, Diagnostic> {
    const OP: &str = "ChangeDisplaySettingsExW";
    let (binding, panel) = panel_path(deadline)?;
    if panel.shared_source {
        return Err(Diagnostic::new(OP, DiagnosticKind::Unsupported)
            .detail("the internal panel shares a mirrored source"));
    }
    let source = binding.gdi_name()?;
    let current = current_gdi_mode(source)?;
    let rates = rate_candidates(source, &current, deadline)?;
    let target = requested_mode(current, &rates, hz)?;
    apply_refresh(
        &current,
        &target,
        || current_gdi_mode(source),
        || {
            let (fresh, panel) = panel_path(deadline)?;
            Ok(!panel.shared_source && binding.same_panel(&fresh))
        },
        |mode, flags, op| {
            before_deadline(deadline, op)?;
            display_change_status(
                unsafe { ChangeDisplaySettingsExW(source, Some(mode), None, flags, None) },
                op,
            )
        },
    )
}

fn apply_refresh(
    current: &DEVMODEW,
    target: &DEVMODEW,
    mut read: impl FnMut() -> Result<DEVMODEW, Diagnostic>,
    mut still_bound: impl FnMut() -> Result<bool, Diagnostic>,
    mut change: impl FnMut(&DEVMODEW, CDS_TYPE, &'static str) -> Result<(), Diagnostic>,
) -> Result<u16, Diagnostic> {
    const OP: &str = "ChangeDisplaySettingsExW";
    change(target, CDS_TEST, "ChangeDisplaySettingsExW(CDS_TEST)")?;
    if !still_bound()? {
        return Err(Diagnostic::new(OP, DiagnosticKind::Busy)
            .detail("display binding changed during preflight"));
    }
    let pending = read()?;
    if !same_mode_geometry(current, &pending)
        || current.dmDisplayFrequency != pending.dmDisplayFrequency
    {
        return Err(Diagnostic::new(OP, DiagnosticKind::Busy)
            .detail("display mode changed during preflight"));
    }
    if pending.dmDisplayFrequency != target.dmDisplayFrequency {
        change(
            target,
            CDS_UPDATEREGISTRY,
            "ChangeDisplaySettingsExW(CDS_UPDATEREGISTRY)",
        )?;
    }
    let observed = read()?;
    if !still_bound()? || !same_mode_geometry(current, &observed) {
        return Err(Diagnostic::invalid(
            OP,
            "display binding or mode geometry changed after applying",
        ));
    }
    mode_rate(&observed)
        .ok_or_else(|| Diagnostic::invalid(OP, "current mode has no discrete refresh rate"))
}

fn requested_mode(
    mut current: DEVMODEW,
    rates: &BTreeSet<u16>,
    hz: u16,
) -> Result<DEVMODEW, Diagnostic> {
    if !rates.contains(&hz) {
        return Err(
            Diagnostic::new("EnumDisplaySettingsW", DiagnosticKind::Unsupported)
                .detail("requested refresh rate is not available for the current mode geometry"),
        );
    }
    current.dmDisplayFrequency = u32::from(hz);
    current.dmDriverExtra = 0;
    current.dmFields = DM_DISPLAYFREQUENCY
        | DM_BITSPERPEL
        | DM_PELSWIDTH
        | DM_PELSHEIGHT
        | (current.dmFields & (DM_DISPLAYORIENTATION | DM_DISPLAYFLAGS | DM_DISPLAYFIXEDOUTPUT));
    Ok(current)
}

fn display_change_status(status: DISP_CHANGE, op: &'static str) -> Result<(), Diagnostic> {
    if status == DISP_CHANGE_SUCCESSFUL {
        return Ok(());
    }
    let kind = if status == DISP_CHANGE_BADMODE {
        DiagnosticKind::Unsupported
    } else {
        DiagnosticKind::Rejected
    };
    let error = Diagnostic::native(op, kind, NativeDomain::DisplayChange, status.0 as u32);
    Err(if status == DISP_CHANGE_RESTART {
        error.detail("the new mode was not applied; Windows reports a restart is needed")
    } else {
        error
    })
}

pub(crate) fn set_display_brightness(percent: u8, deadline: Instant) -> Result<u8, Diagnostic> {
    wmi_acpi::validate_brightness(percent)?;
    let (binding, _) = panel_path(deadline)?;
    let key = binding.monitor_key.ok_or_else(|| {
        Diagnostic::invalid(
            "CCD monitor identity",
            "no PnP identity for the internal panel",
        )
    })?;
    before_deadline(deadline, "WmiSetBrightness")?;
    wmi_acpi::set_brightness(key, percent)
}

fn gdi_mode(source: PCWSTR, index: ENUM_DISPLAY_SETTINGS_MODE) -> Option<DEVMODEW> {
    let mut mode = DEVMODEW {
        dmSize: size_of::<DEVMODEW>() as u16,
        ..Default::default()
    };
    unsafe { EnumDisplaySettingsW(source, index, &mut mode) }
        .as_bool()
        .then_some(mode)
}

const COLOR_SCOPE: WCS_PROFILE_MANAGEMENT_SCOPE = WCS_PROFILE_MANAGEMENT_SCOPE_CURRENT_USER;
const CLASS_MONITOR: u32 = u32::from_be_bytes(*b"mntr");
const EDD_GET_DEVICE_INTERFACE_NAME: u32 = 1;

pub(crate) fn set_color_profile(id: &str, deadline: Instant) -> Result<(), Diagnostic> {
    validate_color_profile_id(id)?;
    let dir = color_directory()?;
    let profiles = installed_profiles(&dir, deadline)?;
    let name = resolve_profile(&profiles, id)?;
    read_icm(&dir.join(name))?;

    let (binding, panel) = panel_path(deadline)?;
    if panel.shared_source {
        return Err(
            Diagnostic::new("WCS internal panel", DiagnosticKind::Unsupported)
                .detail("the internal panel shares a mirrored source"),
        );
    }
    let device_name = wcs_monitor_name(&binding)?;
    let device = PCWSTR(device_name.as_ptr());
    let profile_name: Vec<u16> = name.encode_utf16().chain([0]).collect();
    let profile = PCWSTR(profile_name.as_ptr());
    let mut per_user = windows::core::BOOL(0);
    wcs_ok(
        unsafe { WcsGetUsePerUserProfiles(device, CLASS_MONITOR, &mut per_user) },
        "WcsGetUsePerUserProfiles",
    )?;
    let (adapter, source) = color_binding(&binding)?;
    let mut enabled_scope = false;
    let mut added = false;
    let mut reset_default = false;
    let mut previous = None;
    let result = (|| {
        let (verified, panel) = panel_path(deadline)?;
        if !binding.same_panel(&verified) || panel.shared_source || !primary_source(&verified)? {
            return Err(Diagnostic::new("WCS internal panel", DiagnosticKind::Busy)
                .detail("display changed during color preflight"));
        }
        before_deadline(deadline, "Windows color association")?;
        if !per_user.as_bool() {
            wcs_ok(
                unsafe { WcsSetUsePerUserProfiles(device, CLASS_MONITOR, true) },
                "WcsSetUsePerUserProfiles",
            )?;
            enabled_scope = true;
        }
        if display_scope(adapter, source)? != COLOR_SCOPE {
            return Err(Diagnostic::invalid(
                "WcsSetUsePerUserProfiles",
                "user scope was not enabled for the bound display",
            ));
        }
        previous = display_default_name(COLOR_SCOPE, adapter, source)?;
        if !display_profile_names(COLOR_SCOPE, adapter, source)?
            .iter()
            .any(|value| same_profile_name(value, name))
        {
            before_deadline(deadline, "WcsAssociateColorProfileWithDevice")?;
            wcs_ok(
                unsafe { WcsAssociateColorProfileWithDevice(COLOR_SCOPE, profile, device) },
                "WcsAssociateColorProfileWithDevice",
            )?;
            added = true;
        }
        if !previous
            .as_ref()
            .is_some_and(|value| same_profile_name(value, name))
        {
            before_deadline(deadline, "WcsSetDefaultColorProfile")?;
            reset_default = true;
            wcs_default(device, profile)?;
        }
        let observed = wcs_default_name(device)?;
        let (verified, panel) = panel_path(deadline)?;
        let effective_scope = display_scope(adapter, source)?;
        let effective = display_default_name(effective_scope, adapter, source)?;
        if !binding.same_panel(&verified)
            || panel.shared_source
            || !primary_source(&verified)?
            || effective_scope != COLOR_SCOPE
            || !same_profile_name(&observed, name)
            || !effective.is_some_and(|value| same_profile_name(&value, name))
        {
            return Err(Diagnostic::invalid(
                "WcsGetDefaultColorProfile",
                "selected profile is not the effective default for the primary internal panel",
            ));
        }
        Ok(())
    })();
    if let Err(error) = result {
        let mut failures = Vec::new();
        if reset_default {
            let original = previous
                .as_deref()
                .and_then(|value| Path::new(value).file_name()?.to_str());
            let words = original.map(|name| name.encode_utf16().chain([0]).collect::<Vec<_>>());
            if let Err(error) = wcs_default(
                device,
                words
                    .as_ref()
                    .map_or(PCWSTR::null(), |name| PCWSTR(name.as_ptr())),
            ) {
                failures.push(error.to_string());
            }
        }
        if added {
            if let Err(error) = wcs_ok(
                unsafe { WcsDisassociateColorProfileFromDevice(COLOR_SCOPE, profile, device) },
                "WcsDisassociateColorProfileFromDevice",
            ) {
                failures.push(error.to_string());
            }
        }
        if enabled_scope {
            if let Err(error) = wcs_ok(
                unsafe { WcsSetUsePerUserProfiles(device, CLASS_MONITOR, false) },
                "WcsSetUsePerUserProfiles(restore scope)",
            ) {
                failures.push(error.to_string());
            }
        }
        return if failures.is_empty() {
            Err(error)
        } else {
            Err(error
                .clone()
                .detail(format!("{error}; rollback: {}", failures.join("; "))))
        };
    }
    Ok(())
}

fn primary_source(binding: &Binding) -> Result<bool, Diagnostic> {
    let _ = binding.gdi_name()?;
    let source = wide_str(&binding.source.viewGdiDeviceName).expect("validated GDI name");
    for index in 0..128 {
        let mut device = DISPLAY_DEVICEW {
            cb: size_of::<DISPLAY_DEVICEW>() as u32,
            ..Default::default()
        };
        if !unsafe { EnumDisplayDevicesW(PCWSTR::null(), index, &mut device, 0) }.as_bool() {
            break;
        }
        if wide_str(&device.DeviceName).is_some_and(|name| name.eq_ignore_ascii_case(&source)) {
            let flags = DISPLAY_DEVICE_ATTACHED_TO_DESKTOP | DISPLAY_DEVICE_PRIMARY_DEVICE;
            return Ok(device.StateFlags.contains(flags));
        }
    }
    Err(
        Diagnostic::new("EnumDisplayDevicesW", DiagnosticKind::Unavailable)
            .detail("active internal GDI source is missing"),
    )
}

fn wcs_monitor_name(binding: &Binding) -> Result<[u16; 32], Diagnostic> {
    const OP: &str = "WCS internal panel";
    if !primary_source(binding)? {
        return Err(Diagnostic::new(OP, DiagnosticKind::Unsupported)
            .detail("the internal panel is not the primary Windows display"));
    }
    let source = binding.gdi_name()?;
    let monitor = wide_str(&binding.monitor_path)
        .filter(|text| !text.is_empty())
        .ok_or_else(|| Diagnostic::invalid(OP, "missing CCD monitor interface"))?;
    let mut found = None;
    for index in 0..64 {
        let mut device = DISPLAY_DEVICEW {
            cb: size_of::<DISPLAY_DEVICEW>() as u32,
            ..Default::default()
        };
        if !unsafe {
            EnumDisplayDevicesW(source, index, &mut device, EDD_GET_DEVICE_INTERFACE_NAME)
        }
        .as_bool()
        {
            return found.ok_or_else(|| {
                Diagnostic::new(OP, DiagnosticKind::Unavailable)
                    .detail("no GDI monitor matching the internal panel interface")
            });
        }
        if device.StateFlags.contains(DISPLAY_DEVICE_ACTIVE)
            && wide_str(&device.DeviceID).is_some_and(|id| id.eq_ignore_ascii_case(&monitor))
        {
            if !wide_str(&device.DeviceName).is_some_and(|name| !name.is_empty())
                || found.replace(device.DeviceName).is_some()
            {
                return Err(Diagnostic::invalid(
                    OP,
                    "ambiguous or malformed GDI monitor name",
                ));
            }
        }
    }
    Err(Diagnostic::invalid(
        OP,
        "too many GDI monitors for one source",
    ))
}

fn wcs_ok(status: windows::core::BOOL, op: &'static str) -> Result<(), Diagnostic> {
    if status.as_bool() {
        Ok(())
    } else {
        Err(Diagnostic::last_win32(op))
    }
}

fn wcs_default(device: PCWSTR, profile: PCWSTR) -> Result<(), Diagnostic> {
    wcs_ok(
        unsafe { WcsSetDefaultColorProfile(COLOR_SCOPE, device, CPT_ICC, CPST_NONE, 0, profile) },
        "WcsSetDefaultColorProfile",
    )
}

fn wcs_default_name(device: PCWSTR) -> Result<String, Diagnostic> {
    let mut size = 0;
    wcs_ok(
        unsafe {
            WcsGetDefaultColorProfileSize(COLOR_SCOPE, device, CPT_ICC, CPST_NONE, 0, &mut size)
        },
        "WcsGetDefaultColorProfileSize",
    )?;
    if !(2..=65_536).contains(&size) || size % 2 != 0 {
        return Err(Diagnostic::invalid(
            "WcsGetDefaultColorProfileSize",
            "invalid UTF-16 buffer size",
        ));
    }
    let mut buffer = vec![0; (size / 2) as usize];
    wcs_ok(
        unsafe {
            WcsGetDefaultColorProfile(
                COLOR_SCOPE,
                device,
                CPT_ICC,
                CPST_NONE,
                0,
                size,
                PWSTR(buffer.as_mut_ptr()),
            )
        },
        "WcsGetDefaultColorProfile",
    )?;
    wide_str(&buffer)
        .filter(|name| !name.is_empty())
        .ok_or_else(|| Diagnostic::invalid("WcsGetDefaultColorProfile", "invalid profile name"))
}

fn same_profile_name(value: &str, basename: &str) -> bool {
    Path::new(value)
        .file_name()
        .and_then(|os| os.to_str())
        .is_some_and(|name| name.eq_ignore_ascii_case(basename))
}

struct Local<T>(*mut T);
impl<T> Drop for Local<T> {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { LocalFree(Some(HLOCAL(self.0.cast()))) };
        }
    }
}

fn color_associations(
    binding: &Binding,
    color: &mut ColorReport,
    issues: &mut Vec<Diagnostic>,
) -> Result<(), Diagnostic> {
    let (luid, id) = color_binding(binding)?;
    let scope = display_scope(luid, id)?;
    color.association_scope = Some(match scope {
        WCS_PROFILE_MANAGEMENT_SCOPE_CURRENT_USER => "currentUser",
        WCS_PROFILE_MANAGEMENT_SCOPE_SYSTEM_WIDE => "systemWide",
        _ => {
            return Err(Diagnostic::invalid(
                "ColorProfileGetDisplayUserScope",
                "unknown color scope",
            ));
        }
    });
    match display_profile_names(scope, luid, id) {
        Ok(names) => {
            let mut ids = BTreeSet::new();
            for name in names {
                match profile_id_from_path(&name) {
                    Some(id) => {
                        ids.insert(id);
                    }
                    None => issues.push(Diagnostic::invalid(
                        "ColorProfileGetDisplayList",
                        "invalid profile name or ID",
                    )),
                }
            }
            color.associated_profile_ids = Some(ids.into_iter().collect());
        }
        Err(e) => issues.push(e),
    }
    match display_default_name(scope, luid, id) {
        Ok(Some(name)) => {
            color.default_profile_id = profile_id_from_path(&name);
            if color.default_profile_id.is_none() {
                issues.push(Diagnostic::invalid(
                    "ColorProfileGetDisplayDefault",
                    "invalid profile name or ID",
                ));
            }
        }
        Ok(None) => (),
        Err(e) => issues.push(e),
    }
    Ok(())
}

fn display_scope(
    luid: windows::Win32::Foundation::LUID,
    id: u32,
) -> Result<WCS_PROFILE_MANAGEMENT_SCOPE, Diagnostic> {
    unsafe { ColorProfileGetDisplayUserScope(luid, id) }
        .map_err(|e| Diagnostic::windows("ColorProfileGetDisplayUserScope", e))
}

fn color_binding(binding: &Binding) -> Result<(windows::Win32::Foundation::LUID, u32), Diagnostic> {
    let target = binding.path.targetInfo;
    let source = binding.path.sourceInfo;
    if source.adapterId != target.adapterId || source.id == u32::MAX || target.id == u32::MAX {
        return Err(
            Diagnostic::new("Windows color association", DiagnosticKind::Unavailable)
                .detail("ambiguous adapter/source binding"),
        );
    }
    Ok((target.adapterId, source.id))
}

fn display_profile_names(
    scope: WCS_PROFILE_MANAGEMENT_SCOPE,
    luid: windows::Win32::Foundation::LUID,
    id: u32,
) -> Result<Vec<String>, Diagnostic> {
    const OP: &str = "ColorProfileGetDisplayList";
    let mut list = Local::<PWSTR>(ptr::null_mut());
    let mut count = 0;
    match unsafe { ColorProfileGetDisplayList(scope, luid, id, &mut list.0, &mut count) } {
        Ok(()) if count == 0 => Ok(Vec::new()),
        Ok(()) if !list.0.is_null() && count <= 256 => {
            let names = unsafe { std::slice::from_raw_parts(list.0, count as usize) };
            names
                .iter()
                .map(|&name| {
                    wcs_profile_name(name)
                        .ok_or_else(|| Diagnostic::invalid(OP, "invalid profile name"))
                })
                .collect()
        }
        Ok(()) => Err(Diagnostic::invalid(OP, "invalid profile pointer/count")),
        Err(e) if missing_profile(&e) => Ok(Vec::new()),
        Err(e) => Err(Diagnostic::windows(OP, e)),
    }
}

fn display_default_name(
    scope: WCS_PROFILE_MANAGEMENT_SCOPE,
    luid: windows::Win32::Foundation::LUID,
    id: u32,
) -> Result<Option<String>, Diagnostic> {
    const OP: &str = "ColorProfileGetDisplayDefault";
    match unsafe { ColorProfileGetDisplayDefault(scope, luid, id, CPT_ICC, CPST_NONE) } {
        Ok(name) => {
            let _name = Local(name.0);
            wcs_profile_name(name)
                .map(Some)
                .ok_or_else(|| Diagnostic::invalid(OP, "invalid default profile name"))
        }
        Err(e) if missing_profile(&e) => Ok(None),
        Err(e) => Err(Diagnostic::windows(OP, e)),
    }
}

fn missing_profile(error: &windows::core::Error) -> bool {
    [ERROR_FILE_NOT_FOUND, ERROR_NOT_FOUND]
        .iter()
        .any(|code| error.code() == code.to_hresult())
}

fn wcs_profile_name(name: PWSTR) -> Option<String> {
    if name.is_null() {
        return None;
    }
    // Win32 owns a NUL-terminated string; cap scanning before accepting it as a profile name.
    let end = (0..32_768).find(|&pos| unsafe { *name.0.add(pos) == 0 })?;
    (end > 0)
        .then(|| String::from_utf16(unsafe { std::slice::from_raw_parts(name.0, end) }).ok())
        .flatten()
}

fn profile_id_from_path(value: &str) -> Option<String> {
    profile_id(Path::new(value).file_name()?.to_str()?)
}

fn valid_profile_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
}

pub(crate) fn validate_color_profile_id(id: &str) -> Result<(), Diagnostic> {
    if valid_profile_id(id) {
        Ok(())
    } else {
        Err(Diagnostic::invalid(
            "WCS profile ID",
            "expected 1..=64 ASCII letters, digits, hyphens or underscores",
        ))
    }
}

fn resolve_profile<'a>(
    profiles: &'a BTreeMap<String, String>,
    id: &str,
) -> Result<&'a str, Diagnostic> {
    validate_color_profile_id(id)?;
    profiles.get(id).map(String::as_str).ok_or_else(|| {
        Diagnostic::new("WCS profile ID", DiagnosticKind::Unsupported)
            .detail("profile is not in the installed allowlist")
    })
}

fn profile_id(basename: &str) -> Option<String> {
    if basename
        .chars()
        .any(|ch| ch.is_control() || matches!(ch, '\\' | '/' | ':'))
    {
        return None;
    }
    let (stem, ext) = basename.rsplit_once('.')?;
    if !(ext.eq_ignore_ascii_case("icc") || ext.eq_ignore_ascii_case("icm")) || stem.is_empty() {
        return None;
    }
    if ext.eq_ignore_ascii_case("icm")
        && valid_profile_id(stem)
        && !stem.to_ascii_lowercase().starts_with("icc_")
    {
        return Some(stem.to_owned());
    }
    let hash = format!("{:x}", Sha256::digest(basename.to_lowercase().as_bytes()));
    Some(format!("icc_{}", &hash[..32]))
}

fn color_directory() -> Result<PathBuf, Diagnostic> {
    const OP: &str = "GetColorDirectoryW";
    let mut words = [0u16; 1024];
    let mut bytes = size_of_val(&words) as u32;
    if !unsafe { GetColorDirectoryW(PCWSTR::null(), Some(PWSTR(words.as_mut_ptr())), &mut bytes) }
        .as_bool()
    {
        return Err(Diagnostic::last_win32(OP));
    }
    let end = words
        .iter()
        .position(|&w| w == 0)
        .filter(|&n| n > 0)
        .ok_or_else(|| Diagnostic::invalid(OP, "invalid color directory name"))?;
    let dir = PathBuf::from(OsString::from_wide(&words[..end]));
    if !dir.is_absolute() {
        return Err(Diagnostic::invalid(
            OP,
            "expected absolute system color directory",
        ));
    }
    Ok(dir)
}

fn installed_profiles(
    dir: &Path,
    deadline: Instant,
) -> Result<BTreeMap<String, String>, Diagnostic> {
    const OP: &str = "installed ICC directory";
    before_deadline(deadline, OP)?;
    let mut found = BTreeMap::new();
    let mut seen = BTreeSet::new();
    for (index, entry) in fs::read_dir(dir)
        .map_err(|e| Diagnostic::file(OP, e))?
        .enumerate()
    {
        before_deadline(deadline, OP)?;
        if index == MAX_DIRECTORY_ENTRIES {
            return Err(Diagnostic::invalid(OP, "too many directory entries"));
        }
        let entry = entry.map_err(|e| Diagnostic::file(OP, e))?;
        if !entry
            .file_type()
            .map_err(|e| Diagnostic::file(OP, e))?
            .is_file()
        {
            continue;
        }
        let name = entry.file_name();
        if let Some((id, name)) = name
            .to_str()
            .and_then(|name| Some((profile_id(name)?, name)))
        {
            let metadata = entry.metadata().map_err(|e| Diagnostic::file(OP, e))?;
            const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
            if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                continue;
            }
            if !seen.insert(id.to_ascii_lowercase()) {
                return Err(Diagnostic::invalid(OP, "ambiguous installed profile ID"));
            }
            found.insert(id, name.to_owned());
        }
    }
    Ok(found)
}

fn factory_profiles(
    deadline: Instant,
    found: &mut Vec<FactoryProfile>,
    issues: &mut Vec<Diagnostic>,
) -> Result<bool, Diagnostic> {
    const OP: &str = "True Color factory ICM directory";
    before_deadline(deadline, OP)?;
    let entries = match fs::read_dir(FACTORY_DIR) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(Diagnostic::file(OP, e)),
    };
    for (index, entry) in entries.enumerate() {
        before_deadline(deadline, OP)?;
        if index == MAX_DIRECTORY_ENTRIES {
            return Err(Diagnostic::invalid(OP, "too many directory entries"));
        }
        let entry = entry.map_err(|e| Diagnostic::file(OP, e))?;
        let basename = entry.file_name();
        let Some((stem, ext)) = basename.to_str().and_then(|name| name.rsplit_once('.')) else {
            continue;
        };
        if !ext.eq_ignore_ascii_case("icm") {
            continue;
        }
        if !valid_profile_id(stem) || stem.to_ascii_lowercase().starts_with("icc_") {
            issues.push(Diagnostic::invalid(
                OP,
                "factory file has no valid ASCII profile ID",
            ));
            continue;
        }
        if !entry
            .file_type()
            .map_err(|e| Diagnostic::file(OP, e))?
            .is_file()
        {
            continue;
        }
        match read_icm(&entry.path()) {
            Ok(has_mhc2) => found.push(FactoryProfile {
                id: stem.to_owned(),
                has_mhc2,
            }),
            Err(error) => issues.push(error.detail(stem)),
        }
    }
    found.sort_unstable_by(|a, b| a.id.cmp(&b.id));
    Ok(true)
}

fn read_icm(path: &Path) -> Result<bool, Diagnostic> {
    const OP: &str = "True Color factory ICC header/tags";
    let mut data = Vec::new();
    File::open(path)
        .map_err(|e| Diagnostic::file(OP, e))?
        .take(MAX_ICC_BYTES as u64 + 1)
        .read_to_end(&mut data)
        .map_err(|e| Diagnostic::file(OP, e))?;
    icc_has_mhc2(&data).ok_or_else(|| {
        Diagnostic::invalid(OP, "invalid display ICC size, signature, or tag boundaries")
    })
}

fn icc_has_mhc2(data: &[u8]) -> Option<bool> {
    if !(132..=MAX_ICC_BYTES).contains(&data.len())
        || data[12..16] != *b"mntr"
        || data[16..20] != *b"RGB "
        || data[36..40] != *b"acsp"
    {
        return None;
    }
    let u32_at = |offset: usize| -> Option<usize> {
        Some(u32::from_be_bytes(data.get(offset..offset + 4)?.try_into().ok()?) as usize)
    };
    if u32_at(0)? != data.len() {
        return None;
    }
    let count = u32_at(128)?;
    let table_end = count.checked_mul(12)?.checked_add(132)?;
    let tags = data.get(132..table_end)?;
    let mut signatures = BTreeSet::new();
    for (i, tag) in tags.chunks_exact(12).enumerate() {
        let offset = u32_at(132 + 12 * i + 4)?;
        let size = u32_at(132 + 12 * i + 8)?;
        if offset < table_end
            || offset % 4 != 0
            || size < 8
            || offset.checked_add(size)? > data.len()
            || !signatures.insert(&tag[..4])
        {
            return None;
        }
    }
    Some(signatures.contains(b"MHC2".as_slice()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn panel_ids_and_profile_ids_do_not_embed_paths() {
        assert_eq!(
            monitor_key(r"\\?\DISPLAY#AUO84A6#5&ABC&0&UID1#{guid}").as_deref(),
            Some(r"DISPLAY\AUO84A6\5&ABC&0&UID1")
        );
        assert!(monitor_key(r"\\?\DISPLAY#AUO84A6#bad/path#{guid}").is_none());
        assert!(profile_id(r"C:\Users\someone\x.icm").is_none());
        assert_eq!(
            profile_id("TrueColor_Native_6500.icm").as_deref(),
            Some("TrueColor_Native_6500")
        );
        assert!(profile_id("sRGB Color Space Profile.icm").is_some_and(|id| valid_profile_id(&id)));
        assert_eq!(
            (RefreshRate {
                numerator: 59_940,
                denominator: 1000
            })
            .rounded_hz(),
            Some(60)
        );
        assert_eq!(
            (RefreshRate {
                numerator: 60,
                denominator: 0
            })
            .rounded_hz(),
            None
        );
    }

    #[test]
    fn icc_requires_a_bounded_tag_payload() {
        let mut icc = [0; 152];
        icc[..4].copy_from_slice(&152u32.to_be_bytes());
        icc[12..20].copy_from_slice(b"mntrRGB ");
        icc[36..40].copy_from_slice(b"acsp");
        icc[128..132].copy_from_slice(&1u32.to_be_bytes());
        icc[132..136].copy_from_slice(b"MHC2");
        icc[136..140].copy_from_slice(&144u32.to_be_bytes());
        icc[140..144].copy_from_slice(&8u32.to_be_bytes());
        assert_eq!(icc_has_mhc2(&icc), Some(true));
        icc[139] = 0;
        assert_eq!(icc_has_mhc2(&icc), None);
    }

    #[test]
    fn refresh_request_keeps_geometry_and_uses_only_enumerated_hz() {
        let mut current = DEVMODEW {
            dmPelsWidth: 1600,
            dmPelsHeight: 2560,
            dmBitsPerPel: 32,
            dmDisplayFrequency: 240,
            dmFields: DM_POSITION | DM_DISPLAYORIENTATION | DM_DISPLAYFLAGS,
            dmDriverExtra: 16,
            ..Default::default()
        };
        current.Anonymous1.Anonymous2.dmDisplayOrientation = DMDO_90;
        let rates = BTreeSet::from([60, 240]);
        let target = requested_mode(current, &rates, 60).ok().unwrap();
        assert!(same_mode_geometry(&current, &target));
        assert_eq!(target.dmDriverExtra, 0);
        assert!(!target.dmFields.contains(DM_POSITION));
        assert!(
            target
                .dmFields
                .contains(DM_DISPLAYORIENTATION | DM_DISPLAYFREQUENCY)
        );
        assert_eq!(mode_rate(&target), Some(60));
        let mut changed = target;
        changed.Anonymous1.Anonymous2.dmDisplayOrientation = DMDO_270;
        assert!(!same_mode_geometry(&current, &changed));
        changed = target;
        changed.Anonymous2.dmDisplayFlags = DM_INTERLACED.0;
        assert!(!same_mode_geometry(&current, &changed));
        changed = target;
        changed.dmBitsPerPel = 24;
        assert!(!same_mode_geometry(&current, &changed));
        assert_eq!(
            requested_mode(current, &rates, 120).err().unwrap().kind,
            DiagnosticKind::Unsupported
        );
    }

    #[test]
    fn display_api_restart_and_bad_modes_are_not_success() {
        assert!(display_change_status(DISP_CHANGE_SUCCESSFUL, "test").is_ok());
        let restart = display_change_status(DISP_CHANGE_RESTART, "test").unwrap_err();
        assert_eq!(restart.kind, DiagnosticKind::Rejected);
        assert_eq!(restart.native_status.unwrap().code, 1);
        assert_eq!(
            display_change_status(DISP_CHANGE_BADMODE, "test")
                .unwrap_err()
                .kind,
            DiagnosticKind::Unsupported
        );
    }

    #[test]
    fn refresh_transaction_preflights_then_returns_readback_not_request() {
        let current = DEVMODEW {
            dmPelsWidth: 1920,
            dmPelsHeight: 1080,
            dmBitsPerPel: 32,
            dmDisplayFrequency: 60,
            ..Default::default()
        };
        let target = DEVMODEW {
            dmDisplayFrequency: 240,
            ..current
        };
        let readback = DEVMODEW {
            dmDisplayFrequency: 239,
            ..current
        };
        let mut reads = [current, readback].into_iter();
        let mut flags = Vec::new();
        let hz = apply_refresh(
            &current,
            &target,
            || Ok(reads.next().unwrap()),
            || Ok(true),
            |mode, flag, _| {
                assert_eq!(mode.dmDisplayFrequency, 240);
                flags.push(flag);
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(hz, 239);
        assert_eq!(flags, [CDS_TEST, CDS_UPDATEREGISTRY]);
        flags.clear();
        let error = apply_refresh(
            &current,
            &target,
            || panic!("no post-test read"),
            || panic!("no topology query"),
            |_, flag, op| {
                flags.push(flag);
                display_change_status(DISP_CHANGE_BADMODE, op)
            },
        )
        .unwrap_err();
        assert_eq!(flags, [CDS_TEST]);
        assert_eq!(error.kind, DiagnosticKind::Unsupported);
    }

    #[test]
    fn color_write_resolves_only_enumerated_ids() {
        let display_name = "Vendor Display 1.icm";
        let id = profile_id(display_name).unwrap();
        let profiles = BTreeMap::from([(id.clone(), display_name.to_owned())]);
        assert_eq!(resolve_profile(&profiles, &id).unwrap(), display_name);
        assert_eq!(
            resolve_profile(&profiles, "unknown").unwrap_err().kind,
            DiagnosticKind::Unsupported
        );
        for bad in [
            "",
            "../vendor.icm",
            "x/y",
            "x.y",
            "x\0",
            "设备",
            &"a".repeat(65),
        ] {
            assert_eq!(
                resolve_profile(&profiles, bad).unwrap_err().kind,
                DiagnosticKind::InvalidData
            );
        }
        assert!(validate_color_profile_id(&"a".repeat(64)).is_ok());
        assert!(same_profile_name(
            r"C:\Windows\System32\spool\drivers\color\vendor display 1.icm",
            display_name
        ));
        assert!(!same_profile_name("Vendor Display 1.icc", display_name));
    }
}
