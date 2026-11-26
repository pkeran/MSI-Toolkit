use std::fmt::{Display, Write};

use msilt_hal::ProbeReport;
use msilt_hal::battery::CapacityUnit;

pub(crate) fn render(report: &ProbeReport) -> String {
    let mut table = Table(String::from(
        "MSI Toolkit | Phase 0 read-only diagnostics\n",
    ));
    table.section("Hardware and security");
    let id = &report.identity;
    table.row("Model", text(id.model.as_deref()));
    table.row(
        "Board ID / revision",
        format_args!(
            "{} / {}",
            text(id.board_id.as_deref()),
            text(id.board_revision.as_deref())
        ),
    );
    table.row("BIOS", text(id.bios_version.as_deref()));
    table.row("Elevated token", yes_no(id.elevated));
    table.row("Secure Boot", yes_no(id.secure_boot_enabled));
    table.row("HVCI running", yes_no(id.hvci_running));
    table.row(
        "Vulnerable Driver Blocklist policy",
        yes_no(id.vulnerable_driver_blocklist_policy_enabled),
    );
    table.row(
        "EC map envelope / guard",
        if report.ec_map.validated {
            "valid / read-only"
        } else {
            "invalid"
        },
    );
    table.row(
        "Board/revision/BIOS match",
        yes_no(report.ec_map.board_and_bios_match),
    );
    if let Some(expected) = &report.ec_map.identity {
        table.row(
            "Map baseline EC / WMI",
            format_args!(
                "{} / {} (expected)",
                expected.ec_firmware, expected.wmi_version
            ),
        );
    }

    table.section("Internal display and color");
    if let Some(panel) = &report.display.panel {
        table.row(
            "EDID / resolution",
            format_args!(
                "{} / {} x {}",
                text(panel.edid_id.as_deref()),
                avail(panel.width),
                avail(panel.height)
            ),
        );
        table.row(
            "CCD adapter vendor / output type",
            format_args!(
                "{} / {}",
                panel
                    .adapter_vendor_id
                    .map(|id| format!("0x{id:04X}"))
                    .unwrap_or_else(|| "unknown".into()),
                panel.ccd_output_technology
            ),
        );
        for (name, rate) in [
            ("Current CCD refresh", panel.refresh),
            ("Target signal refresh", panel.signal_refresh),
        ] {
            table.row(
                name,
                rate.map(|r| {
                    format!(
                        "{:.3} Hz ({}/{})",
                        f64::from(r.numerator) / f64::from(r.denominator),
                        r.numerator,
                        r.denominator
                    )
                })
                .unwrap_or_else(|| "unknown".into()),
            );
        }
        table.row(
            "Supported refresh (Hz)",
            if panel.shared_source {
                "unknown (cloned source)".into()
            } else {
                values(panel.supported_refresh_hz.iter())
            },
        );
    } else {
        table.row("Panel", "unavailable");
    }
    table.row(
        "Brightness",
        report
            .display
            .brightness
            .as_ref()
            .map(|b| format!("{}% ({} levels)", b.current_percent, b.levels.len()))
            .unwrap_or_else(|| "unknown".into()),
    );
    let color = &report.display.color;
    table.row("Windows color scope", text(color.association_scope));
    table.row(
        "Windows default ICC ID",
        text(color.default_profile_id.as_deref()),
    );
    table.row(
        "Associated ICC IDs",
        color
            .associated_profile_ids
            .as_ref()
            .map(|ids| values(ids.iter()))
            .unwrap_or_else(|| "unknown".into()),
    );
    table.row(
        "Installed ICC profiles",
        avail(color.installed_profile_ids.as_ref().map(Vec::len)),
    );
    table.row(
        "Factory ICM library",
        match color.factory_library_present {
            Some(true) => format!(
                "{} display profiles ({} with MHC2)",
                color.factory_profiles.len(),
                color.factory_profiles.iter().filter(|p| p.has_mhc2).count()
            ),
            Some(false) => "not present".into(),
            None => "unknown".into(),
        },
    );
    if !color.factory_profiles.is_empty() {
        table.row(
            "Factory ICM IDs",
            values(color.factory_profiles.iter().map(|p| &p.id)),
        );
    }

    table.section("Battery");
    let battery = &report.battery;
    table.row(
        "System power / battery level",
        format_args!(
            "{} / {}",
            battery
                .power_source
                .map(|p| format!("{p:?}"))
                .unwrap_or_else(|| "unknown".into()),
            unit(battery.system_percent, "%")
        ),
    );
    table.row(
        "System battery packs",
        format_args!(
            "{}{}",
            battery.packs.len(),
            complete(battery.enumeration_complete)
        ),
    );
    for (index, pack) in battery.packs.iter().enumerate() {
        let capacity_unit = if pack.capacity_unit == CapacityUnit::MilliWattHours {
            "mWh"
        } else {
            "relative"
        };
        table.row(
            &format!("Pack {} design / full / remaining", index + 1),
            format_args!(
                "{} / {} / {} {capacity_unit}",
                avail(pack.designed_capacity),
                avail(pack.full_charged_capacity),
                avail(pack.remaining_capacity)
            ),
        );
        table.row(
            "Health / state of charge / cycles",
            format_args!(
                "{} / {} / {}",
                real(pack.health_percent, "%"),
                real(pack.percent, "%"),
                pack.cycle_count
                    .map(|n| n.to_string())
                    .unwrap_or_else(|| format!("unknown (driver {})", pack.cycle_count_raw))
            ),
        );
        table.row(
            "Voltage / current / power",
            format_args!(
                "{} / {} / {}",
                unit(pack.voltage_mv, " mV"),
                real(pack.current_ma, " mA"),
                real(pack.power_w, " W")
            ),
        );
        if pack.capacity_unit == CapacityUnit::Relative {
            table.row("Relative charge/discharge rate", avail(pack.relative_rate));
        }
        table.row(
            "Pack source / charging / discharging",
            format_args!(
                "{} / {} / {}",
                pack.power_source
                    .map(|p| format!("{p:?}"))
                    .unwrap_or_else(|| "unknown".into()),
                yes_no(pack.charging),
                yes_no(pack.discharging)
            ),
        );
    }

    table.section("HID and MSI conflicts");
    table.row(
        "HID 1462:1603 collections",
        format_args!(
            "{}{}",
            report.hid.collections.len(),
            complete(report.hid.enumeration_complete)
        ),
    );
    for (index, hid) in report.hid.collections.iter().enumerate() {
        table.row(&format!("Collection {}", index + 1), format_args!("rev {:04X}; UsagePage 0x{:04X}, Usage 0x{:04X}; input/output/feature {}/{}/{} bytes",
            hid.revision, hid.usage_page, hid.usage, hid.input_report_bytes, hid.output_report_bytes, hid.feature_report_bytes));
    }
    let conflicts = &report.conflicts;
    table.row("RGB OEM conflict", yes_no(report.hid.oem_conflict));
    table.row(
        "Running MSI processes",
        format_args!(
            "{}{}",
            values(conflicts.running_processes.iter()),
            complete(conflicts.process_enumeration_complete)
        ),
    );
    table.row(
        "Running MSI services",
        format_args!(
            "{}{}",
            values(conflicts.running_services.iter()),
            complete(conflicts.service_query_complete)
        ),
    );

    table.section("Optional readers");
    if let Some(acpi) = &report.acpi {
        table.row(
            "MSI_ACPI identity match",
            format_args!(
                "{}; active instances {}",
                yes_no(Some(acpi.identity_matched)),
                avail(acpi.active_instances)
            ),
        );
        table.row(
            "Observed EC and WMI",
            format_args!(
                "{} / {}",
                text(acpi.ec_firmware.as_deref()),
                text(acpi.wmi_version.as_deref())
            ),
        );
        let t = &acpi.telemetry;
        table.row(
            "CPU / GPU temperature",
            format_args!(
                "{} / {}",
                unit(t.cpu_temperature_c, " C"),
                unit(t.gpu_temperature_c, " C")
            ),
        );
        table.row(
            "CPU / GPU fan speed",
            format_args!(
                "{} / {}",
                unit(t.cpu_fan_rpm, " RPM"),
                unit(t.gpu_fan_rpm, " RPM")
            ),
        );
        table.row(
            "CPU / GPU reported duty",
            format_args!(
                "{} / {}",
                unit(t.cpu_reported_duty_percent, "%"),
                unit(t.gpu_reported_duty_percent, "%")
            ),
        );
        table.row("Cooler Boost candidate bit", yes_no(acpi.cooler_boost_bit));
        table.row(
            "MUX support bit / EC D1",
            format_args!(
                "{} / {}",
                yes_no(acpi.mux_support_indicator),
                acpi.mux_ec_indicator
                    .map(|v| format!("0x{v:02X}"))
                    .unwrap_or_else(|| "unknown".into())
            ),
        );
        for method in &acpi.methods {
            let selector = method
                .selector
                .map(|n| format!("(0x{n:02X})"))
                .unwrap_or_default();
            let value = match &method.error {
                Some(error) => error.to_string(),
                None => format!(
                    "{} bytes, ReturnValue {}, prefix {}",
                    avail(method.package_bytes),
                    yes_no(method.wmi_return_value),
                    method
                        .prefix
                        .as_ref()
                        .map(|b| b
                            .iter()
                            .map(|n| format!("{n:02X}"))
                            .collect::<Vec<_>>()
                            .join(" "))
                        .unwrap_or_default()
                ),
            };
            table.row(
                &format!("{}{}", method.method, selector),
                format_args!("{value} [{} us]", method.elapsed_us),
            );
        }
        for issue in &acpi.diagnostics {
            table.row("MSI_ACPI", issue);
        }
    } else {
        table.row(
            "MSI_ACPI",
            "disabled (use --acpi as an elevated administrator)",
        );
    }
    if let Some(nvml) = &report.nvml {
        table.row("NVML driver", text(nvml.driver_version.as_deref()));
        for gpu in &nvml.devices {
            table.row(
                &format!("GPU {}", gpu.index),
                format_args!(
                    "{}; {}, {}, utilization {}",
                    text(gpu.name.as_deref()),
                    unit(gpu.temperature_c, " C"),
                    unit(gpu.power_mw, " mW"),
                    unit(gpu.gpu_utilization_percent, "%")
                ),
            );
        }
        for issue in &nvml.diagnostics {
            table.row("NVML", issue);
        }
    } else {
        table.row("NVML", "disabled");
    }
    if !report.diagnostics.is_empty() {
        table.section("Read failures");
        for issue in &report.diagnostics {
            table.row("Diagnostic", issue);
        }
    }
    table.0
}

struct Table(String);
impl Table {
    fn section(&mut self, title: &str) {
        let _ = writeln!(self.0, "\n{title}");
    }
    fn row(&mut self, label: &str, value: impl Display) {
        let _ = writeln!(self.0, "  {label:<37} | {value}");
    }
}

fn text(value: Option<&str>) -> &str {
    value.unwrap_or("unknown")
}
fn yes_no(value: Option<bool>) -> &'static str {
    match value {
        Some(true) => "yes",
        Some(false) => "no",
        None => "unknown",
    }
}
fn avail(value: Option<impl Display>) -> String {
    unit(value, "")
}
fn unit(value: Option<impl Display>, unit: &str) -> String {
    value
        .map(|v| format!("{v}{unit}"))
        .unwrap_or_else(|| "unknown".into())
}
fn real(value: Option<f64>, unit: &str) -> String {
    value
        .map(|v| format!("{v:.2}{unit}"))
        .unwrap_or_else(|| "unknown".into())
}
fn values(items: impl Iterator<Item = impl Display>) -> String {
    let names: Vec<_> = items.map(|value| value.to_string()).collect();
    if names.is_empty() {
        "none".into()
    } else {
        names.join(", ")
    }
}
fn complete(complete: bool) -> &'static str {
    if complete { "" } else { " (incomplete)" }
}
