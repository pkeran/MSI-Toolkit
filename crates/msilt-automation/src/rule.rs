//! `automation.toml` v1: the frozen schema, a strict parser and the normalized
//! rule set the engine evaluates.
//!
//! Design rules for this module:
//!
//! * Every key is checked against an explicit allowlist. A typo is a load error,
//!   never a silently ignored setting.
//! * Every list, depth and numeric range is bounded before it reaches the
//!   engine. `automation.toml` is untrusted user data.
//! * Parsing happens once, at load. The engine only ever walks the compiled
//!   representation, so an event costs no parsing and no allocation.

use msilt_core::{
    ControlChange, EffectivePowerMode, FanChannel, FanCurve, FanPoint, OsdKind, PerformanceMode,
    Rgb8, FAN_POINT_COUNT, KEYBOARD_ZONE_COUNT,
};
use thiserror::Error;
use toml::{Table, Value};

use crate::lease::TargetSlot;
use crate::world::{
    AppGateMode, CmpOp, DayMask, EvalCtx, HhMm, TimeWindow, battery_threshold_satisfied,
};

pub const AUTOMATION_SCHEMA_VERSION: i64 = 1;
/// `AUTOMATION_SPEC` §7 / `CONTRACTS.md` §7: the file is capped before parsing.
pub const MAX_CONFIG_BYTES: usize = 512 * 1024;
pub const MAX_RULES: usize = 512;
pub const MAX_CONDITION_DEPTH: usize = 8;
pub const MAX_CONDITION_LEAVES: usize = 64;
pub const MAX_ANY_BRANCHES: usize = 32;
pub const MAX_ACTIONS_PER_RULE: usize = 16;
pub const MAX_APPS_PER_GATE: usize = 64;
pub const MAX_APP_ID_BYTES: usize = 128;
pub const MAX_RULE_ID_BYTES: usize = 64;
pub const MIN_RULE_PRIORITY: u16 = 100;
pub const MAX_RULE_PRIORITY: u16 = 799;
pub const DEFAULT_RULE_PRIORITY: u16 = 500;
pub const MAX_DEBOUNCE_ON_MS: u64 = 10_000;
pub const MAX_DEBOUNCE_OFF_MS: u64 = 30_000;
pub const MAX_COOLDOWN_MS: u64 = 600_000;
pub const MAX_HYSTERESIS_PERCENT: u8 = 20;
pub const MAX_TTL_MS: u64 = 86_400_000;
pub const MAX_IDLE_THRESHOLD_MS: u64 = 86_400_000;
pub const MAX_INTERVAL_MS: u64 = 7 * 86_400_000;
pub const MAX_SYNTAX_NESTING: usize = 64;

const TOP_KEYS: &[&str] = &["schema", "limits", "rules"];
const LIMIT_KEYS: &[&str] = &[
    "max_rules",
    "max_condition_depth",
    "min_timer_period_ms",
    "ec_writes_per_10s",
    "ec_min_gap_ms",
    "battery_hysteresis_percent",
    "utc_offset_minutes",
    "max_actions_per_event",
    "lease_depth",
    "flap_window_ms",
    "flap_threshold",
    "flap_freeze_ms",
];
const RULE_KEYS: &[&str] = &[
    "id",
    "name",
    "enabled",
    "priority",
    "debounce_on_ms",
    "debounce_off_ms",
    "cooldown_ms",
    "hysteresis_percent",
    "ttl_ms",
    "notify",
    "trigger",
    "condition",
    "actions",
];

#[derive(Debug, Error)]
pub enum RuleError {
    #[error("automation config is {size} bytes; the limit is {limit}")]
    TooLarge { size: usize, limit: usize },
    #[error("automation config is not valid UTF-8")]
    NotUtf8,
    #[error("invalid TOML: {0}")]
    Toml(String),
    #[error("automation config nests brackets deeper than {limit}")]
    SyntaxTooDeep { limit: usize },
    #[error("unsupported automation schema {0}; expected 1")]
    UnsupportedSchema(i64),
    #[error("{path}: {message}")]
    Invalid { path: String, message: String },
    #[error("{path}: unknown key `{key}`")]
    UnknownKey { path: String, key: String },
    #[error("{path}: missing required key `{key}`")]
    MissingKey { path: String, key: String },
    #[error("too many rules: {count} exceeds {max}")]
    TooManyRules { count: usize, max: usize },
    #[error("duplicate rule id `{id}`")]
    DuplicateRuleId { id: String },
    #[error("rule id `{id}` and `{other}` map to the same lease owner")]
    OwnerIdCollision { id: String, other: String },
    #[error("{path}: condition depth {depth} exceeds {max}")]
    ConditionTooDeep {
        path: String,
        depth: usize,
        max: usize,
    },
    #[error("{path}: {count} condition leaves exceed {max}")]
    TooManyLeaves {
        path: String,
        count: usize,
        max: usize,
    },
}

fn invalid(path: &str, message: impl Into<String>) -> RuleError {
    RuleError::Invalid {
        path: path.to_owned(),
        message: message.into(),
    }
}

/// Tunables a rule set may tighten but not loosen. Defaults are the frozen
/// safety floors from `AUTOMATION_SPEC` §4.3 / §4.4 / §6.1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuleLimits {
    pub max_rules: u16,
    pub max_condition_depth: u8,
    pub min_timer_period_ms: u64,
    pub ec_writes_per_10s: u8,
    pub ec_min_gap_ms: u64,
    pub battery_hysteresis_percent: u8,
    /// Local offset used by `daily` and `time_window`; Core v1 only carries UTC.
    pub utc_offset_minutes: i16,
    pub max_actions_per_event: u8,
    pub lease_depth: u8,
    pub flap_window_ms: u64,
    pub flap_threshold: u8,
    pub flap_freeze_ms: u64,
}

impl Default for RuleLimits {
    fn default() -> Self {
        Self {
            max_rules: MAX_RULES as u16,
            max_condition_depth: MAX_CONDITION_DEPTH as u8,
            min_timer_period_ms: 60_000,
            ec_writes_per_10s: 4,
            ec_min_gap_ms: 250,
            battery_hysteresis_percent: 3,
            utc_offset_minutes: 0,
            max_actions_per_event: 32,
            lease_depth: 8,
            flap_window_ms: 60_000,
            flap_threshold: 5,
            flap_freeze_ms: 120_000,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForegroundExit {
    /// Release when the app stops being foreground (alt-tab counts).
    ForegroundLost,
    /// Release only when the tracked instance exits.
    ProcessExited,
    Both,
}

impl ForegroundExit {
    const fn gate_mode(self) -> AppGateMode {
        match self {
            Self::ForegroundLost => AppGateMode::Foreground,
            Self::ProcessExited => AppGateMode::Running,
            Self::Both => AppGateMode::Either,
        }
    }
}

/// What makes the engine look at a rule.
///
/// Trigger kinds that carry a `latch` (interval, daily, hotkey) cannot arm a
/// rule before their own pulse has been observed, so a rule cannot fire just
/// because some unrelated event arrived.
#[derive(Debug, Clone)]
pub enum Trigger {
    Power,
    Battery,
    PowerSaving,
    EffectivePower,
    Foreground {
        apps: Vec<String>,
        exit: ForegroundExit,
    },
    Process {
        apps: Vec<String>,
    },
    Display,
    Session,
    Idle {
        threshold_ms: u64,
    },
    Interval {
        every_ms: u64,
    },
    Daily {
        at: HhMm,
        days: DayMask,
    },
    Hotkey {
        id: String,
    },
    State,
}

impl Trigger {
    pub const fn kind_str(&self) -> &'static str {
        match self {
            Self::Power => "power",
            Self::Battery => "battery",
            Self::PowerSaving => "power_saving",
            Self::EffectivePower => "effective_power",
            Self::Foreground { .. } => "foreground",
            Self::Process { .. } => "process",
            Self::Display => "display",
            Self::Session => "session",
            Self::Idle { .. } => "idle",
            Self::Interval { .. } => "interval",
            Self::Daily { .. } => "daily",
            Self::Hotkey { .. } => "hotkey",
            Self::State => "state",
        }
    }

    /// Trigger kinds whose pulse must be observed before the rule may arm.
    pub const fn needs_latch(&self) -> bool {
        matches!(
            self,
            Self::Interval { .. } | Self::Daily { .. } | Self::Hotkey { .. }
        )
    }

    pub fn app_gate(&self) -> Option<(&[String], AppGateMode)> {
        match self {
            Self::Foreground { apps, exit } => Some((apps, exit.gate_mode())),
            Self::Process { apps } => Some((apps, AppGateMode::Running)),
            _ => None,
        }
    }
}

/// Condition tree. Evaluated as a pure function of the event facts.
#[derive(Debug, Clone)]
pub enum Condition {
    Always,
    AcConnected(bool),
    BatteryPercent {
        op: CmpOp,
        value: u8,
    },
    PowerSaving(bool),
    EffectivePowerModeIn(Vec<EffectivePowerMode>),
    AppForeground(Vec<String>),
    AppRunning(Vec<String>),
    ExternalDisplays {
        op: CmpOp,
        count: u16,
    },
    SessionUnlocked(bool),
    IdleAtLeast {
        ms: u64,
    },
    ModeIn(Vec<PerformanceMode>),
    RefreshRate {
        op: CmpOp,
        hz: u16,
    },
    TimeWindow(TimeWindow),
    All(Vec<Condition>),
    Any(Vec<Condition>),
    Not(Box<Condition>),
}

impl Condition {
    pub fn eval(&self, ctx: EvalCtx<'_>) -> bool {
        match self {
            Self::Always => true,
            Self::AcConnected(expected) => ctx.world.ac_connected == Some(*expected),
            Self::BatteryPercent { op, value } => ctx.world.battery_percent.is_some_and(|actual| {
                battery_threshold_satisfied(*op, *value, actual, ctx.mode, ctx.hysteresis_percent)
            }),
            Self::PowerSaving(expected) => ctx.world.power_saving == Some(*expected),
            Self::EffectivePowerModeIn(modes) => ctx
                .world
                .effective_power_mode
                .is_some_and(|mode| modes.contains(&mode)),
            Self::AppForeground(apps) => ctx
                .world
                .app_gate_matches(apps, AppGateMode::Foreground),
            Self::AppRunning(apps) => ctx.world.app_gate_matches(apps, AppGateMode::Running),
            Self::ExternalDisplays { op, count } => ctx
                .world
                .external_displays
                .is_some_and(|actual| op.matches(actual, *count)),
            Self::SessionUnlocked(expected) => ctx.world.session_unlocked == Some(*expected),
            Self::IdleAtLeast { ms } => ctx.world.idle_ms.is_some_and(|idle| idle >= *ms),
            Self::ModeIn(modes) => ctx
                .world
                .performance_mode
                .is_some_and(|mode| modes.contains(&mode)),
            Self::RefreshRate { op, hz } => ctx
                .world
                .refresh_hz
                .is_some_and(|actual| op.matches(actual, *hz)),
            Self::TimeWindow(window) => window.contains(ctx.unix_ms, ctx.utc_offset_minutes),
            Self::All(items) => items.iter().all(|item| item.eval(ctx)),
            Self::Any(items) => items.iter().any(|item| item.eval(ctx)),
            Self::Not(inner) => !inner.eval(ctx),
        }
    }

    pub fn leaf_count(&self) -> u16 {
        match self {
            Self::All(items) | Self::Any(items) => {
                items.iter().map(Self::leaf_count).sum::<u16>()
            }
            Self::Not(inner) => inner.leaf_count(),
            _ => 1,
        }
    }

    /// Next instant a `time_window` node changes state, so the scheduler can arm
    /// a single wake-up instead of sampling the clock.
    pub fn next_time_boundary(&self, unix_ms: u64, utc_offset_minutes: i16) -> Option<u64> {
        match self {
            Self::TimeWindow(window) => window.next_boundary(unix_ms, utc_offset_minutes),
            Self::All(items) | Self::Any(items) => items
                .iter()
                .filter_map(|item| item.next_time_boundary(unix_ms, utc_offset_minutes))
                .min(),
            Self::Not(inner) => inner.next_time_boundary(unix_ms, utc_offset_minutes),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct AutomationRule {
    /// Stable numeric lease owner derived from `id` (FNV-1a), so a reload keeps
    /// leases alive for rules that did not change.
    pub owner: u32,
    pub id: String,
    pub name: String,
    pub enabled: bool,
    pub priority: u16,
    /// Condition leaf count (plus the app gate). Breaks priority ties.
    pub specificity: u16,
    pub debounce_on_ms: u64,
    pub debounce_off_ms: u64,
    pub cooldown_ms: u64,
    pub hysteresis_percent: u8,
    pub ttl_ms: Option<u64>,
    pub notify: Vec<OsdKind>,
    pub trigger: Trigger,
    pub condition: Condition,
    pub actions: Vec<ControlChange>,
    /// Bitmask of the [`TargetSlot`]s this rule proposes, precomputed at load so
    /// the hot path never rescans `actions`.
    pub target_mask: u16,
}

#[derive(Debug, Clone)]
pub struct AutomationRuleSet {
    pub limits: RuleLimits,
    pub rules: Vec<AutomationRule>,
    pub source_bytes: usize,
}

impl Default for AutomationRuleSet {
    fn default() -> Self {
        Self {
            limits: RuleLimits::default(),
            rules: Vec::new(),
            source_bytes: 0,
        }
    }
}

impl AutomationRuleSet {
    pub fn parse(bytes: &[u8]) -> Result<Self, RuleError> {
        if bytes.len() > MAX_CONFIG_BYTES {
            return Err(RuleError::TooLarge {
                size: bytes.len(),
                limit: MAX_CONFIG_BYTES,
            });
        }
        let text = std::str::from_utf8(bytes).map_err(|_| RuleError::NotUtf8)?;
        let mut set = Self::parse_str(text)?;
        set.source_bytes = bytes.len();
        Ok(set)
    }

    pub fn parse_str(text: &str) -> Result<Self, RuleError> {
        if text.len() > MAX_CONFIG_BYTES {
            return Err(RuleError::TooLarge {
                size: text.len(),
                limit: MAX_CONFIG_BYTES,
            });
        }
        check_syntax_nesting(text)?;
        let root: Table = text
            .parse()
            .map_err(|error: toml::de::Error| RuleError::Toml(error.to_string()))?;
        let mut set = parse_document(&root)?;
        set.source_bytes = text.len();
        Ok(set)
    }

    pub fn len(&self) -> usize {
        self.rules.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    pub fn rule_by_owner(&self, owner: u32) -> Option<&AutomationRule> {
        self.rules.iter().find(|rule| rule.owner == owner)
    }

    pub fn rule_by_id(&self, id: &str) -> Option<&AutomationRule> {
        self.rules.iter().find(|rule| rule.id == id)
    }
}

/// Approximate but conservative pre-scan so a bracket bomb cannot reach the
/// recursive TOML parser. Strings and comments are skipped.
fn check_syntax_nesting(text: &str) -> Result<(), RuleError> {
    #[derive(Clone, Copy)]
    enum Str {
        None,
        Basic,
        Literal,
        MultiBasic,
        MultiLiteral,
    }

    let bytes = text.as_bytes();
    let mut index = 0usize;
    let mut depth = 0usize;
    let mut state = Str::None;
    while index < bytes.len() {
        let byte = bytes[index];
        match state {
            Str::None => match byte {
                b'#' => {
                    while index < bytes.len() && bytes[index] != b'\n' {
                        index += 1;
                    }
                    continue;
                }
                b'"' => {
                    if bytes[index..].starts_with(b"\"\"\"") {
                        state = Str::MultiBasic;
                        index += 3;
                        continue;
                    }
                    state = Str::Basic;
                }
                b'\'' => {
                    if bytes[index..].starts_with(b"'''") {
                        state = Str::MultiLiteral;
                        index += 3;
                        continue;
                    }
                    state = Str::Literal;
                }
                b'[' | b'{' => {
                    depth += 1;
                    if depth > MAX_SYNTAX_NESTING {
                        return Err(RuleError::SyntaxTooDeep {
                            limit: MAX_SYNTAX_NESTING,
                        });
                    }
                }
                b']' | b'}' => depth = depth.saturating_sub(1),
                _ => {}
            },
            Str::Basic => match byte {
                b'\\' => index += 1,
                b'"' => state = Str::None,
                _ => {}
            },
            Str::Literal => {
                if byte == b'\'' {
                    state = Str::None;
                }
            }
            Str::MultiBasic => {
                if bytes[index..].starts_with(b"\"\"\"") {
                    state = Str::None;
                    index += 3;
                    continue;
                }
            }
            Str::MultiLiteral => {
                if bytes[index..].starts_with(b"'''") {
                    state = Str::None;
                    index += 3;
                    continue;
                }
            }
        }
        index += 1;
    }
    Ok(())
}

fn parse_document(root: &Table) -> Result<AutomationRuleSet, RuleError> {
    check_keys(root, "automation", TOP_KEYS)?;
    let schema = require(root, "schema", "automation")?;
    let schema = expect_int(schema, "automation.schema")?;
    if schema != AUTOMATION_SCHEMA_VERSION {
        return Err(RuleError::UnsupportedSchema(schema));
    }
    let limits = match root.get("limits") {
        Some(value) => parse_limits(value)?,
        None => RuleLimits::default(),
    };
    let rules_value = require(root, "rules", "automation")?;
    let Value::Array(items) = rules_value else {
        return Err(invalid(
            "automation.rules",
            "expected an array of `[[rules]]` tables",
        ));
    };
    if items.len() > usize::from(limits.max_rules) {
        return Err(RuleError::TooManyRules {
            count: items.len(),
            max: usize::from(limits.max_rules),
        });
    }

    let mut rules = Vec::with_capacity(items.len());
    let mut seen: Vec<(u32, String)> = Vec::with_capacity(items.len());
    for (index, item) in items.iter().enumerate() {
        let path = format!("automation.rules[{index}]");
        let rule = parse_rule(item, &path, &limits)?;
        if seen.iter().any(|(_, id)| id.eq_ignore_ascii_case(&rule.id)) {
            return Err(RuleError::DuplicateRuleId { id: rule.id.clone() });
        }
        if let Some((_, other)) = seen.iter().find(|(owner, _)| *owner == rule.owner) {
            return Err(RuleError::OwnerIdCollision {
                id: rule.id.clone(),
                other: other.clone(),
            });
        }
        seen.push((rule.owner, rule.id.clone()));
        rules.push(rule);
    }

    Ok(AutomationRuleSet {
        limits,
        rules,
        source_bytes: 0,
    })
}

fn parse_limits(value: &Value) -> Result<RuleLimits, RuleError> {
    let path = "automation.limits";
    let table = expect_table(value, path)?;
    check_keys(table, path, LIMIT_KEYS)?;
    let defaults = RuleLimits::default();
    let limits = RuleLimits {
        max_rules: opt_int(table, "max_rules", path, 1, MAX_RULES as i64)?
            .map_or(defaults.max_rules, |value| value as u16),
        max_condition_depth: opt_int(table, "max_condition_depth", path, 1, MAX_CONDITION_DEPTH as i64)?
            .map_or(defaults.max_condition_depth, |value| value as u8),
        min_timer_period_ms: opt_int(
            table,
            "min_timer_period_ms",
            path,
            60_000,
            MAX_INTERVAL_MS as i64,
        )?
        .map_or(defaults.min_timer_period_ms, |value| value as u64),
        ec_writes_per_10s: opt_int(table, "ec_writes_per_10s", path, 1, 4)?
            .map_or(defaults.ec_writes_per_10s, |value| value as u8),
        ec_min_gap_ms: opt_int(table, "ec_min_gap_ms", path, 250, 10_000)?
            .map_or(defaults.ec_min_gap_ms, |value| value as u64),
        battery_hysteresis_percent: opt_int(
            table,
            "battery_hysteresis_percent",
            path,
            0,
            i64::from(MAX_HYSTERESIS_PERCENT),
        )?
        .map_or(defaults.battery_hysteresis_percent, |value| value as u8),
        utc_offset_minutes: opt_int(table, "utc_offset_minutes", path, -720, 840)?
            .map_or(defaults.utc_offset_minutes, |value| value as i16),
        max_actions_per_event: opt_int(table, "max_actions_per_event", path, 1, 32)?
            .map_or(defaults.max_actions_per_event, |value| value as u8),
        lease_depth: opt_int(table, "lease_depth", path, 1, 8)?
            .map_or(defaults.lease_depth, |value| value as u8),
        flap_window_ms: opt_int(table, "flap_window_ms", path, 1_000, 600_000)?
            .map_or(defaults.flap_window_ms, |value| value as u64),
        flap_threshold: opt_int(table, "flap_threshold", path, 2, 8)?
            .map_or(defaults.flap_threshold, |value| value as u8),
        flap_freeze_ms: opt_int(table, "flap_freeze_ms", path, 1_000, 3_600_000)?
            .map_or(defaults.flap_freeze_ms, |value| value as u64),
    };
    Ok(limits)
}

fn parse_rule(value: &Value, path: &str, limits: &RuleLimits) -> Result<AutomationRule, RuleError> {
    let table = expect_table(value, path)?;
    check_keys(table, path, RULE_KEYS)?;

    let id = require_str(table, "id", path)?;
    validate_rule_id(&id, &format!("{path}.id"))?;
    let name = match table.get("name") {
        Some(value) => expect_str(value, &format!("{path}.name"))?,
        None => id.clone(),
    };
    if name.len() > 128 {
        return Err(invalid(&format!("{path}.name"), "name must be 1..=128 bytes"));
    }

    let enabled = opt_bool(table, "enabled", path)?.unwrap_or(true);
    let priority = opt_int(
        table,
        "priority",
        path,
        i64::from(MIN_RULE_PRIORITY),
        i64::from(MAX_RULE_PRIORITY),
    )?
    .map_or(DEFAULT_RULE_PRIORITY, |value| value as u16);
    let debounce_on_ms = opt_int(table, "debounce_on_ms", path, 0, MAX_DEBOUNCE_ON_MS as i64)?
        .map_or(800, |value| value as u64);
    let debounce_off_ms = opt_int(
        table,
        "debounce_off_ms",
        path,
        0,
        MAX_DEBOUNCE_OFF_MS as i64,
    )?
    .map_or(1_500, |value| value as u64);
    let cooldown_ms = opt_int(table, "cooldown_ms", path, 0, MAX_COOLDOWN_MS as i64)?
        .map_or(3_000, |value| value as u64);
    let hysteresis_percent = opt_int(
        table,
        "hysteresis_percent",
        path,
        0,
        i64::from(MAX_HYSTERESIS_PERCENT),
    )?
    .map_or(limits.battery_hysteresis_percent, |value| value as u8);
    let ttl_ms = opt_int(table, "ttl_ms", path, 0, MAX_TTL_MS as i64)?
        .filter(|value| *value > 0)
        .map(|value| value as u64);

    let notify = match table.get("notify") {
        Some(value) => parse_notify(value, &format!("{path}.notify"))?,
        None => Vec::new(),
    };

    let trigger = parse_trigger(
        require(table, "trigger", path)?,
        &format!("{path}.trigger"),
        limits,
    )?;
    let mut leaves = 0usize;
    let condition = parse_condition(
        require(table, "condition", path)?,
        &format!("{path}.condition"),
        1,
        limits,
        &mut leaves,
    )?;
    let actions = parse_actions(
        require(table, "actions", path)?,
        &format!("{path}.actions"),
    )?;

    let specificity = condition
        .leaf_count()
        .max(1)
        .saturating_add(u16::from(trigger.app_gate().is_some()));
    let mut target_mask = 0u16;
    for action in &actions {
        if let Some(slot) = TargetSlot::of_change(action) {
            target_mask |= 1 << slot.index();
        }
    }

    Ok(AutomationRule {
        owner: owner_id(&id),
        id,
        name,
        enabled,
        priority,
        specificity,
        debounce_on_ms,
        debounce_off_ms,
        cooldown_ms,
        hysteresis_percent,
        ttl_ms,
        notify,
        trigger,
        condition,
        actions,
        target_mask,
    })
}

fn validate_rule_id(id: &str, path: &str) -> Result<(), RuleError> {
    if id.is_empty() || id.len() > MAX_RULE_ID_BYTES {
        return Err(invalid(
            path,
            format!("rule id must be 1..={MAX_RULE_ID_BYTES} bytes"),
        ));
    }
    if !id
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b':' | b'/'))
    {
        return Err(invalid(
            path,
            "rule id may only use ASCII letters, digits and `._-:/`",
        ));
    }
    Ok(())
}

/// FNV-1a. Only used to derive a stable `LeaseOwner::Rule` id from the string
/// id; collisions are rejected at load.
fn owner_id(id: &str) -> u32 {
    let mut hash = 0x811c_9dc5u32;
    for byte in id.as_bytes() {
        hash ^= u32::from(*byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    if hash == 0 { 1 } else { hash }
}

fn parse_notify(value: &Value, path: &str) -> Result<Vec<OsdKind>, RuleError> {
    let Value::Array(items) = value else {
        return Err(invalid(path, "expected an array of OSD kinds"));
    };
    let mut kinds = Vec::with_capacity(items.len());
    for (index, item) in items.iter().enumerate() {
        let kind = match expect_str(item, &format!("{path}[{index}]"))?.as_str() {
            "performanceMode" => OsdKind::PerformanceMode,
            "coolerBoost" => OsdKind::CoolerBoost,
            "displayRefreshRate" => OsdKind::DisplayRefreshRate,
            "automationApplied" => OsdKind::AutomationApplied,
            other => {
                return Err(invalid(
                    &format!("{path}[{index}]"),
                    format!("unknown OSD kind `{other}`"),
                ));
            }
        };
        if !kinds.contains(&kind) {
            kinds.push(kind);
        }
    }
    Ok(kinds)
}

fn parse_trigger(value: &Value, path: &str, limits: &RuleLimits) -> Result<Trigger, RuleError> {
    let table = expect_table(value, path)?;
    let on = require_str(table, "on", path)?;
    let allowed: &[&str] = match on.as_str() {
        "power" | "battery" | "power_saving" | "effective_power" | "display" | "session"
        | "state" => &["on"],
        "foreground" => &["on", "apps", "exit"],
        "process" => &["on", "apps"],
        "idle" => &["on", "threshold_ms"],
        "interval" => &["on", "every_ms"],
        "daily" => &["on", "at", "days"],
        "hotkey" => &["on", "id"],
        other => {
            return Err(invalid(path, format!("unknown trigger `{other}`")));
        }
    };
    check_keys(table, path, allowed)?;

    Ok(match on.as_str() {
        "power" => Trigger::Power,
        "battery" => Trigger::Battery,
        "power_saving" => Trigger::PowerSaving,
        "effective_power" => Trigger::EffectivePower,
        "display" => Trigger::Display,
        "session" => Trigger::Session,
        "state" => Trigger::State,
        "foreground" => {
            let apps = parse_app_list(require(table, "apps", path)?, &format!("{path}.apps"))?;
            let exit = match table.get("exit") {
                Some(value) => match expect_str(value, &format!("{path}.exit"))?.as_str() {
                    "foreground_lost" => ForegroundExit::ForegroundLost,
                    "process_exit" => ForegroundExit::ProcessExited,
                    "both" => ForegroundExit::Both,
                    other => {
                        return Err(invalid(
                            &format!("{path}.exit"),
                            format!("unknown exit mode `{other}`"),
                        ));
                    }
                },
                None => ForegroundExit::ForegroundLost,
            };
            Trigger::Foreground { apps, exit }
        }
        "process" => Trigger::Process {
            apps: parse_app_list(require(table, "apps", path)?, &format!("{path}.apps"))?,
        },
        "idle" => Trigger::Idle {
            threshold_ms: require_int(
                table,
                "threshold_ms",
                path,
                1_000,
                MAX_IDLE_THRESHOLD_MS as i64,
            )? as u64,
        },
        "interval" => {
            let min = limits.min_timer_period_ms.max(60_000);
            Trigger::Interval {
                every_ms: require_int(table, "every_ms", path, min as i64, MAX_INTERVAL_MS as i64)?
                    as u64,
            }
        }
        "daily" => Trigger::Daily {
            at: parse_hhmm(require(table, "at", path)?, &format!("{path}.at"))?,
            days: match table.get("days") {
                Some(value) => parse_days(value, &format!("{path}.days"))?,
                None => DayMask::ALL,
            },
        },
        "hotkey" => {
            let id = require_str(table, "id", path)?;
            if id.is_empty() || id.len() > MAX_RULE_ID_BYTES {
                return Err(invalid(
                    &format!("{path}.id"),
                    format!("hotkey id must be 1..={MAX_RULE_ID_BYTES} bytes"),
                ));
            }
            if id.bytes().any(|byte| byte.is_ascii_control()) {
                return Err(invalid(
                    &format!("{path}.id"),
                    "hotkey id must not contain control characters",
                ));
            }
            Trigger::Hotkey { id }
        }
        _ => unreachable!("trigger name validated above"),
    })
}

fn parse_condition(
    value: &Value,
    path: &str,
    depth: usize,
    limits: &RuleLimits,
    leaves: &mut usize,
) -> Result<Condition, RuleError> {
    let max_depth = usize::from(limits.max_condition_depth);
    if depth > max_depth {
        return Err(RuleError::ConditionTooDeep {
            path: path.to_owned(),
            depth,
            max: max_depth,
        });
    }
    let table = expect_table(value, path)?;
    if table.len() != 1 {
        return Err(invalid(
            path,
            "a condition must be a table with exactly one key",
        ));
    }
    let Some((key, raw)) = table.iter().next() else {
        return Err(invalid(path, "empty condition table"));
    };

    match key.as_str() {
        "all" | "any" => {
            let branch_path = format!("{path}.{key}");
            let Value::Array(items) = raw else {
                return Err(invalid(&branch_path, "expected an array of conditions"));
            };
            let limit = if key == "any" {
                MAX_ANY_BRANCHES
            } else {
                MAX_CONDITION_LEAVES
            };
            if items.is_empty() {
                return Err(invalid(&branch_path, "must not be empty"));
            }
            if items.len() > limit {
                return Err(invalid(
                    &branch_path,
                    format!("at most {limit} branches are allowed"),
                ));
            }
            let mut conditions = Vec::with_capacity(items.len());
            for (index, item) in items.iter().enumerate() {
                conditions.push(parse_condition(
                    item,
                    &format!("{branch_path}[{index}]"),
                    depth + 1,
                    limits,
                    leaves,
                )?);
            }
            Ok(if key == "any" {
                Condition::Any(conditions)
            } else {
                Condition::All(conditions)
            })
        }
        "not" => {
            let inner = parse_condition(raw, &format!("{path}.not"), depth + 1, limits, leaves)?;
            Ok(Condition::Not(Box::new(inner)))
        }
        _ => {
            *leaves += 1;
            if *leaves > MAX_CONDITION_LEAVES {
                return Err(RuleError::TooManyLeaves {
                    path: path.to_owned(),
                    count: *leaves,
                    max: MAX_CONDITION_LEAVES,
                });
            }
            parse_leaf(key, raw, path)
        }
    }
}

fn parse_leaf(key: &str, value: &Value, path: &str) -> Result<Condition, RuleError> {
    let leaf_path = format!("{path}.{key}");
    Ok(match key {
        "always" => {
            if !expect_bool(value, &leaf_path)? {
                return Err(invalid(&leaf_path, "`always` must be true"));
            }
            Condition::Always
        }
        "ac_connected" => Condition::AcConnected(expect_bool(value, &leaf_path)?),
        "battery_percent" => {
            let (op, number) = parse_op_number(value, &leaf_path, "value")?;
            Condition::BatteryPercent {
                op,
                value: clamp_int(number, &leaf_path, 0, 100)? as u8,
            }
        }
        "power_saving" => Condition::PowerSaving(expect_bool(value, &leaf_path)?),
        "effective_power_mode_in" => {
            let modes = parse_string_list(value, &leaf_path, 8)?;
            let mut parsed = Vec::with_capacity(modes.len());
            for (index, mode) in modes.iter().enumerate() {
                let mode = parse_effective_power_mode(mode, &format!("{leaf_path}[{index}]"))?;
                if !parsed.contains(&mode) {
                    parsed.push(mode);
                }
            }
            Condition::EffectivePowerModeIn(parsed)
        }
        "app_foreground" => Condition::AppForeground(parse_app_list(value, &leaf_path)?),
        "app_running" => Condition::AppRunning(parse_app_list(value, &leaf_path)?),
        "external_displays" => {
            let (op, number) = parse_op_number(value, &leaf_path, "count")?;
            Condition::ExternalDisplays {
                op,
                count: clamp_int(number, &leaf_path, 0, 16)? as u16,
            }
        }
        "session_unlocked" => Condition::SessionUnlocked(expect_bool(value, &leaf_path)?),
        "idle_at_least_ms" => Condition::IdleAtLeast {
            ms: clamp_int(
                expect_int(value, &leaf_path)?,
                &leaf_path,
                0,
                MAX_IDLE_THRESHOLD_MS as i64,
            )? as u64,
        },
        "mode_in" => {
            let modes = parse_string_list(value, &leaf_path, 8)?;
            let mut parsed = Vec::with_capacity(modes.len());
            for (index, mode) in modes.iter().enumerate() {
                let mode = parse_performance_mode(mode, &format!("{leaf_path}[{index}]"))?;
                if !parsed.contains(&mode) {
                    parsed.push(mode);
                }
            }
            Condition::ModeIn(parsed)
        }
        "refresh_rate" => {
            let (op, number) = parse_op_number(value, &leaf_path, "hz")?;
            Condition::RefreshRate {
                op,
                hz: clamp_int(number, &leaf_path, 1, 1_000)? as u16,
            }
        }
        "time_window" => {
            let table = expect_table(value, &leaf_path)?;
            check_keys(table, &leaf_path, &["from", "to", "days"])?;
            Condition::TimeWindow(TimeWindow {
                from: parse_hhmm(require(table, "from", &leaf_path)?, &format!("{leaf_path}.from"))?,
                to: parse_hhmm(require(table, "to", &leaf_path)?, &format!("{leaf_path}.to"))?,
                days: match table.get("days") {
                    Some(value) => parse_days(value, &format!("{leaf_path}.days"))?,
                    None => DayMask::ALL,
                },
            })
        }
        other => {
            return Err(invalid(path, format!("unknown condition `{other}`")));
        }
    })
}

fn parse_op_number(
    value: &Value,
    path: &str,
    number_key: &str,
) -> Result<(CmpOp, i64), RuleError> {
    let table = expect_table(value, path)?;
    check_keys(table, path, &["op", number_key])?;
    let op_text = require_str(table, "op", path)?;
    let op = CmpOp::parse(&op_text).ok_or_else(|| {
        invalid(
            &format!("{path}.op"),
            format!("unknown comparison `{op_text}`; expected lt|le|eq|ge|gt"),
        )
    })?;
    let number = expect_int(
        require(table, number_key, path)?,
        &format!("{path}.{number_key}"),
    )?;
    Ok((op, number))
}

fn parse_actions(value: &Value, path: &str) -> Result<Vec<ControlChange>, RuleError> {
    let Value::Array(items) = value else {
        return Err(invalid(path, "expected an array of action tables"));
    };
    if items.is_empty() {
        return Err(invalid(path, "a rule needs at least one action"));
    }
    if items.len() > MAX_ACTIONS_PER_RULE {
        return Err(invalid(
            path,
            format!("at most {MAX_ACTIONS_PER_RULE} actions per rule"),
        ));
    }
    let mut actions = Vec::with_capacity(items.len());
    let mut targets: Vec<u8> = Vec::with_capacity(items.len());
    for (index, item) in items.iter().enumerate() {
        let item_path = format!("{path}[{index}]");
        let change = parse_action(item, &item_path)?;
        let slot = TargetSlot::of_change(&change)
            .ok_or_else(|| invalid(&item_path, "action target is not arbitrated by this engine"))?;
        if targets.contains(&(slot.index() as u8)) {
            return Err(invalid(
                &item_path,
                format!("duplicate action for `{}`", slot.as_str()),
            ));
        }
        targets.push(slot.index() as u8);
        actions.push(change);
    }
    Ok(actions)
}

fn parse_action(value: &Value, path: &str) -> Result<ControlChange, RuleError> {
    let table = expect_table(value, path)?;
    let field = require_str(table, "field", path)?;
    let change = match field.as_str() {
        "performanceMode" => {
            check_keys(table, path, &["field", "value"])?;
            ControlChange::SetPerformanceMode {
                mode: parse_performance_mode(
                    &require_str(table, "value", path)?,
                    &format!("{path}.value"),
                )?,
            }
        }
        "fanCurve" => {
            check_keys(table, path, &["field", "channel", "points"])?;
            ControlChange::SetFanCurve {
                channel: parse_fan_channel(
                    &require_str(table, "channel", path)?,
                    &format!("{path}.channel"),
                )?,
                curve: parse_fan_curve(require(table, "points", path)?, &format!("{path}.points"))?,
            }
        }
        "coolerBoost" => {
            check_keys(table, path, &["field", "value"])?;
            ControlChange::SetCoolerBoost {
                enabled: expect_bool(
                    require(table, "value", path)?,
                    &format!("{path}.value"),
                )?,
            }
        }
        "chargeLimit" => {
            check_keys(table, path, &["field", "value"])?;
            ControlChange::SetChargeLimit {
                percent: require_int(table, "value", path, 1, 100)? as u8,
            }
        }
        "displayRefreshRate" => {
            check_keys(table, path, &["field", "value"])?;
            ControlChange::SetDisplayRefreshRate {
                hz: require_int(table, "value", path, 1, 1_000)? as u16,
            }
        }
        "displayBrightness" => {
            check_keys(table, path, &["field", "value"])?;
            ControlChange::SetDisplayBrightness {
                percent: require_int(table, "value", path, 0, 100)? as u8,
            }
        }
        "colorProfile" => {
            check_keys(table, path, &["field", "value"])?;
            ControlChange::SetColorProfile {
                profile_id: require_str(table, "value", path)?,
            }
        }
        "keyboardLighting" => {
            check_keys(table, path, &["field", "zones"])?;
            ControlChange::SetKeyboardLighting {
                zones: parse_zones(require(table, "zones", path)?, &format!("{path}.zones"))?,
                effect: Default::default(),
                speed: Default::default(),
            }
        }
        other => {
            return Err(invalid(path, format!("unknown action field `{other}`")));
        }
    };
    change
        .validate_parameters()
        .map_err(|error| invalid(path, error.to_string()))?;
    Ok(change)
}

fn parse_fan_channel(text: &str, path: &str) -> Result<FanChannel, RuleError> {
    match text {
        "cpu" => Ok(FanChannel::Cpu),
        "gpu" => Ok(FanChannel::Gpu),
        other => Err(invalid(path, format!("unknown fan channel `{other}`"))),
    }
}

fn parse_fan_curve(value: &Value, path: &str) -> Result<FanCurve, RuleError> {
    let Value::Array(items) = value else {
        return Err(invalid(path, "expected an array of six fan points"));
    };
    if items.len() != FAN_POINT_COUNT {
        return Err(invalid(
            path,
            format!("expected exactly {FAN_POINT_COUNT} fan points, found {}", items.len()),
        ));
    }
    let mut points = [FanPoint {
        temperature_c: 0,
        duty_percent: 0,
    }; FAN_POINT_COUNT];
    for (index, item) in items.iter().enumerate() {
        let item_path = format!("{path}[{index}]");
        let table = expect_table(item, &item_path)?;
        check_keys(table, &item_path, &["temperatureC", "dutyPercent"])?;
        points[index] = FanPoint {
            temperature_c: require_int(table, "temperatureC", &item_path, 0, 100)? as u8,
            duty_percent: require_int(table, "dutyPercent", &item_path, 0, 150)? as u8,
        };
    }
    let curve = FanCurve { points };
    curve
        .validate_shape()
        .map_err(|error| invalid(path, error.to_string()))?;
    Ok(curve)
}

fn parse_zones(value: &Value, path: &str) -> Result<[Rgb8; KEYBOARD_ZONE_COUNT], RuleError> {
    let Value::Array(items) = value else {
        return Err(invalid(path, "expected an array of 24 RGB zones"));
    };
    if items.len() != KEYBOARD_ZONE_COUNT {
        return Err(invalid(
            path,
            format!(
                "expected exactly {KEYBOARD_ZONE_COUNT} zones, found {}",
                items.len()
            ),
        ));
    }
    let mut zones = [Rgb8 { r: 0, g: 0, b: 0 }; KEYBOARD_ZONE_COUNT];
    for (index, item) in items.iter().enumerate() {
        let item_path = format!("{path}[{index}]");
        let table = expect_table(item, &item_path)?;
        check_keys(table, &item_path, &["r", "g", "b"])?;
        zones[index] = Rgb8 {
            r: require_int(table, "r", &item_path, 0, 255)? as u8,
            g: require_int(table, "g", &item_path, 0, 255)? as u8,
            b: require_int(table, "b", &item_path, 0, 255)? as u8,
        };
    }
    Ok(zones)
}

fn parse_performance_mode(text: &str, path: &str) -> Result<PerformanceMode, RuleError> {
    match text {
        "superBattery" => Ok(PerformanceMode::SuperBattery),
        "silent" => Ok(PerformanceMode::Silent),
        "balanced" => Ok(PerformanceMode::Balanced),
        "extremePerformance" => Ok(PerformanceMode::ExtremePerformance),
        other => Err(invalid(path, format!("unknown performance mode `{other}`"))),
    }
}

fn parse_effective_power_mode(text: &str, path: &str) -> Result<EffectivePowerMode, RuleError> {
    match text {
        "batterySaver" => Ok(EffectivePowerMode::BatterySaver),
        "betterBattery" => Ok(EffectivePowerMode::BetterBattery),
        "balanced" => Ok(EffectivePowerMode::Balanced),
        "highPerformance" => Ok(EffectivePowerMode::HighPerformance),
        "maxPerformance" => Ok(EffectivePowerMode::MaxPerformance),
        "gameMode" => Ok(EffectivePowerMode::GameMode),
        "mixedReality" => Ok(EffectivePowerMode::MixedReality),
        other => Err(invalid(
            path,
            format!("unknown effective power mode `{other}`"),
        )),
    }
}

fn parse_app_list(value: &Value, path: &str) -> Result<Vec<String>, RuleError> {
    let mut apps: Vec<String> = Vec::new();
    match value {
        Value::String(text) => push_app(&mut apps, text, path)?,
        Value::Array(items) => {
            if items.is_empty() {
                return Err(invalid(path, "the app list must not be empty"));
            }
            if items.len() > MAX_APPS_PER_GATE {
                return Err(invalid(
                    path,
                    format!("at most {MAX_APPS_PER_GATE} apps are allowed"),
                ));
            }
            for (index, item) in items.iter().enumerate() {
                let item_path = format!("{path}[{index}]");
                let Value::String(text) = item else {
                    return Err(invalid(&item_path, "expected a string"));
                };
                push_app(&mut apps, text, &item_path)?;
            }
        }
        _ => {
            return Err(invalid(
                path,
                "expected an app id string or an array of app ids",
            ));
        }
    }
    Ok(apps)
}

fn push_app(apps: &mut Vec<String>, text: &str, path: &str) -> Result<(), RuleError> {
    let trimmed = text.trim();
    if trimmed.is_empty() || trimmed.len() > MAX_APP_ID_BYTES {
        return Err(invalid(
            path,
            format!("app id must be 1..={MAX_APP_ID_BYTES} bytes"),
        ));
    }
    if trimmed.bytes().any(|byte| byte.is_ascii_control()) {
        return Err(invalid(path, "app id must not contain control characters"));
    }
    let normalized = trimmed.to_ascii_lowercase();
    if !apps.contains(&normalized) {
        apps.push(normalized);
    }
    Ok(())
}

fn parse_string_list(value: &Value, path: &str, max: usize) -> Result<Vec<String>, RuleError> {
    let Value::Array(items) = value else {
        return Err(invalid(path, "expected an array of strings"));
    };
    if items.is_empty() {
        return Err(invalid(path, "must not be empty"));
    }
    if items.len() > max {
        return Err(invalid(path, format!("at most {max} entries are allowed")));
    }
    let mut parsed = Vec::with_capacity(items.len());
    for (index, item) in items.iter().enumerate() {
        parsed.push(expect_str(item, &format!("{path}[{index}]"))?);
    }
    Ok(parsed)
}

fn parse_days(value: &Value, path: &str) -> Result<DayMask, RuleError> {
    let names = parse_string_list(value, path, 7)?;
    let mut mask = 0u8;
    for (index, name) in names.iter().enumerate() {
        let day = DayMask::parse_day(name).ok_or_else(|| {
            invalid(
                &format!("{path}[{index}]"),
                format!("unknown weekday `{name}`"),
            )
        })?;
        mask |= 1 << day;
    }
    Ok(DayMask::from_bits(mask))
}

fn parse_hhmm(value: &Value, path: &str) -> Result<HhMm, RuleError> {
    let text = expect_str(value, path)?;
    HhMm::parse(&text).ok_or_else(|| invalid(path, format!("invalid time `{text}`; expected HH:MM")))
}

fn check_keys(table: &Table, path: &str, allowed: &[&str]) -> Result<(), RuleError> {
    for key in table.keys() {
        if !allowed.contains(&key.as_str()) {
            return Err(RuleError::UnknownKey {
                path: path.to_owned(),
                key: key.clone(),
            });
        }
    }
    Ok(())
}

fn require<'a>(table: &'a Table, key: &str, path: &str) -> Result<&'a Value, RuleError> {
    table.get(key).ok_or_else(|| RuleError::MissingKey {
        path: path.to_owned(),
        key: key.to_owned(),
    })
}

fn expect_table<'a>(value: &'a Value, path: &str) -> Result<&'a Table, RuleError> {
    value
        .as_table()
        .ok_or_else(|| invalid(path, "expected a table"))
}

fn expect_str(value: &Value, path: &str) -> Result<String, RuleError> {
    value
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| invalid(path, "expected a string"))
}

fn expect_bool(value: &Value, path: &str) -> Result<bool, RuleError> {
    value
        .as_bool()
        .ok_or_else(|| invalid(path, "expected a boolean"))
}

fn expect_int(value: &Value, path: &str) -> Result<i64, RuleError> {
    value
        .as_integer()
        .ok_or_else(|| invalid(path, "expected an integer"))
}

fn clamp_int(value: i64, path: &str, min: i64, max: i64) -> Result<i64, RuleError> {
    if value < min || value > max {
        return Err(invalid(path, format!("expected {min}..={max}, found {value}")));
    }
    Ok(value)
}

fn require_int(table: &Table, key: &str, path: &str, min: i64, max: i64) -> Result<i64, RuleError> {
    clamp_int(
        expect_int(require(table, key, path)?, &format!("{path}.{key}"))?,
        &format!("{path}.{key}"),
        min,
        max,
    )
}

fn opt_int(
    table: &Table,
    key: &str,
    path: &str,
    min: i64,
    max: i64,
) -> Result<Option<i64>, RuleError> {
    match table.get(key) {
        Some(value) => Ok(Some(clamp_int(
            expect_int(value, &format!("{path}.{key}"))?,
            &format!("{path}.{key}"),
            min,
            max,
        )?)),
        None => Ok(None),
    }
}

fn opt_bool(table: &Table, key: &str, path: &str) -> Result<Option<bool>, RuleError> {
    match table.get(key) {
        Some(value) => Ok(Some(expect_bool(value, &format!("{path}.{key}"))?)),
        None => Ok(None),
    }
}

fn require_str(table: &Table, key: &str, path: &str) -> Result<String, RuleError> {
    expect_str(require(table, key, path)?, &format!("{path}.{key}"))
}
