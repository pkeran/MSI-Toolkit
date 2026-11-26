//! Immutable-per-event world facts plus the pure calendar/compare helpers used
//! by conditions.
//!
//! The engine never reads a system clock: every fact arrives either from an
//! [`AutomationEvent`] (an OS push fact) or from the read-back
//! [`SystemSnapshot`]. That keeps `on_event` deterministic and lets the whole
//! crate run without any platform call.

use msilt_core::{
    AutomationEvent, EffectivePowerMode, PerformanceMode, PowerSource, SystemSnapshot,
};

/// Upper bound on tracked background instances. Bounded on purpose: a runaway
/// process-start stream must not grow the engine.
pub const MAX_RUNNING_APPS: usize = 32;

pub const MINUTE_MS: u64 = 60_000;
pub const DAY_MS: u64 = 86_400_000;

/// Which evaluation pass a condition is being asked for.
///
/// `Arm` is the strict trigger test. `Release` is the relaxed test used while a
/// lease is already held, so a value sitting on a threshold cannot oscillate
/// (battery 20% -> 21% -> 20% would otherwise re-arm on every report).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvalMode {
    Arm,
    Release,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CmpOp {
    Lt,
    Le,
    Eq,
    Ge,
    Gt,
}

impl CmpOp {
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "lt" | "<" => Some(Self::Lt),
            "le" | "<=" => Some(Self::Le),
            "eq" | "==" | "=" => Some(Self::Eq),
            "ge" | ">=" => Some(Self::Ge),
            "gt" | ">" => Some(Self::Gt),
            _ => None,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Lt => "lt",
            Self::Le => "le",
            Self::Eq => "eq",
            Self::Ge => "ge",
            Self::Gt => "gt",
        }
    }

    pub fn matches<T: Ord>(self, lhs: T, rhs: T) -> bool {
        match self {
            Self::Lt => lhs < rhs,
            Self::Le => lhs <= rhs,
            Self::Eq => lhs == rhs,
            Self::Ge => lhs >= rhs,
            Self::Gt => lhs > rhs,
        }
    }
}

/// Threshold test with release-side hysteresis.
///
/// `lt 20` with `hysteresis = 3` arms below 20% and only lets go above 23%, i.e.
/// it stays satisfied while `actual < 24`. The comparison is widened by the
/// hysteresis in whichever direction relaxes it, and is exact for `Eq`.
pub fn battery_threshold_satisfied(
    op: CmpOp,
    threshold: u8,
    actual: u8,
    mode: EvalMode,
    hysteresis_percent: u8,
) -> bool {
    if mode == EvalMode::Arm || hysteresis_percent == 0 {
        return op.matches(actual, threshold);
    }
    let hysteresis = hysteresis_percent;
    match op {
        CmpOp::Lt => actual < threshold.saturating_add(hysteresis).saturating_add(1),
        CmpOp::Le => actual <= threshold.saturating_add(hysteresis),
        CmpOp::Gt => actual > threshold.saturating_sub(hysteresis).saturating_sub(1),
        CmpOp::Ge => actual.saturating_add(hysteresis) >= threshold,
        CmpOp::Eq => actual == threshold,
    }
}

/// `HH:MM` wall-clock anchor, validated at parse time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct HhMm {
    pub hour: u8,
    pub minute: u8,
}

impl HhMm {
    pub const fn new(hour: u8, minute: u8) -> Option<Self> {
        if hour < 24 && minute < 60 {
            Some(Self { hour, minute })
        } else {
            None
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        let (hour, minute) = text.trim().split_once(':')?;
        if hour.is_empty() || hour.len() > 2 || minute.len() != 2 {
            return None;
        }
        if !hour.bytes().all(|b| b.is_ascii_digit()) || !minute.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        Self::new(hour.parse().ok()?, minute.parse().ok()?)
    }

    pub const fn minutes_of_day(self) -> u16 {
        self.hour as u16 * 60 + self.minute as u16
    }
}

/// Weekday bitset, bit 0 = Monday ... bit 6 = Sunday.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DayMask(u8);

impl DayMask {
    pub const ALL: Self = Self(0b111_1111);
    pub const WEEKDAYS: Self = Self(0b001_1111);

    pub const fn from_bits(bits: u8) -> Self {
        Self(bits & 0b111_1111)
    }

    pub const fn bits(self) -> u8 {
        self.0
    }

    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    pub const fn contains(self, day: u8) -> bool {
        day < 7 && self.0 & (1 << day) != 0
    }

    pub const fn name(day: u8) -> &'static str {
        match day {
            0 => "mon",
            1 => "tue",
            2 => "wed",
            3 => "thu",
            4 => "fri",
            5 => "sat",
            _ => "sun",
        }
    }

    pub fn parse_day(name: &str) -> Option<u8> {
        let name = name.trim().to_ascii_lowercase();
        for day in 0..7u8 {
            let short = Self::name(day);
            if name == short || name.starts_with(short) {
                return Some(day);
            }
        }
        None
    }
}

/// Local civil day index (days since the Unix epoch) and minute of day.
///
/// `utc_offset_minutes` comes from the rule set (`[limits] utc_offset_minutes`)
/// because Core v1 carries only a UTC timestamp. See the Wave 2 report in
/// `docs/collab/04-pi-auto.md` for the requested `AutomationContext` field.
pub fn local_clock(unix_ms: u64, utc_offset_minutes: i16) -> (i64, u16) {
    let offset_ms = i64::from(utc_offset_minutes) * MINUTE_MS as i64;
    let local = i64::try_from(unix_ms).unwrap_or(i64::MAX).saturating_add(offset_ms);
    let day = local.div_euclid(DAY_MS as i64);
    let minute = local.rem_euclid(DAY_MS as i64) / MINUTE_MS as i64;
    (day, minute as u16)
}

/// 0 = Monday ... 6 = Sunday. 1970-01-01 was a Thursday (index 3).
pub const fn weekday_index(day: i64) -> u8 {
    ((day + 3).rem_euclid(7)) as u8
}

/// First instant strictly after `unix_ms` that hits `at` on one of `days`.
pub fn next_daily_after(
    unix_ms: u64,
    at: HhMm,
    days: DayMask,
    utc_offset_minutes: i16,
) -> Option<u64> {
    if days.is_empty() {
        return None;
    }
    let offset_ms = i64::from(utc_offset_minutes) * MINUTE_MS as i64;
    let now = i64::try_from(unix_ms).unwrap_or(i64::MAX);
    let (day0, _) = local_clock(unix_ms, utc_offset_minutes);
    for step in 0..=7i64 {
        let day = day0 + step;
        if !days.contains(weekday_index(day)) {
            continue;
        }
        let candidate = day
            .saturating_mul(DAY_MS as i64)
            .saturating_add(i64::from(at.minutes_of_day()) * MINUTE_MS as i64)
            .saturating_sub(offset_ms);
        if candidate > now {
            return u64::try_from(candidate).ok();
        }
    }
    None
}

/// Recurring local time window; `from == to` means "all day".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimeWindow {
    pub from: HhMm,
    pub to: HhMm,
    pub days: DayMask,
}

impl TimeWindow {
    pub fn contains(&self, unix_ms: u64, utc_offset_minutes: i16) -> bool {
        let (day, minute) = local_clock(unix_ms, utc_offset_minutes);
        let weekday = weekday_index(day);
        let from = self.from.minutes_of_day();
        let to = self.to.minutes_of_day();
        if from == to {
            return self.days.contains(weekday);
        }
        if from < to {
            self.days.contains(weekday) && minute >= from && minute < to
        } else if minute >= from {
            // The window opened today and runs past midnight.
            self.days.contains(weekday)
        } else if minute < to {
            // Still inside yesterday's window.
            self.days.contains((weekday + 6) % 7)
        } else {
            false
        }
    }

    /// Next instant where the window flips state, so the engine can schedule a
    /// single wake-up instead of sampling the clock.
    pub fn next_boundary(&self, unix_ms: u64, utc_offset_minutes: i16) -> Option<u64> {
        let open = next_daily_after(unix_ms, self.from, self.days, utc_offset_minutes);
        let close = next_daily_after(unix_ms, self.to, self.days, utc_offset_minutes);
        match (open, close) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }
}

/// How an app allowlist is matched against the tracked facts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppGateMode {
    /// Only the current foreground app counts; alt-tab releases the lease.
    Foreground,
    /// Any tracked running instance counts, including a backgrounded game.
    Running,
    /// Either of the above.
    Either,
}

/// Case-insensitive match with an optional single trailing `*` wildcard.
///
/// Matching is done on the daemon-resolved `app_id` (installed/allowlisted app
/// identity), never on a bare process name; see `docs/CONTRACTS.md` §2.
pub fn app_matches(pattern: &str, app_id: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    match pattern.strip_suffix('*') {
        Some(prefix) => {
            app_id.len() >= prefix.len()
                && app_id.as_bytes()[..prefix.len()].eq_ignore_ascii_case(prefix.as_bytes())
        }
        None => pattern.eq_ignore_ascii_case(app_id),
    }
}

pub fn matches_any_app(apps: &[String], app_id: Option<&str>) -> bool {
    let Some(app_id) = app_id else {
        return false;
    };
    apps.iter().any(|pattern| app_matches(pattern, app_id))
}

impl AppGateMode {
    /// Human name used in `automation.toml` and in diagnostics.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Foreground => "foreground",
            Self::Running => "running",
            Self::Either => "either",
        }
    }
}

/// Facts that arrived with an event and that the caller must act on.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WorldChange {
    pub power_source_changed: bool,
}

/// Every derived fact the engine is allowed to read while evaluating rules.
///
/// Foreground and process facts are opaque ids; no path, handle or raw ETW
/// payload is stored.
#[derive(Debug, Clone, Default)]
pub struct World {
    pub synced: bool,
    pub revision: u64,
    pub ac_connected: Option<bool>,
    pub battery_percent: Option<u8>,
    pub power_saving: Option<bool>,
    pub effective_power_mode: Option<EffectivePowerMode>,
    pub foreground: Option<String>,
    pub foreground_instance: Option<u64>,
    pub running: Vec<(String, u64)>,
    pub external_displays: Option<u16>,
    pub session_unlocked: Option<bool>,
    pub idle_ms: Option<u64>,
    pub performance_mode: Option<PerformanceMode>,
    pub refresh_hz: Option<u16>,
}

impl World {
    /// Adopt read-back hardware facts. Only called when the snapshot revision
    /// advanced, so a fresher event fact is never clobbered by a stale snapshot.
    pub fn sync_from_snapshot(&mut self, snapshot: &SystemSnapshot) {
        self.ac_connected = snapshot
            .hardware
            .power_source
            .map(|source| matches!(source, PowerSource::Ac));
        self.battery_percent = snapshot.hardware.battery_percent;
        self.performance_mode = snapshot.hardware.performance_mode;
        self.refresh_hz = snapshot.hardware.display_refresh_hz;
        self.revision = snapshot.revision;
        self.synced = true;
    }

    /// Fold one OS-pushed fact into the world. Allocation-free except for the
    /// human-paced foreground/process-start paths, which clone one small id.
    pub fn apply_event(&mut self, event: &AutomationEvent) -> WorldChange {
        let mut change = WorldChange::default();
        match event {
            AutomationEvent::PowerSourceChanged { source } => {
                self.ac_connected = source.map(|source| matches!(source, PowerSource::Ac));
                change.power_source_changed = true;
            }
            AutomationEvent::BatteryPercentChanged { percent } => {
                self.battery_percent = *percent;
            }
            AutomationEvent::PowerSavingChanged { enabled } => {
                self.power_saving = Some(*enabled);
            }
            AutomationEvent::EffectivePowerModeChanged { mode } => {
                self.effective_power_mode = *mode;
            }
            AutomationEvent::ForegroundChanged { app_id, instance } => {
                self.set_foreground(app_id.as_deref(), *instance);
                if let (Some(app_id), Some(instance)) = (app_id.as_deref(), *instance) {
                    self.mark_running(app_id, instance);
                }
            }
            AutomationEvent::ProcessStarted { app_id, instance } => {
                self.mark_running(app_id, *instance);
            }
            AutomationEvent::ProcessExited { instance } => {
                self.mark_exited(*instance);
            }
            AutomationEvent::DisplayTopologyChanged {
                external_display_count,
            } => {
                self.external_displays = Some(*external_display_count);
            }
            AutomationEvent::SessionChanged { unlocked } => {
                self.session_unlocked = Some(*unlocked);
            }
            AutomationEvent::IdleThresholdReached { idle_ms } => {
                self.idle_ms = Some(*idle_ms);
            }
            _ => {}
        }
        change
    }

    pub fn set_foreground(&mut self, app_id: Option<&str>, instance: Option<u64>) {
        self.foreground = app_id.map(str::to_owned);
        self.foreground_instance = instance;
    }

    pub fn mark_running(&mut self, app_id: &str, instance: u64) {
        if let Some(slot) = self
            .running
            .iter_mut()
            .find(|(_, tracked)| *tracked == instance)
        {
            if !slot.0.eq_ignore_ascii_case(app_id) {
                slot.0.clear();
                slot.0.push_str(app_id);
            }
            return;
        }
        if self.running.len() >= MAX_RUNNING_APPS {
            self.running.remove(0);
        }
        self.running.push((app_id.to_owned(), instance));
    }

    pub fn mark_exited(&mut self, instance: u64) {
        self.running.retain(|(_, tracked)| *tracked != instance);
        if self.foreground_instance == Some(instance) {
            self.foreground = None;
            self.foreground_instance = None;
        }
    }

    pub fn app_gate_matches(&self, apps: &[String], mode: AppGateMode) -> bool {
        let foreground = matches_any_app(apps, self.foreground.as_deref());
        match mode {
            AppGateMode::Foreground => foreground,
            AppGateMode::Running => self
                .running
                .iter()
                .any(|(app_id, _)| matches_any_app(apps, Some(app_id))),
            AppGateMode::Either => {
                foreground
                    || self
                        .running
                        .iter()
                        .any(|(app_id, _)| matches_any_app(apps, Some(app_id)))
            }
        }
    }
}

/// Everything a condition may read. `Copy` so the recursive evaluator never
/// needs a heap frame.
#[derive(Debug, Clone, Copy)]
pub struct EvalCtx<'a> {
    pub world: &'a World,
    pub mode: EvalMode,
    pub hysteresis_percent: u8,
    pub utc_offset_minutes: i16,
    pub unix_ms: u64,
}
