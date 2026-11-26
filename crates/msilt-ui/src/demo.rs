//! Offline demo snapshot built from the real factory parameters of the
//! reference machine (MS-17S3 / `17S3EMS1.107` / AUO84A6 2560x1600 60-240 Hz /
//! HID `1462:1603` 24-zone keyboard).

use msilt_core::{
    AutomationRuleView, BatteryDetail, CapabilityFlags, DaemonStatus, EcWriteStatus, FanCurve,
    FanCurveLimits, FanPoint, FanState, GpuMuxMode, GpuMuxState, HardwareIdentity, HardwareState,
    OemInventory, OemItemKind, OemItemStatus, OemPackageId, OemPackageStatus, OemProcessStatus,
    OemScope, PerformanceMode, PowerSource, Rgb8, StartupMode, StartupStatus, SystemSnapshot,
    TelemetrySample, KEYBOARD_ZONE_COUNT,
};

pub const CPU_TEMPS: [u8; 6] = [0, 48, 56, 62, 70, 78];
pub const CPU_DUTIES: [u8; 6] = [0, 36, 42, 52, 76, 84];
pub const GPU_TEMPS: [u8; 6] = [0, 43, 48, 56, 65, 72];
pub const GPU_DUTIES: [u8; 6] = [0, 36, 46, 56, 80, 88];
pub const MAX_DUTY: u8 = 150;

pub const BOARD_ID: &str = "MS-17S3";
pub const EC_FIRMWARE: &str = "17S3EMS1.107";
pub const PANEL: &str = "AUO84A6 · 2560×1600 · 60/240 Hz";

fn curve(temps: [u8; 6], duties: [u8; 6]) -> FanCurve {
    FanCurve {
        points: std::array::from_fn(|i| FanPoint {
            temperature_c: temps[i],
            duty_percent: duties[i],
        }),
    }
}

fn fan(temps: [u8; 6], duties: [u8; 6]) -> FanState {
    let floor = curve(temps, duties);
    FanState {
        curve: floor,
        limits: Some(FanCurveLimits {
            floor,
            max_duty_percent: MAX_DUTY,
            fixed_anchor: true,
        }),
    }
}

fn zones() -> [Rgb8; KEYBOARD_ZONE_COUNT] {
    // Wootility v5 matte editorial keycap palette: slate-gray alpha, sage modifiers, brick right-zone, ochre Enter
    let slate = Rgb8 { r: 0x7A, g: 0x7D, b: 0x84 };
    let sage  = Rgb8 { r: 0x45, g: 0x85, b: 0x62 };
    let brick = Rgb8 { r: 0x8E, g: 0x35, b: 0x30 };
    let ochre = Rgb8 { r: 0x9C, g: 0x83, b: 0x38 };
    let mut out = [slate; KEYBOARD_ZONE_COUNT];
    for i in [6, 12, 18, 22] { out[i] = sage; }
    for i in [3, 9, 15, 21] { out[i] = brick; }
    for i in [16] { out[i] = ochre; }
    for i in [5, 11, 17, 23] { out[i] = sage; }
    out
}

fn all_controls() -> CapabilityFlags {
    CapabilityFlags::PERFORMANCE_MODE
        | CapabilityFlags::FAN_CURVE
        | CapabilityFlags::COOLER_BOOST
        | CapabilityFlags::CHARGE_LIMIT
        | CapabilityFlags::DISPLAY_REFRESH_RATE
        | CapabilityFlags::DISPLAY_BRIGHTNESS
        | CapabilityFlags::COLOR_PROFILE
        | CapabilityFlags::KEYBOARD_LIGHTING
        | CapabilityFlags::GPU_MUX
        | CapabilityFlags::TELEMETRY
        | CapabilityFlags::OSD
        | CapabilityFlags::AUTOMATION
}

pub fn snapshot() -> SystemSnapshot {
    SystemSnapshot {
        revision: 128,
        status: DaemonStatus::Ready,
        ec_write_status: EcWriteStatus::Enabled,
        capabilities: all_controls(),
        writable: all_controls(),
        hardware: HardwareState {
            identity: HardwareIdentity {
                board_id: Some(BOARD_ID.into()),
                ec_firmware: Some(EC_FIRMWARE.into()),
            },
            supported_performance_modes: vec![
                PerformanceMode::SuperBattery,
                PerformanceMode::Silent,
                PerformanceMode::Balanced,
                PerformanceMode::ExtremePerformance,
            ],
            performance_mode: Some(PerformanceMode::Balanced),
            cpu_fan: Some(fan(CPU_TEMPS, CPU_DUTIES)),
            gpu_fan: Some(fan(GPU_TEMPS, GPU_DUTIES)),
            cooler_boost: Some(false),
            power_source: Some(PowerSource::Ac),
            battery_percent: Some(87),
            charge_limit_percent: Some(80),
            supported_charge_limits: vec![60, 80, 100],
            display_refresh_hz: Some(240),
            supported_refresh_rates: vec![60, 240],
            display_brightness_percent: Some(72),
            color_profile_id: Some("TrueColor_DCIP3_6300".into()),
            supported_color_profiles: vec![
                "TrueColor_DCIP3_6300".into(),
                "TrueColor_sRGB_6500".into(),
                "TrueColor_DisplayP3".into(),
            ],
            keyboard_zones: Some(zones()),
            gpu_mux: Some(GpuMuxState { current: GpuMuxMode::Hybrid, pending: None }),
            battery_detail: Some(BatteryDetail {
                designed_capacity_mwh: Some(87_395),
                full_charged_capacity_mwh: Some(87_195),
                remaining_capacity_mwh: Some(75_860),
                health_tenths_percent: Some(998),
                cycle_count: Some(12),
                voltage_mv: Some(15_420),
                rate_mw: Some(24_200),
                charging: Some(true),
            }),
            panel_name: Some(PANEL.into()),
            keyboard_conflict: Some(false),
        },
        manual_overrides: Vec::new(),
    }
}

pub fn startup_status() -> StartupStatus {
    StartupStatus {
        enabled: true,
        mode: Some(StartupMode::ScheduledTask),
        elevated: true,
        installed_in_program_files: true,
        detail: "已通过计划任务 \\MSI-Toolkit\\Daemon 注册（登录后免 UAC 静默启动）".into(),
    }
}

pub fn oem_inventory() -> OemInventory {
    OemInventory {
        elevated: true,
        has_backup: true,
        protected_intact: true,
        items: vec![
            OemItemStatus {
                id: "MSI_Center_Service".into(),
                display_name: "MSI Center 核心服务".into(),
                kind: OemItemKind::Service,
                tier: OemScope::Core,
                present: true,
                running: true,
                auto_start: true,
                startup_label: "自动".into(),
            },
            OemItemStatus {
                id: "MSI Foundation Service".into(),
                display_name: "MSI 基础服务".into(),
                kind: OemItemKind::Service,
                tier: OemScope::Core,
                present: true,
                running: true,
                auto_start: true,
                startup_label: "自动".into(),
            },
            OemItemStatus {
                id: "MSI Sendevsvc".into(),
                display_name: "MSI 传感器服务".into(),
                kind: OemItemKind::Service,
                tier: OemScope::Core,
                present: true,
                running: true,
                auto_start: true,
                startup_label: "自动".into(),
            },
            OemItemStatus {
                id: "Mystic_Light_Service".into(),
                display_name: "Mystic Light 灯效服务".into(),
                kind: OemItemKind::Service,
                tier: OemScope::Lighting,
                present: true,
                running: true,
                auto_start: true,
                startup_label: "自动".into(),
            },
            OemItemStatus {
                id: "MSITrueColorService".into(),
                display_name: "MSI True Color 校色服务".into(),
                kind: OemItemKind::Service,
                tier: OemScope::TrueColor,
                present: true,
                running: true,
                auto_start: true,
                startup_label: "自动".into(),
            },
        ],
        processes: vec![
            OemProcessStatus { name: "DCv2".into(), pid: 7048, memory_kib: 142_000, tier: OemScope::Core },
            OemProcessStatus { name: "MSI.CentralServer".into(), pid: 33392, memory_kib: 68_000, tier: OemScope::Core },
            OemProcessStatus { name: "LEDKeeper2".into(), pid: 35792, memory_kib: 44_000, tier: OemScope::Lighting },
            OemProcessStatus { name: "MsiTrueColor".into(), pid: 11928, memory_kib: 38_000, tier: OemScope::TrueColor },
        ],
        packages: vec![
            OemPackageStatus {
                id: OemPackageId::MsiCenterUwp,
                display_name: "MSI Center (Store)".into(),
                version: "2.0.75.0".into(),
                installed: true,
                can_uninstall: true,
                note: "UWP · 734 MiB".into(),
            },
            OemPackageStatus {
                id: OemPackageId::MsiCenterSdk,
                display_name: "MSI Center SDK".into(),
                version: "3.2026.1013.01".into(),
                installed: true,
                can_uninstall: true,
                note: "服务包 · 1.1 GiB".into(),
            },
            OemPackageStatus {
                id: OemPackageId::MsiTrueColor,
                display_name: "MSI True Color".into(),
                version: "5.1.26.0".into(),
                installed: true,
                can_uninstall: true,
                note: "校色组件（保留出厂 ICM）".into(),
            },
        ],
        summary: "演示快照：检测到 5 项运行中 MSI 服务与 3 个已安装官软包".into(),
    }
}

pub fn automation_rules() -> Vec<AutomationRuleView> {
    vec![
        AutomationRuleView {
            id: "battery-silent-60hz".into(),
            name: "拔电切静音与 60Hz".into(),
            enabled: true,
            priority: 500,
            trigger: "电源切换 · 使用电池".into(),
            actions: "静音模式 + 60 Hz 刷新率".into(),
            diagnostic: "未命中（当前 AC）".into(),
            active: false,
        },
        AutomationRuleView {
            id: "ac-balanced-240hz".into(),
            name: "插电恢复平衡与 240Hz".into(),
            enabled: true,
            priority: 500,
            trigger: "电源切换 · 交流电供电".into(),
            actions: "平衡模式 + 240 Hz 刷新率".into(),
            diagnostic: "已命中".into(),
            active: true,
        },
    ]
}

fn tri(tick: u64, period: u64, amplitude: i32) -> i32 {
    let period = period.max(2);
    let half = (period / 2) as i32;
    let position = (tick % period) as i32;
    let folded = if position < half { position } else { period as i32 - position };
    (folded * amplitude * 2 / half) - amplitude
}

pub fn telemetry(tick: u64, now_unix_ms: u64) -> TelemetrySample {
    let cpu = (62 + tri(tick, 47, 6)).clamp(0, 100) as i16;
    let gpu = (55 + tri(tick, 61, 8)).clamp(0, 100) as i16;
    let cpu_duty = (58 + tri(tick, 47, 10)).clamp(0, 150) as u8;
    let gpu_duty = (62 + tri(tick, 61, 12)).clamp(0, 150) as u8;
    TelemetrySample {
        sampled_at_unix_ms: now_unix_ms,
        cpu_temperature_c: Some(cpu),
        gpu_temperature_c: Some(gpu),
        cpu_fan_rpm: Some((2380 + tri(tick, 53, 260)).clamp(0, 6500) as u16),
        gpu_fan_rpm: Some((2520 + tri(tick, 67, 300)).clamp(0, 6500) as u16),
        cpu_reported_duty_percent: Some(cpu_duty),
        gpu_reported_duty_percent: Some(gpu_duty),
        gpu_power_mw: Some((82_000 + tri(tick, 71, 18_000)).clamp(0, 175_000) as u32),
    }
}
