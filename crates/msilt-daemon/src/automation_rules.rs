use std::{
    fmt::{self, Write},
    io,
};

use msilt_automation::{
    AutomationRule, AutomationRuleSet, BlockReason, Condition, DayMask, DryRunInput, EvalMode,
    ForegroundExit, HhMm, RuleEngine, RuleLimits, Trigger, lease::TargetSlot, world::EvalCtx,
};
use msilt_core::{
    AutomationEvent, AutomationPreset, AutomationRuleView, ControlChange, EffectivePowerMode,
    ErrorCode, FanChannel, IpcReply, IpcResponse, IpcResult, LeaseOwner, OsdKind, SystemSnapshot,
};
use msilt_hal::daemon_wire::{Outgoing, encode_reply};

pub(crate) const LOCK_SCREEN_ID: &str = "lock-screen-dark-keyboard";

pub(crate) struct RuleViewContext<'a> {
    pub(crate) snapshot: &'a SystemSnapshot,
    pub(crate) enabled: bool,
    pub(crate) dry_run: bool,
    pub(crate) monotonic_ms: u64,
    pub(crate) unix_ms: u64,
    pub(crate) unlocked: bool,
    pub(crate) saver: Option<bool>,
    pub(crate) effective: Option<EffectivePowerMode>,
    pub(crate) external_count: Option<u16>,
}

pub(crate) fn view_reply(
    engine: &RuleEngine,
    ctx: RuleViewContext<'_>,
) -> Result<IpcReply, ErrorCode> {
    let mut world = engine.world().clone();
    world.sync_from_snapshot(ctx.snapshot);
    world.session_unlocked = Some(ctx.unlocked);
    world.power_saving = ctx.saver;
    world.effective_power_mode = ctx.effective;
    world.external_displays = ctx.external_count;
    let events = [
        Some(AutomationEvent::SessionChanged {
            unlocked: ctx.unlocked,
        }),
        ctx.saver
            .map(|enabled| AutomationEvent::PowerSavingChanged { enabled }),
        Some(AutomationEvent::EffectivePowerModeChanged {
            mode: ctx.effective,
        }),
        ctx.external_count.map(
            |external_display_count| AutomationEvent::DisplayTopologyChanged {
                external_display_count,
            },
        ),
    ];
    let dry = engine.dry_run(DryRunInput {
        snapshot: ctx.snapshot,
        event: None,
        monotonic_ms: ctx.monotonic_ms,
        unix_ms: ctx.unix_ms,
        rule_id: None,
    });
    let mut matched_count = 0;
    let mut ready_count = 0;
    let mut rows = Vec::new();
    for (rule, dry) in engine.rules().iter().zip(dry.rules) {
        let app_matches = rule
            .trigger
            .app_gate()
            .is_none_or(|(apps, mode)| world.app_gate_matches(apps, mode));
        let idle_matches = match rule.trigger {
            Trigger::Idle { threshold_ms } => world.idle_ms.is_some_and(|ms| ms >= threshold_ms),
            _ => true,
        };
        let matched = app_matches
            && idle_matches
            && !matches!(dry.blocked, Some(BlockReason::NotTriggered))
            && rule.condition.eval(EvalCtx {
                world: &world,
                mode: EvalMode::Arm,
                hysteresis_percent: rule.hysteresis_percent,
                utc_offset_minutes: engine.limits().utc_offset_minutes,
                unix_ms: ctx.unix_ms,
            });
        let mut blocked = if !rule.enabled {
            Some(BlockReason::Disabled)
        } else if !matched && !matches!(dry.blocked, Some(BlockReason::NotTriggered)) {
            Some(BlockReason::ConditionFalse)
        } else if !matches!(dry.blocked, Some(BlockReason::ConditionFalse)) {
            dry.blocked
        } else {
            None
        };
        // DryRunInput accepts one OS event. Recover runtime diagnostics for matching
        // rules that depend on a session, Windows power mode or topology fact.
        if matched && matches!(dry.blocked, Some(BlockReason::ConditionFalse)) {
            for event in events.iter().flatten() {
                let probe = engine.dry_run(DryRunInput {
                    snapshot: ctx.snapshot,
                    event: Some(event),
                    monotonic_ms: ctx.monotonic_ms,
                    unix_ms: ctx.unix_ms,
                    rule_id: Some(&rule.id),
                });
                if let Some(probe) = probe.rules.first().filter(|row| row.matched) {
                    blocked = probe.blocked;
                    break;
                }
            }
        }
        let lock_keyboard = is_lock_screen_preset(rule)
            && !ctx.unlocked
            && ctx
                .snapshot
                .capabilities
                .contains(msilt_core::CapabilityFlags::KEYBOARD_LIGHTING);
        if lock_keyboard && blocked == Some(BlockReason::CapabilityMissing) {
            blocked = None;
        }
        if matched && blocked.is_none() {
            blocked = rule.actions.iter().find_map(|action| {
                if ctx.snapshot.manual_overrides.contains(&action.target()) {
                    Some(BlockReason::ManualOverride)
                } else if !lock_keyboard && !ctx.snapshot.writable.contains(action.capability()) {
                    Some(BlockReason::CapabilityMissing)
                } else if action.validate_against(&ctx.snapshot.hardware).is_err() {
                    Some(BlockReason::UnsupportedValue)
                } else {
                    None
                }
            });
        }
        let eligible = matched && blocked.is_none() && ctx.enabled;
        let leased = ctx.enabled
            && rule
                .actions
                .iter()
                .filter_map(TargetSlot::of_change)
                .any(|slot| {
                    engine
                        .leases()
                        .find(slot, LeaseOwner::Rule(rule.owner))
                        .is_some()
                });
        matched_count += usize::from(matched && rule.enabled);
        ready_count += usize::from(eligible);
        let diagnostic = if let Some(blocked) = blocked {
            block_label(blocked)
        } else if !ctx.enabled {
            "已命中；自动化总开关关闭"
        } else if leased {
            "已命中；规则持有租约"
        } else {
            "已命中；动作可执行"
        };
        rows.push(AutomationRuleView {
            id: rule.id.clone(),
            name: rule.name.clone(),
            enabled: rule.enabled,
            priority: rule.priority,
            trigger: rule.trigger.kind_str().into(),
            actions: rule
                .actions
                .iter()
                .map(action_label)
                .collect::<Vec<_>>()
                .join("；"),
            diagnostic: diagnostic.into(),
            active: if ctx.dry_run { eligible } else { leased },
        });
    }
    let total = rows.len();
    let summary = if ctx.dry_run {
        format!("模拟：{matched_count}/{total} 条命中，{ready_count} 条可执行")
    } else {
        format!(
            "共 {total} 条规则，{} 条启用",
            rows.iter().filter(|r| r.enabled).count()
        )
    };
    bounded_reply(rows, summary)
}

fn bounded_reply(
    mut rules: Vec<AutomationRuleView>,
    summary: String,
) -> Result<IpcReply, ErrorCode> {
    let total = rules.len();
    let mut frame = Vec::new();
    loop {
        let shown = rules.len();
        let summary = if shown == total {
            summary.clone()
        } else {
            format!("{summary}；当前响应显示前 {shown} 条")
        };
        let message = IpcResponse {
            id: u64::MAX,
            result: IpcResult::Ok {
                reply: IpcReply::AutomationRules { rules, summary },
            },
        };
        let fits = encode_reply(Outgoing::Response(&message), &mut frame).is_ok();
        let IpcResult::Ok {
            reply:
                IpcReply::AutomationRules {
                    rules: found,
                    summary: rendered,
                },
        } = message.result
        else {
            unreachable!()
        };
        if fits {
            return Ok(IpcReply::AutomationRules {
                rules: found,
                summary: rendered,
            });
        }
        rules = found;
        if rules.is_empty() {
            return Err(ErrorCode::Internal);
        }
        rules.truncate(shown.saturating_sub((shown / 8).max(1)));
    }
}

fn block_label(reason: BlockReason) -> &'static str {
    match reason {
        BlockReason::Disabled => "规则已禁用",
        BlockReason::Unsupported => "当前硬件不支持",
        BlockReason::NotTriggered => "尚未收到触发事件",
        BlockReason::ManualOverride => "命中；手动覆盖中",
        BlockReason::CapabilityMissing => "命中；硬件能力不可写",
        BlockReason::UnsupportedValue => "命中；硬件不支持该值",
        BlockReason::RefusedByDaemon => "命中；后台拒绝动作",
        BlockReason::SafetyHalt => "命中；写入已保护锁定",
        BlockReason::Cooldown { .. } => "命中；等待冷却",
        BlockReason::Frozen { .. } => "命中；频繁切换已抑制",
        BlockReason::GovernorDeferred { .. } | BlockReason::GovernorDenied => {
            "命中；受写入频率限制"
        }
        BlockReason::LeasePressure => "命中；租约容量不足",
        BlockReason::ActionBudgetExhausted => "命中；动作额度耗尽",
        BlockReason::ConditionFalse => "当前条件不匹配",
        BlockReason::Debouncing { .. } => "命中；等待条件稳定",
    }
}

fn action_label(change: &ControlChange) -> String {
    match change {
        ControlChange::SetPerformanceMode { mode } => match mode {
            msilt_core::PerformanceMode::SuperBattery => "超级省电",
            msilt_core::PerformanceMode::Silent => "静音",
            msilt_core::PerformanceMode::Balanced => "平衡",
            msilt_core::PerformanceMode::ExtremePerformance => "极致性能",
        }
        .into(),
        ControlChange::SetCoolerBoost { enabled } => if *enabled {
            "开启强冷"
        } else {
            "关闭强冷"
        }
        .into(),
        ControlChange::SetChargeLimit { percent } => format!("充电上限 {percent}%"),
        ControlChange::SetDisplayRefreshRate { hz } => format!("{hz} Hz"),
        ControlChange::SetDisplayBrightness { percent } => format!("亮度 {percent}%"),
        ControlChange::SetColorProfile { profile_id } => format!("色彩 {profile_id}"),
        ControlChange::SetFanCurve { channel, .. } => format!("{channel:?} 风扇曲线"),
        change @ ControlChange::SetKeyboardLighting { .. } if dark_keyboard(change) => {
            "关闭键盘灯".into()
        }
        ControlChange::SetKeyboardLighting { .. } => "24 区键盘灯".into(),
        _ => "未知动作".into(),
    }
}

pub(crate) fn is_lock_screen_preset(rule: &AutomationRule) -> bool {
    rule.id == LOCK_SCREEN_ID
        && rule.enabled
        && matches!(rule.trigger, Trigger::Session)
        && matches!(rule.condition, Condition::SessionUnlocked(false))
        && matches!(rule.actions.as_slice(), [change] if dark_keyboard(change))
}

pub(crate) fn dark_keyboard(change: &ControlChange) -> bool {
    matches!(change, ControlChange::SetKeyboardLighting { zones } if zones.iter().all(|rgb| rgb.r == 0 && rgb.g == 0 && rgb.b == 0))
}

pub(crate) fn preset(preset: AutomationPreset) -> (&'static str, String) {
    let (id, name, priority, schedule, condition, actions) = match preset {
        AutomationPreset::BatterySilent60Hz => (
            "battery-silent-60hz", "电池供电：静音与 60 Hz", 500, "power", "{ac_connected = false}",
            r#"{field = "performanceMode", value = "silent"}, {field = "displayRefreshRate", value = 60}"#.into(),
        ),
        AutomationPreset::AcBalanced240Hz => (
            "ac-balanced-240hz", "接通电源：平衡与 240 Hz", 500, "power", "{ac_connected = true}",
            r#"{field = "performanceMode", value = "balanced"}, {field = "displayRefreshRate", value = 240}"#.into(),
        ),
        AutomationPreset::LowBatterySuperSave => (
            "low-battery-super-save", "低电量：超级省电", 650, "battery",
            r#"{all = [{ac_connected = false}, {battery_percent = {op = "le", value = 20}}]}"#,
            r#"{field = "performanceMode", value = "superBattery"}, {field = "displayRefreshRate", value = 60}"#.into(),
        ),
        AutomationPreset::LockScreenDarkKeyboard => (
            LOCK_SCREEN_ID, "锁屏：关闭键盘灯", 550, "session", "{session_unlocked = false}",
            format!(r#"{{field = "keyboardLighting", zones = [{}]}}"#, ["{r = 0, g = 0, b = 0}"; msilt_core::KEYBOARD_ZONE_COUNT].join(", ")),
        ),
    };
    let timing = if preset == AutomationPreset::LockScreenDarkKeyboard {
        "\ndebounce_on_ms = 0\ndebounce_off_ms = 0"
    } else {
        ""
    };
    (
        id,
        format!(
            "\n[[rules]]\nid = {id:?}\nname = {name:?}\npriority = {priority}{timing}\ntrigger = {{on = {schedule:?}}}\ncondition = {condition}\nactions = [{actions}]\n"
        ),
    )
}

pub(crate) fn encode(set: &AutomationRuleSet, appended_rule: Option<&str>) -> io::Result<String> {
    let mut text = if set.rules.is_empty() && appended_rule.is_none() {
        String::from("schema = 1\nrules = []\n")
    } else {
        String::from("schema = 1\n")
    };
    write_limits(&mut text, set.limits).expect("write to String");
    for rule in &set.rules {
        write_rule(&mut text, rule, set.limits).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidData, "unsupported rule while saving")
        })?;
    }
    if let Some(rule) = appended_rule {
        text.push_str(rule);
    }
    Ok(text)
}

fn write_limits(out: &mut String, limits: RuleLimits) -> fmt::Result {
    if limits == RuleLimits::default() {
        return Ok(());
    }
    writeln!(out, "\n[limits]")?;
    macro_rules! fields {
        ($($field:ident),+) => { $(writeln!(out, "{} = {}", stringify!($field), limits.$field)?;)+ };
    }
    fields!(
        max_rules,
        max_condition_depth,
        min_timer_period_ms,
        ec_writes_per_10s,
        ec_min_gap_ms,
        battery_hysteresis_percent,
        utc_offset_minutes,
        max_actions_per_event,
        lease_depth,
        flap_window_ms,
        flap_threshold,
        flap_freeze_ms
    );
    Ok(())
}

fn write_rule(out: &mut String, rule: &AutomationRule, limits: RuleLimits) -> fmt::Result {
    writeln!(out, "\n[[rules]]\nid = {}", quoted(&rule.id))?;
    if rule.name != rule.id {
        writeln!(out, "name = {}", quoted(&rule.name))?;
    }
    if !rule.enabled {
        writeln!(out, "enabled = false")?;
    }
    if rule.priority != msilt_automation::rule::DEFAULT_RULE_PRIORITY {
        writeln!(out, "priority = {}", rule.priority)?;
    }
    if rule.debounce_on_ms != 800 {
        writeln!(out, "debounce_on_ms = {}", rule.debounce_on_ms)?;
    }
    if rule.debounce_off_ms != 1500 {
        writeln!(out, "debounce_off_ms = {}", rule.debounce_off_ms)?;
    }
    if rule.cooldown_ms != 3000 {
        writeln!(out, "cooldown_ms = {}", rule.cooldown_ms)?;
    }
    if rule.hysteresis_percent != limits.battery_hysteresis_percent {
        writeln!(out, "hysteresis_percent = {}", rule.hysteresis_percent)?;
    }
    if let Some(ttl) = rule.ttl_ms {
        writeln!(out, "ttl_ms = {ttl}")?;
    }
    if !rule.notify.is_empty() {
        write!(out, "notify = ")?;
        array(
            out,
            rule.notify.iter().map(|kind| match kind {
                OsdKind::PerformanceMode => "performanceMode",
                OsdKind::CoolerBoost => "coolerBoost",
                OsdKind::DisplayRefreshRate => "displayRefreshRate",
                OsdKind::AutomationApplied => "automationApplied",
            }),
            string,
        )?;
        writeln!(out)?;
    }
    write!(out, "trigger = ")?;
    trigger(out, &rule.trigger)?;
    write!(out, "\ncondition = ")?;
    condition(out, &rule.condition)?;
    write!(out, "\nactions = ")?;
    array(out, rule.actions.iter(), action)?;
    writeln!(out)
}

fn trigger(out: &mut String, trigger: &Trigger) -> fmt::Result {
    write!(out, "{{on = {:?}", trigger.kind_str())?;
    match trigger {
        Trigger::Foreground { apps, exit } => {
            write!(out, ", apps = ")?;
            array(out, apps.iter().map(String::as_str), string)?;
            write!(
                out,
                ", exit = {:?}",
                match exit {
                    ForegroundExit::ForegroundLost => "foreground_lost",
                    ForegroundExit::ProcessExited => "process_exit",
                    ForegroundExit::Both => "both",
                }
            )?;
        }
        Trigger::Process { apps } => {
            write!(out, ", apps = ")?;
            array(out, apps.iter().map(String::as_str), string)?;
        }
        Trigger::Idle { threshold_ms } => write!(out, ", threshold_ms = {threshold_ms}")?,
        Trigger::Interval { every_ms } => write!(out, ", every_ms = {every_ms}")?,
        Trigger::Daily { at, days } => {
            write!(out, ", at = {}, days = ", time(*at))?;
            weekdays(out, *days)?;
        }
        Trigger::Hotkey { id } => write!(out, ", id = {}", quoted(id))?,
        Trigger::Power
        | Trigger::Battery
        | Trigger::PowerSaving
        | Trigger::EffectivePower
        | Trigger::Display
        | Trigger::Session
        | Trigger::State => {}
    }
    write!(out, "}}")
}

fn condition(out: &mut String, test: &Condition) -> fmt::Result {
    use Condition::*;
    write!(out, "{{")?;
    match test {
        Always => write!(out, "always = true")?,
        AcConnected(value) => write!(out, "ac_connected = {value}")?,
        BatteryPercent { op, value } => write!(
            out,
            "battery_percent = {{op = {:?}, value = {value}}}",
            op.as_str()
        )?,
        PowerSaving(value) => write!(out, "power_saving = {value}")?,
        EffectivePowerModeIn(modes) => {
            write!(out, "effective_power_mode_in = ")?;
            array(
                out,
                modes.iter().map(|mode| match mode {
                    EffectivePowerMode::BatterySaver => "batterySaver",
                    EffectivePowerMode::BetterBattery => "betterBattery",
                    EffectivePowerMode::Balanced => "balanced",
                    EffectivePowerMode::HighPerformance => "highPerformance",
                    EffectivePowerMode::MaxPerformance => "maxPerformance",
                    EffectivePowerMode::GameMode => "gameMode",
                    EffectivePowerMode::MixedReality => "mixedReality",
                    _ => "unsupportedMode",
                }),
                string,
            )?;
        }
        AppForeground(apps) | AppRunning(apps) => {
            write!(
                out,
                "{} = ",
                if matches!(test, AppForeground(_)) {
                    "app_foreground"
                } else {
                    "app_running"
                }
            )?;
            array(out, apps.iter().map(String::as_str), string)?;
        }
        ExternalDisplays { op, count } => write!(
            out,
            "external_displays = {{op = {:?}, count = {count}}}",
            op.as_str()
        )?,
        SessionUnlocked(value) => write!(out, "session_unlocked = {value}")?,
        IdleAtLeast { ms } => write!(out, "idle_at_least_ms = {ms}")?,
        ModeIn(modes) => {
            write!(out, "mode_in = ")?;
            array(out, modes.iter().map(|mode| mode.protocol_name()), string)?;
        }
        RefreshRate { op, hz } => {
            write!(out, "refresh_rate = {{op = {:?}, hz = {hz}}}", op.as_str())?
        }
        TimeWindow(window) => {
            write!(
                out,
                "time_window = {{from = {}, to = {}, days = ",
                time(window.from),
                time(window.to)
            )?;
            weekdays(out, window.days)?;
            write!(out, "}}")?;
        }
        All(items) | Any(items) => {
            write!(
                out,
                "{} = ",
                if matches!(test, All(_)) { "all" } else { "any" }
            )?;
            array(out, items.iter(), condition)?;
        }
        Not(item) => {
            write!(out, "not = ")?;
            condition(out, item)?;
        }
    }
    write!(out, "}}")
}

fn action(out: &mut String, change: &ControlChange) -> fmt::Result {
    use ControlChange::*;
    match change {
        SetPerformanceMode { mode } => write!(
            out,
            "{{field = \"performanceMode\", value = {:?}}}",
            mode.protocol_name()
        ),
        SetCoolerBoost { enabled } => write!(out, "{{field = \"coolerBoost\", value = {enabled}}}"),
        SetChargeLimit { percent } => write!(out, "{{field = \"chargeLimit\", value = {percent}}}"),
        SetDisplayRefreshRate { hz } => {
            write!(out, "{{field = \"displayRefreshRate\", value = {hz}}}")
        }
        SetDisplayBrightness { percent } => {
            write!(out, "{{field = \"displayBrightness\", value = {percent}}}")
        }
        SetColorProfile { profile_id } => write!(
            out,
            "{{field = \"colorProfile\", value = {}}}",
            quoted(profile_id)
        ),
        SetFanCurve { channel, curve } => {
            write!(
                out,
                "{{field = \"fanCurve\", channel = {:?}, points = ",
                if *channel == FanChannel::Cpu {
                    "cpu"
                } else {
                    "gpu"
                }
            )?;
            array(out, curve.points.iter(), |out, point| {
                write!(
                    out,
                    "{{temperatureC = {}, dutyPercent = {}}}",
                    point.temperature_c, point.duty_percent
                )
            })?;
            write!(out, "}}")
        }
        SetKeyboardLighting { zones } => {
            write!(out, "{{field = \"keyboardLighting\", zones = ")?;
            array(out, zones.iter(), |out, rgb| {
                write!(out, "{{r = {}, g = {}, b = {}}}", rgb.r, rgb.g, rgb.b)
            })?;
            write!(out, "}}")
        }
        _ => Err(fmt::Error),
    }
}

fn weekdays(out: &mut String, mask: DayMask) -> fmt::Result {
    array(
        out,
        (0..7).filter(|&day| mask.contains(day)).map(DayMask::name),
        string,
    )
}

fn time(at: HhMm) -> String {
    format!("\"{:02}:{:02}\"", at.hour, at.minute)
}

fn array<T>(
    out: &mut String,
    values: impl Iterator<Item = T>,
    emit: impl Fn(&mut String, T) -> fmt::Result,
) -> fmt::Result {
    write!(out, "[")?;
    for (i, value) in values.enumerate() {
        if i != 0 {
            write!(out, ", ")?;
        }
        emit(out, value)?;
    }
    write!(out, "]")
}

fn string(out: &mut String, text: &str) -> fmt::Result {
    write!(out, "{}", quoted(text))
}

fn quoted(text: &str) -> String {
    let mut escaped = String::from("\"");
    for c in text.chars() {
        match c {
            '"' => escaped.push_str("\\\""),
            '\\' => escaped.push_str("\\\\"),
            c if c.is_control() => {
                write!(escaped, "\\u{:04X}", u32::from(c)).expect("write to String")
            }
            c => escaped.push(c),
        }
    }
    escaped.push('"');
    escaped
}
