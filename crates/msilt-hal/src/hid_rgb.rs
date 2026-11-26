use std::mem::size_of;
use std::time::Instant;

use msilt_core::{KEYBOARD_ZONE_COUNT, Rgb8};
use serde::Serialize;
use windows::Win32::Devices::HumanInterfaceDevice::*;
use windows::Win32::Foundation::{ERROR_SHARING_VIOLATION, GENERIC_READ, GENERIC_WRITE};

use crate::executor::before_deadline;
use crate::identity::{self, ConflictReport};
use crate::native::{NamedMutex, OwnedHandle, device_interfaces, wide_str};
use crate::{Diagnostic, DiagnosticKind, NativeDomain};

const VID: u16 = 0x1462;
const PID: u16 = 0x1603;
const REPORT_BYTES: usize = 64;
const REPORT_ID: u8 = 2;
const OP: &str = "MS-1603 keyboard lighting";
type FeatureReport = [u8; REPORT_BYTES];

#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HidRgbReport {
    pub collections: Vec<HidCollection>,
    pub enumeration_complete: bool,
    pub oem_conflict: Option<bool>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HidCollection {
    pub vendor_id: u16,
    pub product_id: u16,
    pub revision: u16,
    pub usage_page: u16,
    pub usage: u16,
    pub input_report_bytes: u16,
    pub output_report_bytes: u16,
    pub feature_report_bytes: u16,
    #[serde(skip)]
    supports_feature_02: bool,
}

pub(crate) fn inspect(deadline: Instant, issues: &mut Vec<Diagnostic>) -> HidRgbReport {
    let mut report = HidRgbReport::default();
    let mut incomplete = false;
    if let Err(error) =
        device_interfaces(&unsafe { HidD_GetHidGuid() }, deadline, |path| {
            match path.and_then(|path| read_collection(&path, deadline)) {
                Ok(Some(collection)) => report.collections.push(collection),
                Ok(None) => (),
                Err(error) => {
                    incomplete = true;
                    issues.push(error);
                }
            }
        })
    {
        incomplete = true;
        issues.push(error);
    }
    report.enumeration_complete = !incomplete;
    report
}

pub(crate) fn rgb_conflict(conflicts: &ConflictReport) -> Option<bool> {
    let active = conflicts.running_processes.iter().any(|name| {
        matches!(
            *name,
            "LEDKeeper2.exe" | "LightKeeperService.exe" | "Mystic_Light_Service.exe"
        )
    }) || conflicts.running_services.iter().any(|name| {
        matches!(
            *name,
            "LEDKeeper2" | "LightKeeperService" | "Mystic_Light_Service"
        )
    });
    if active {
        Some(true)
    } else if conflicts.process_enumeration_complete && conflicts.service_query_complete {
        Some(false)
    } else {
        None
    }
}

pub(crate) fn set_keyboard_lighting(
    zones: &[Rgb8; KEYBOARD_ZONE_COUNT],
    allow_oem_conflict: bool,
    deadline: Instant,
) -> Result<(), Diagnostic> {
    before_deadline(deadline, OP)?;
    let _mutex = NamedMutex::msi_software(OP)?;
    verify_conflicts(allow_oem_conflict)?;
    let path = lighting_path(deadline)?;
    before_deadline(deadline, OP)?;
    let handle =
        OwnedHandle::open_device(&path, (GENERIC_READ | GENERIC_WRITE).0).map_err(|e| {
            if e.code() == ERROR_SHARING_VIOLATION.to_hresult() {
                Diagnostic::native(
                    "CreateFileW(MS-1603)",
                    DiagnosticKind::ConflictDetected,
                    NativeDomain::Win32,
                    ERROR_SHARING_VIOLATION.0,
                )
            } else {
                Diagnostic::windows("CreateFileW(MS-1603)", e)
            }
        })?;
    if !describe(&handle, deadline)?.lighting_compatible() {
        return Err(Diagnostic::new(OP, DiagnosticKind::Unsupported)
            .detail("HID collection changed before transmission"));
    }
    verify_conflicts(allow_oem_conflict)?;
    send_frame(zones, |report| {
        before_deadline(deadline, "HidD_SetFeature")?;
        if unsafe { HidD_SetFeature(handle.0, report.as_ptr().cast(), REPORT_BYTES as u32) } {
            Ok(())
        } else {
            Err(Diagnostic::last_win32("HidD_SetFeature"))
        }
    })
}

fn verify_conflicts(allow_oem_conflict: bool) -> Result<(), Diagnostic> {
    let mut issues = Vec::new();
    let conflicts = identity::lighting_conflicts(&mut issues);
    check_conflicts(&conflicts, allow_oem_conflict)
}

fn check_conflicts(conflicts: &ConflictReport, allow_oem_conflict: bool) -> Result<(), Diagnostic> {
    match rgb_conflict(conflicts) {
        Some(true) if !allow_oem_conflict => Err(Diagnostic::conflict(
            OP,
            "LEDKeeper2, Mystic Light or LightKeeper is active",
        )),
        Some(false) => Ok(()),
        _ if conflicts.process_enumeration_complete && conflicts.service_query_complete => Ok(()),
        _ => Err(Diagnostic::new(OP, DiagnosticKind::Unavailable)
            .detail("could not verify both OEM process and service state")),
    }
}

fn lighting_path(deadline: Instant) -> Result<Vec<u16>, Diagnostic> {
    let mut found = None;
    let mut failure = None;
    device_interfaces(&unsafe { HidD_GetHidGuid() }, deadline, |path| {
        let candidate = path.and_then(|path| {
            if !lighting_path_matches(&path)? {
                return Ok(None);
            }
            before_deadline(deadline, "CreateFileW(HID)")?;
            let handle = OwnedHandle::open_device(&path, 0)
                .map_err(|e| Diagnostic::windows("CreateFileW(HID, access=0)", e))?;
            Ok(describe(&handle, deadline)?
                .lighting_compatible()
                .then_some(path))
        });
        match candidate {
            Ok(Some(path)) if found.is_none() => found = Some(path),
            Ok(Some(_)) => {
                failure.get_or_insert_with(|| {
                    Diagnostic::invalid(OP, "multiple matching HID collections")
                });
            }
            Err(error) => {
                failure.get_or_insert(error);
            }
            Ok(None) => (),
        }
    })?;
    if let Some(error) = failure {
        return Err(error);
    }
    found.ok_or_else(|| {
        Diagnostic::new(OP, DiagnosticKind::Unavailable)
            .detail("no unique FEAT 0x02 collection with usage FF/01 and a 64-byte report")
    })
}

impl HidCollection {
    pub(crate) fn lighting_compatible(&self) -> bool {
        (self.usage_page, self.usage, self.feature_report_bytes) == (0x00FF, 1, REPORT_BYTES as u16)
            && self.supports_feature_02
    }
}

fn has_lighting_report(data: &Preparsed, caps: &HIDP_CAPS) -> bool {
    let mut count = caps.NumberFeatureValueCaps;
    if count == 0 {
        return false;
    }
    let mut value_caps = vec![HIDP_VALUE_CAPS::default(); usize::from(count)];
    let status = unsafe {
        HidP_GetValueCaps(HidP_Feature, value_caps.as_mut_ptr(), &mut count, data.0)
    };
    if status != HIDP_STATUS_SUCCESS {
        return false;
    }
    value_caps.truncate(usize::from(count));
    value_caps
        .iter()
        .any(|cap| feature_value_cap_matches(cap.ReportID, cap.BitSize, cap.ReportCount))
}

fn feature_value_cap_matches(report_id: u8, bit_size: u16, report_count: u16) -> bool {
    report_id == REPORT_ID
        && bit_size == 8
        && usize::from(report_count).saturating_add(1) == REPORT_BYTES
}

fn send_frame<E>(
    zones: &[Rgb8; KEYBOARD_ZONE_COUNT],
    mut feature: impl FnMut(&FeatureReport) -> Result<(), E>,
) -> Result<(), E> {
    let mut remaining = (1u32 << KEYBOARD_ZONE_COUNT) - 1;
    while remaining != 0 {
        let color = zones[remaining.trailing_zeros() as usize];
        let mask =
            zones.iter().enumerate().fold(
                0,
                |mask, (i, rgb)| {
                    if *rgb == color { mask | (1 << i) } else { mask }
                },
            );
        let [select, colors] = reports(mask & remaining, color);
        feature(&select)?;
        feature(&colors)?;
        remaining &= !mask;
    }
    Ok(())
}

fn reports(mask: u32, Rgb8 { r, g, b }: Rgb8) -> [FeatureReport; 2] {
    let mut select = [0; REPORT_BYTES];
    select[..2].copy_from_slice(&[REPORT_ID, 0x01]);
    select[2..6].copy_from_slice(&mask.to_le_bytes());
    let mut colors = [0; REPORT_BYTES];
    // Static mode with identical endpoints; the period is expressed in centiseconds.
    colors[..18].copy_from_slice(&[
        REPORT_ID, 0x02, 0x01, 100, 0, 0, 0, 0x0F, 1, 0, 0, r, g, b, 100, r, g, b,
    ]);
    [select, colors]
}

struct Preparsed(PHIDP_PREPARSED_DATA);
impl Drop for Preparsed {
    fn drop(&mut self) {
        let _ = unsafe { HidD_FreePreparsedData(self.0) };
    }
}

fn read_collection(path: &[u16], deadline: Instant) -> Result<Option<HidCollection>, Diagnostic> {
    if !lighting_path_matches(path)? {
        return Ok(None);
    }
    before_deadline(deadline, "CreateFileW(HID)")?;
    let handle = OwnedHandle::open_device(path, 0)
        .map_err(|e| Diagnostic::windows("CreateFileW(HID, access=0)", e))?;
    describe(&handle, deadline).map(Some)
}

fn lighting_path_matches(path: &[u16]) -> Result<bool, Diagnostic> {
    wide_str(path)
        .map(|name| matches_path(&name))
        .ok_or_else(|| Diagnostic::invalid("HID interface path", "invalid UTF-16"))
}

fn describe(handle: &OwnedHandle, deadline: Instant) -> Result<HidCollection, Diagnostic> {
    let mut attrs = HIDD_ATTRIBUTES {
        Size: size_of::<HIDD_ATTRIBUTES>() as u32,
        ..Default::default()
    };
    if !unsafe { HidD_GetAttributes(handle.0, &mut attrs) } {
        return Err(Diagnostic::last_win32("HidD_GetAttributes"));
    }
    if (attrs.VendorID, attrs.ProductID) != (VID, PID) {
        return Err(Diagnostic::invalid(
            "HidD_GetAttributes",
            "PnP VID/PID disagrees with HID attributes",
        ));
    }
    before_deadline(deadline, "HidD_GetPreparsedData")?;
    let mut raw = PHIDP_PREPARSED_DATA::default();
    if !unsafe { HidD_GetPreparsedData(handle.0, &mut raw) } {
        return Err(Diagnostic::last_win32("HidD_GetPreparsedData"));
    }
    if raw.0 == 0 {
        return Err(Diagnostic::invalid(
            "HidD_GetPreparsedData",
            "null preparsed data",
        ));
    }
    let data = Preparsed(raw);
    let mut caps = HIDP_CAPS::default();
    let status = unsafe { HidP_GetCaps(data.0, &mut caps) };
    if status != HIDP_STATUS_SUCCESS {
        return Err(Diagnostic::native(
            "HidP_GetCaps",
            DiagnosticKind::Io,
            NativeDomain::NtStatus,
            status.0 as u32,
        ));
    }
    Ok(HidCollection {
        vendor_id: attrs.VendorID,
        product_id: attrs.ProductID,
        revision: attrs.VersionNumber,
        usage_page: caps.UsagePage,
        usage: caps.Usage,
        input_report_bytes: caps.InputReportByteLength,
        output_report_bytes: caps.OutputReportByteLength,
        feature_report_bytes: caps.FeatureReportByteLength,
        supports_feature_02: caps.FeatureReportByteLength == REPORT_BYTES as u16
            && has_lighting_report(&data, &caps),
    })
}

fn matches_path(path: &str) -> bool {
    let mut parts = path.split(['#', '&', '\\']);
    parts.any(|part| part.eq_ignore_ascii_case("VID_1462"))
        && parts
            .next()
            .is_some_and(|part| part.eq_ignore_ascii_case("PID_1603"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hid_filter_and_unknown_conflict_are_fail_closed() {
        assert!(matches_path(r"\\?\hid#vid_1462&pid_1603&mi_00#redacted"));
        assert!(!matches_path(r"\\?\hid#vid_1462&pid_16030#redacted"));
        assert_eq!(rgb_conflict(&ConflictReport::default()), None);
        assert_eq!(
            rgb_conflict(&ConflictReport {
                process_enumeration_complete: true,
                service_query_complete: true,
                ..Default::default()
            }),
            Some(false)
        );
        assert_eq!(
            rgb_conflict(&ConflictReport {
                running_processes: vec!["LEDKeeper2.exe"],
                ..Default::default()
            }),
            Some(true)
        );
    }

    #[test]
    fn detected_oem_conflicts_require_explicit_waiver_and_complete_inventory() {
        let mut conflicts = ConflictReport {
            running_services: vec!["LEDKeeper2"],
            process_enumeration_complete: true,
            ..Default::default()
        };
        assert_eq!(
            check_conflicts(&conflicts, false).unwrap_err().kind,
            DiagnosticKind::ConflictDetected
        );
        assert_eq!(
            check_conflicts(&conflicts, true).unwrap_err().kind,
            DiagnosticKind::Unavailable
        );
        conflicts.service_query_complete = true;
        assert!(check_conflicts(&conflicts, true).is_ok());
    }

    #[test]
    fn reports_match_select_and_static_wire_bytes() {
        let color = Rgb8 {
            r: 0x12,
            g: 0x34,
            b: 0x56,
        };
        let [first, colors] = reports(1, color);
        let [last, _] = reports(1 << 23, color);
        assert_eq!(&first[..6], &[2, 1, 1, 0, 0, 0]);
        assert_eq!(&last[..6], &[2, 1, 0, 0, 0x80, 0]);
        assert_eq!(
            &colors[..18],
            &[
                2, 2, 1, 100, 0, 0, 0, 15, 1, 0, 0, 0x12, 0x34, 0x56, 100, 0x12, 0x34, 0x56
            ]
        );
        assert_eq!(first[6..], [0; 58]);
        assert_eq!(last[6..], [0; 58]);
        assert_eq!(colors[18..], [0; 46]);
    }

    #[test]
    fn frame_covers_each_zone_once_and_groups_equal_colors() {
        let black = Rgb8 { r: 0, g: 0, b: 0 };
        let green = Rgb8 { r: 0, g: 255, b: 0 };
        let mut zones = [black; KEYBOARD_ZONE_COUNT];
        zones[1] = green;
        zones[23] = green;
        let mut sent = Vec::new();
        send_frame(&zones, |report| {
            sent.push(*report);
            Ok::<_, ()>(())
        })
        .unwrap();
        assert_eq!(sent.len(), 4);
        let mut covered = 0;
        for pair in sent.chunks_exact(2) {
            let mask = u32::from_le_bytes(pair[0][2..6].try_into().unwrap());
            assert_eq!(mask & covered, 0);
            assert_eq!(mask >> 24, 0);
            covered |= mask;
            for (i, rgb) in zones.iter().enumerate() {
                if mask & (1 << i) != 0 {
                    assert_eq!(&pair[1][11..14], &[rgb.r, rgb.g, rgb.b]);
                }
            }
        }
        assert_eq!(covered, 0x00ff_ffff);
        assert_eq!(&sent[2][2..6], &[2, 0, 0x80, 0]);
    }

    #[test]
    fn all_24_distinct_colors_produce_ordered_pairs_and_abort_on_error() {
        let zones = std::array::from_fn(|i| Rgb8 {
            r: i as u8,
            g: 255 - i as u8,
            b: 42,
        });
        let mut sent = Vec::new();
        send_frame(&zones, |report| {
            sent.push(*report);
            Ok::<_, ()>(())
        })
        .unwrap();
        assert_eq!(sent.len(), 48);
        for (i, pair) in sent.chunks_exact(2).enumerate() {
            assert_eq!(&pair[0][..2], &[2, 1]);
            assert_eq!(&pair[0][2..6], &(1u32 << i).to_le_bytes());
            assert_eq!(&pair[1][..3], &[2, 2, 1]);
            assert_eq!(&pair[1][11..14], &[i as u8, 255 - i as u8, 42]);
        }
        let mut calls = 0;
        assert_eq!(
            send_frame(&zones, |_| {
                calls += 1;
                if calls == 2 {
                    Err("feature transfer failed")
                } else {
                    Ok(())
                }
            }),
            Err("feature transfer failed")
        );
        assert_eq!(calls, 2);
    }

    #[test]
    fn feature_02_value_cap_accepts_64_byte_report_and_rejects_mismatches() {
        // Real MS-1603 descriptor reports ID=0x02, BitSize=8, ReportCount=63 (1 + 63 = 64 bytes).
        assert!(feature_value_cap_matches(0x02, 8, 63));
        assert!(!feature_value_cap_matches(0x01, 8, 63));
        assert!(!feature_value_cap_matches(0x02, 8, 1));
        assert!(!feature_value_cap_matches(0x02, 16, 63));
    }
}
