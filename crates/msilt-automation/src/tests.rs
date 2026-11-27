//! Behavioural tests for the automation engine.
//!
//! Every test drives the engine with synthetic events and a synthetic clock, so
//! the suite is deterministic, platform free and does not need a daemon.

use msilt_core::{
    AutomationAction, AutomationContext, AutomationEngine, AutomationEvent, CapabilityFlags,
    ControlChange, ControlTarget, DaemonStatus, EcWriteStatus, ErrorCode, FanChannel, FanCurve,
    FanPoint, HardwareState, LeaseOwner, OsdKind, PerformanceMode, PowerSource, SystemSnapshot,
};

use crate::engine::{BlockReason, DryRunAction, DryRunInput, RuleEngine};
use crate::governor::{
    Cooldown, DebounceState, DebounceStep, EcGovernor, FlapGuard, WriteDecision, debounce,
    register_write_allowed,
};
use crate::lease::{LeaseStack, TargetSlot};
use crate::rule::{
    AutomationRuleSet, MAX_CONFIG_BYTES, RuleError, RuleLimits,
};
use crate::world::{CmpOp, DayMask, HhMm, TimeWindow, World, next_daily_after};

const T0: u64 = 1_000_000;
/// 2026-01-01T00:00:00Z, a Thursday.
const BASE_UNIX: u64 = 1_767_225_600_000;

// ---------------------------------------------------------------- harness ---

fn caps() -> CapabilityFlags {
    CapabilityFlags::PERFORMANCE_MODE
        | CapabilityFlags::DISPLAY_REFRESH_RATE
        | CapabilityFlags::DISPLAY_BRIGHTNESS
        | CapabilityFlags::COLOR_PROFILE
        | CapabilityFlags::COOLER_BOOST
        | CapabilityFlags::CHARGE_LIMIT
        | CapabilityFlags::FAN_CURVE
}

fn ready_snapshot() -> SystemSnapshot {
    SystemSnapshot {
        revision: 1,
        status: DaemonStatus::Ready,
        ec_write_status: EcWriteStatus::Enabled,
        capabilities: caps(),
        writable: caps(),
        hardware: HardwareState {
            power_source: Some(PowerSource::Ac),
            battery_percent: Some(80),
            supported_performance_modes: vec![
                PerformanceMode::SuperBattery,
                PerformanceMode::Silent,
                PerformanceMode::Balanced,
                PerformanceMode::ExtremePerformance,
            ],
            performance_mode: Some(PerformanceMode::Balanced),
            supported_refresh_rates: vec![60, 240],
            display_refresh_hz: Some(60),
            supported_charge_limits: vec![60, 80, 100],
            supported_color_profiles: vec!["factory-srgb".to_owned()],
            ..HardwareState::default()
        },
        manual_overrides: Vec::new(),
        custom_fan_modes: Vec::new(),
    }
}

struct Harness {
    engine: RuleEngine,
    snapshot: SystemSnapshot,
    now: u64,
    unix: u64,
    unix_base: u64,
}

impl Harness {
    fn new(toml: &str) -> Self {
        Self::with_snapshot(toml, ready_snapshot())
    }

    fn with_snapshot(toml: &str, snapshot: SystemSnapshot) -> Self {
        let set = AutomationRuleSet::parse(toml.as_bytes()).expect("rule set should parse");
        let mut engine = RuleEngine::new();
        engine.load(set);
        Self {
            engine,
            snapshot,
            now: T0,
            unix: BASE_UNIX,
            unix_base: BASE_UNIX,
        }
    }

    fn at(&mut self, now: u64) {
        self.now = now;
        self.unix = self.unix_base + (now - T0);
    }

    fn set_unix_base(&mut self, unix_base: u64) {
        self.unix_base = unix_base;
        self.unix = unix_base;
    }

    fn advance(&mut self, delta: u64) {
        let now = self.now + delta;
        self.at(now);
    }

    fn feed(&mut self, event: AutomationEvent) -> Vec<AutomationAction> {
        self.feed_ref(&event)
    }

    fn feed_ref(&mut self, event: &AutomationEvent) -> Vec<AutomationAction> {
        let mut actions = Vec::new();
        let context = AutomationContext {
            snapshot: &self.snapshot,
            monotonic_ms: self.now,
            unix_ms: self.unix,
        };
        AutomationEngine::on_event(&mut self.engine, event, context, &mut actions);
        actions
    }

    fn hotkey(&mut self, id: &str) -> Vec<AutomationAction> {
        let mut actions = Vec::new();
        let context = AutomationContext {
            snapshot: &self.snapshot,
            monotonic_ms: self.now,
            unix_ms: self.unix,
        };
        self.engine.on_hotkey(id, context, &mut actions);
        actions
    }

    fn deadline(&self) -> Option<u64> {
        AutomationEngine::next_deadline_monotonic_ms(&self.engine)
    }

    fn top(&self, slot: TargetSlot) -> Option<ControlChange> {
        self.engine
            .leases()
            .top(slot)
            .map(|lease| lease.change.clone())
    }
}

fn acquired(actions: &[AutomationAction]) -> Vec<ControlChange> {
    actions
        .iter()
        .filter_map(|action| match action {
            AutomationAction::Acquire(lease) => Some(lease.change.clone()),
            _ => None,
        })
        .collect()
}

fn released(actions: &[AutomationAction]) -> Vec<(ControlTarget, LeaseOwner)> {
    actions
        .iter()
        .filter_map(|action| match action {
            AutomationAction::Release { target, owner } => Some((target.clone(), *owner)),
            _ => None,
        })
        .collect()
}

fn notified(actions: &[AutomationAction]) -> Vec<OsdKind> {
    actions
        .iter()
        .filter_map(|action| match action {
            AutomationAction::Notify(kind) => Some(*kind),
            _ => None,
        })
        .collect()
}

fn parse_error(toml: &str) -> RuleError {
    AutomationRuleSet::parse(toml.as_bytes()).expect_err("rule set must be rejected")
}

fn game_rule() -> &'static str {
    r#"
schema = 1

[[rules]]
id = "game-extreme"
name = "进入游戏自动极速"
priority = 600
debounce_on_ms = 0
debounce_off_ms = 0
cooldown_ms = 0
notify = ["performanceMode"]

[rules.trigger]
on = "foreground"
apps = ["game:cs2"]
exit = "foreground_lost"

[rules.condition]
ac_connected = true

[[rules.actions]]
field = "performanceMode"
value = "extremePerformance"

[[rules.actions]]
field = "displayRefreshRate"
value = 240
"#
}

// ------------------------------------------------------------- rule parsing ---

#[test]
fn parses_the_documented_schema() {
    let toml = r#"
schema = 1

[limits]
max_rules = 128
min_timer_period_ms = 60000
ec_writes_per_10s = 4
ec_min_gap_ms = 250
battery_hysteresis_percent = 3
utc_offset_minutes = 480

[[rules]]
id = "game-extreme"
name = "进入游戏自动极速"
priority = 600
enabled = true
debounce_on_ms = 800
debounce_off_ms = 1500
cooldown_ms = 3000
hysteresis_percent = 3
notify = ["performanceMode", "displayRefreshRate"]

[rules.trigger]
on = "foreground"
apps = ["game:cs2", "game:*:valorant"]
exit = "both"

[rules.condition]
all = [
  { ac_connected = true },
  { not = { session_unlocked = false } },
]

[[rules.actions]]
field = "performanceMode"
value = "extremePerformance"

[[rules.actions]]
field = "displayRefreshRate"
value = 240

[[rules]]
id = "fan-max"

[rules.trigger]
on = "state"

[rules.condition]
always = true

[[rules.actions]]
field = "fanCurve"
channel = "cpu"
points = [
  { temperatureC = 0, dutyPercent = 40 },
  { temperatureC = 30, dutyPercent = 45 },
  { temperatureC = 45, dutyPercent = 55 },
  { temperatureC = 60, dutyPercent = 70 },
  { temperatureC = 75, dutyPercent = 90 },
  { temperatureC = 90, dutyPercent = 100 },
]
"#;
    let set = AutomationRuleSet::parse(toml.as_bytes()).expect("documented example parses");
    assert_eq!(set.limits.max_rules, 128);
    assert_eq!(set.limits.utc_offset_minutes, 480);
    assert_eq!(set.limits.battery_hysteresis_percent, 3);
    assert_eq!(set.len(), 2);

    let rule = &set.rules[0];
    assert_eq!(rule.id, "game-extreme");
    assert_eq!(rule.priority, 600);
    // two condition leaves plus the foreground app gate
    assert_eq!(rule.specificity, 3);
    assert_eq!(rule.debounce_on_ms, 800);
    assert_eq!(rule.debounce_off_ms, 1500);
    assert_eq!(rule.cooldown_ms, 3000);
    assert_eq!(rule.hysteresis_percent, 3);
    assert_eq!(
        rule.notify,
        vec![OsdKind::PerformanceMode, OsdKind::DisplayRefreshRate]
    );
    assert_eq!(rule.trigger.kind_str(), "foreground");
    assert_eq!(rule.target_mask, (1 << TargetSlot::PerformanceMode.index())
        | (1 << TargetSlot::DisplayRefreshRate.index()));
    assert_ne!(rule.owner, 0);

    let gate = rule.trigger.app_gate().expect("foreground trigger has an app gate");
    assert_eq!(gate.0.len(), 2);
    assert_eq!(gate.1, crate::world::AppGateMode::Either);

    let fan = &set.rules[1];
    assert_eq!(fan.priority, 500);
    assert_eq!(
        fan.actions,
        vec![ControlChange::SetFanCurve {
            channel: FanChannel::Cpu,
            curve: FanCurve {
                points: [
                    FanPoint { temperature_c: 0, duty_percent: 40 },
                    FanPoint { temperature_c: 30, duty_percent: 45 },
                    FanPoint { temperature_c: 45, duty_percent: 55 },
                    FanPoint { temperature_c: 60, duty_percent: 70 },
                    FanPoint { temperature_c: 75, duty_percent: 90 },
                    FanPoint { temperature_c: 90, duty_percent: 100 },
                ],
            },
        }]
    );
    assert_eq!(set.source_bytes, toml.len());
}

#[test]
fn parses_the_shipped_example() {
    let text = include_str!("../automation.example.toml");
    let set = AutomationRuleSet::parse(text.as_bytes()).expect("shipped example parses");
    assert_eq!(set.len(), 11);
    assert_eq!(set.limits.utc_offset_minutes, 480);
    assert_eq!(
        set.rules
            .iter()
            .filter(|rule| !rule.enabled)
            .map(|rule| rule.id.as_str())
            .collect::<Vec<_>>(),
        vec!["fan-quiet-curve"]
    );
    // Every trigger kind in the schema is exercised by the example.
    let mut kinds: Vec<&str> = set.rules.iter().map(|rule| rule.trigger.kind_str()).collect();
    kinds.sort_unstable();
    kinds.dedup();
    assert_eq!(
        kinds,
        vec![
            "battery", "daily", "display", "foreground", "hotkey", "idle", "interval",
            "power", "session"
        ]
    );
    // Every rule has a distinct lease owner.
    let mut owners: Vec<u32> = set.rules.iter().map(|rule| rule.owner).collect();
    owners.sort_unstable();
    owners.dedup();
    assert_eq!(owners.len(), set.len());
}

#[test]
fn rejects_oversized_and_non_utf8_configs() {
    let oversized = vec![b'#'; MAX_CONFIG_BYTES + 1];
    assert!(matches!(
        AutomationRuleSet::parse(&oversized),
        Err(RuleError::TooLarge { .. })
    ));
    assert!(matches!(
        AutomationRuleSet::parse(&[0xff, 0xfe]),
        Err(RuleError::NotUtf8)
    ));
}

#[test]
fn rejects_unknown_and_missing_keys() {
    assert!(matches!(
        parse_error("schema = 1\nfoo = 1\n"),
        RuleError::UnknownKey { key, .. } if key == "foo"
    ));
    assert!(matches!(
        parse_error("schema = 1\n"),
        RuleError::MissingKey { key, .. } if key == "rules"
    ));
    assert!(matches!(
        parse_error(
            r#"
schema = 1
[[rules]]
id = "a"
colour = 1
[rules.trigger]
on = "state"
[rules.condition]
always = true
[[rules.actions]]
field = "coolerBoost"
value = true
"#
        ),
        RuleError::UnknownKey { key, .. } if key == "colour"
    ));
    assert!(matches!(
        parse_error(
            r#"
schema = 1
[[rules]]
id = "a"
[rules.trigger]
on = "power"
extra = true
[rules.condition]
always = true
[[rules.actions]]
field = "coolerBoost"
value = true
"#
        ),
        RuleError::UnknownKey { key, .. } if key == "extra"
    ));
    assert!(matches!(
        parse_error(
            r#"
schema = 1
[[rules]]
id = "a"
[rules.trigger]
on = "state"
[rules.condition]
always = true
[[rules.actions]]
field = "coolerBoost"
value = true
unexpected = 1
"#
        ),
        RuleError::UnknownKey { key, .. } if key == "unexpected"
    ));
    assert!(matches!(
        parse_error(
            r#"
schema = 1
[[rules]]
id = "a"
[rules.trigger]
on = "state"
[rules.condition]
always = true
"#
        ),
        RuleError::MissingKey { key, .. } if key == "actions"
    ));
}

#[test]
fn rejects_schema_mismatch_duplicates_and_bad_ids() {
    assert!(matches!(
        parse_error("schema = 2\n"),
        RuleError::UnsupportedSchema(2)
    ));
    let duplicate = r#"
schema = 1
[[rules]]
id = "same"
[rules.trigger]
on = "state"
[rules.condition]
always = true
[[rules.actions]]
field = "coolerBoost"
value = true
[[rules]]
id = "same"
[rules.trigger]
on = "state"
[rules.condition]
always = true
[[rules.actions]]
field = "coolerBoost"
value = false
"#;
    assert!(matches!(
        parse_error(duplicate),
        RuleError::DuplicateRuleId { .. }
    ));
    for id in ["", "has space", "has$dollar"] {
        let toml = format!(
            "schema = 1\n[[rules]]\nid = \"{id}\"\n[rules.trigger]\non = \"state\"\n[rules.condition]\nalways = true\n[[rules.actions]]\nfield = \"coolerBoost\"\nvalue = true\n"
        );
        assert!(
            matches!(parse_error(&toml), RuleError::Invalid { .. }),
            "id {id:?} should be rejected"
        );
    }
}

#[test]
fn rejects_out_of_range_scalars_and_limits() {
    let base = |extra: &str| {
        format!(
            "schema = 1\n[[rules]]\nid = \"a\"\n{extra}[rules.trigger]\non = \"state\"\n[rules.condition]\nalways = true\n[[rules.actions]]\nfield = \"coolerBoost\"\nvalue = true\n"
        )
    };
    for extra in [
        "priority = 99\n",
        "priority = 800\n",
        "debounce_on_ms = 10001\n",
        "debounce_off_ms = 30001\n",
        "cooldown_ms = 600001\n",
        "hysteresis_percent = 21\n",
        "ttl_ms = 86400001\n",
    ] {
        assert!(
            matches!(parse_error(&base(extra)), RuleError::Invalid { .. }),
            "{extra:?} should be rejected"
        );
    }

    // The safety floor can only be tightened, never relaxed.
    for limits in [
        "[limits]\nec_writes_per_10s = 5\n",
        "[limits]\nec_min_gap_ms = 249\n",
        "[limits]\nlease_depth = 9\n",
        "[limits]\nmax_rules = 0\n",
        "[limits]\nmin_timer_period_ms = 59999\n",
    ] {
        let toml = format!(
            "schema = 1\n{limits}[[rules]]\nid = \"a\"\n[rules.trigger]\non = \"state\"\n[rules.condition]\nalways = true\n[[rules.actions]]\nfield = \"coolerBoost\"\nvalue = true\n"
        );
        assert!(
            matches!(parse_error(&toml), RuleError::Invalid { .. }),
            "{limits:?} should be rejected"
        );
    }
}

#[test]
fn rejects_timer_periods_below_one_minute() {
    let toml = r#"
schema = 1
[[rules]]
id = "a"
[rules.trigger]
on = "interval"
every_ms = 59999
[rules.condition]
always = true
[[rules.actions]]
field = "coolerBoost"
value = true
"#;
    assert!(matches!(parse_error(toml), RuleError::Invalid { .. }));
}

#[test]
fn rejects_too_many_rules() {
    let mut toml = String::from("schema = 1\n");
    for index in 0..513 {
        toml.push_str(&format!(
            "[[rules]]\nid = \"rule-{index}\"\n[rules.trigger]\non = \"state\"\n[rules.condition]\nalways = true\n[[rules.actions]]\nfield = \"coolerBoost\"\nvalue = true\n"
        ));
    }
    assert!(matches!(
        AutomationRuleSet::parse(toml.as_bytes()),
        Err(RuleError::TooManyRules { count: 513, .. })
    ));
}

#[test]
fn rejects_deep_conditions_and_leaf_explosion() {
    let mut expression = String::from("{ always = true }");
    for _ in 0..10 {
        expression = format!("{{ not = {expression} }}");
    }
    let deep = format!(
        "schema = 1\n[[rules]]\nid = \"deep\"\ncondition = {expression}\n[rules.trigger]\non = \"state\"\n[[rules.actions]]\nfield = \"coolerBoost\"\nvalue = true\n"
    );
    assert!(matches!(
        parse_error(&deep),
        RuleError::ConditionTooDeep { .. }
    ));

    let mut leaves = String::new();
    for group in 0..3 {
        if group > 0 {
            leaves.push(',');
        }
        leaves.push_str("{ any = [");
        for leaf in 0..22 {
            if leaf > 0 {
                leaves.push(',');
            }
            leaves.push_str("{ ac_connected = true }");
        }
        leaves.push_str("] }");
    }
    let wide = format!(
        "schema = 1\n[[rules]]\nid = \"wide\"\n[rules.trigger]\non = \"state\"\n[rules.condition]\nall = [{leaves}]\n[[rules.actions]]\nfield = \"coolerBoost\"\nvalue = true\n"
    );
    assert!(matches!(
        parse_error(&wide),
        RuleError::TooManyLeaves { .. }
    ));
}

#[test]
fn rejects_bracket_bombs_before_parsing() {
    let bomb = format!("schema = 1\nvalue = {}", "{".repeat(80));
    assert!(matches!(
        parse_error(&bomb),
        RuleError::SyntaxTooDeep { .. }
    ));
    // Brackets inside strings and comments must not count.
    let benign = format!(
        "schema = 1\n# {}\n[[rules]]\nid = \"a\"\nname = \"{}\"\n[rules.trigger]\non = \"state\"\n[rules.condition]\nalways = true\n[[rules.actions]]\nfield = \"coolerBoost\"\nvalue = true\n",
        "{".repeat(200),
        "{".repeat(60)
    );
    assert!(AutomationRuleSet::parse(benign.as_bytes()).is_ok());
}

#[test]
fn rejects_bad_actions() {
    let with_actions = |actions: &str| {
        format!(
            "schema = 1\n[[rules]]\nid = \"a\"\n[rules.trigger]\non = \"state\"\n[rules.condition]\nalways = true\n{actions}"
        )
    };
    // Five points instead of six.
    assert!(matches!(
        parse_error(&with_actions(
            "[[rules.actions]]\nfield = \"fanCurve\"\nchannel = \"cpu\"\npoints = [\n{temperatureC = 0, dutyPercent = 10},\n{temperatureC = 20, dutyPercent = 20},\n{temperatureC = 40, dutyPercent = 30},\n{temperatureC = 60, dutyPercent = 40},\n{temperatureC = 80, dutyPercent = 50},\n]\n"
        )),
        RuleError::Invalid { .. }
    ));
    // Non-monotonic duty.
    assert!(matches!(
        parse_error(&with_actions(
            "[[rules.actions]]\nfield = \"fanCurve\"\nchannel = \"cpu\"\npoints = [\n{temperatureC = 0, dutyPercent = 90},\n{temperatureC = 20, dutyPercent = 80},\n{temperatureC = 40, dutyPercent = 30},\n{temperatureC = 60, dutyPercent = 40},\n{temperatureC = 80, dutyPercent = 50},\n{temperatureC = 90, dutyPercent = 60},\n]\n"
        )),
        RuleError::Invalid { .. }
    ));
    // Two actions on the same target.
    assert!(matches!(
        parse_error(&with_actions(
            "[[rules.actions]]\nfield = \"coolerBoost\"\nvalue = true\n[[rules.actions]]\nfield = \"coolerBoost\"\nvalue = false\n"
        )),
        RuleError::Invalid { .. }
    ));
    // Out-of-range and unknown values.
    for action in [
        "field = \"displayBrightness\"\nvalue = 101\n",
        "field = \"chargeLimit\"\nvalue = 0\n",
        "field = \"displayRefreshRate\"\nvalue = 0\n",
        "field = \"performanceMode\"\nvalue = \"turbo\"\n",
        "field = \"colorProfile\"\nvalue = \"../factory.icm\"\n",
        "field = \"unknown\"\nvalue = 1\n",
    ] {
        assert!(
            matches!(
                parse_error(&with_actions(&format!("[[rules.actions]]\n{action}"))),
                RuleError::Invalid { .. }
            ),
            "{action:?} should be rejected"
        );
    }
}

#[test]
fn rejects_bad_conditions() {
    let with_condition = |condition: &str| {
        format!(
            "schema = 1\n[[rules]]\nid = \"a\"\ncondition = {condition}\n[rules.trigger]\non = \"state\"\n[[rules.actions]]\nfield = \"coolerBoost\"\nvalue = true\n"
        )
    };
    for condition in [
        "{ battery_percent = { op = \"lt\", value = 101 } }",
        "{ external_displays = { op = \"gt\", count = 99 } }",
        "{ refresh_rate = { op = \"eq\", hz = 0 } }",
        "{ time_window = { from = \"25:00\", to = \"07:00\" } }",
        "{ time_window = { from = \"22:00\", to = \"07:00\", days = [\"funday\"] } }",
        "{ effective_power_mode_in = [\"turbo\"] }",
        "{ mode_in = [\"turbo\"] }",
        "{ app_foreground = \"\" }",
        "{ always = false }",
        "{ unknown_leaf = true }",
        "{ ac_connected = true, power_saving = true }",
    ] {
        assert!(
            matches!(
                parse_error(&with_condition(condition)),
                RuleError::Invalid { .. }
            ),
            "{condition} should be rejected"
        );
    }
}

// ------------------------------------------------------------------- engine ---

#[test]
fn game_enter_acquires_and_exit_releases() {
    let mut harness = Harness::new(game_rule());
    assert_eq!(harness.deadline(), None, "nothing scheduled before the first event");

    let enter = harness.feed(AutomationEvent::ForegroundChanged {
        app_id: Some("game:cs2".to_owned()),
        instance: Some(77),
    });
    assert_eq!(
        acquired(&enter),
        vec![
            ControlChange::SetPerformanceMode {
                mode: PerformanceMode::ExtremePerformance,
            },
            ControlChange::SetDisplayRefreshRate { hz: 240 },
        ]
    );
    assert_eq!(notified(&enter), vec![OsdKind::PerformanceMode]);
    assert_eq!(
        harness.top(TargetSlot::PerformanceMode),
        Some(ControlChange::SetPerformanceMode {
            mode: PerformanceMode::ExtremePerformance,
        })
    );
    assert_eq!(harness.engine.leases().total(), 2);
    assert_eq!(harness.engine.stats().rules_fired, 1);

    // Re-delivering the same foreground event is idempotent: nothing new.
    let again = harness.feed(AutomationEvent::ForegroundChanged {
        app_id: Some("game:cs2".to_owned()),
        instance: Some(77),
    });
    assert!(again.is_empty(), "repeated events must not re-propose");

    let exit = harness.feed(AutomationEvent::ForegroundChanged {
        app_id: Some("shell:desktop".to_owned()),
        instance: Some(1),
    });
    let released_targets = released(&exit);
    assert_eq!(
        released_targets,
        vec![
            (
                ControlTarget::PerformanceMode {},
                LeaseOwner::Rule(harness.engine.rules()[0].owner)
            ),
            (
                ControlTarget::DisplayRefreshRate {},
                LeaseOwner::Rule(harness.engine.rules()[0].owner)
            ),
        ]
    );
    assert_eq!(harness.engine.leases().total(), 0, "exit restores the baseline");
    assert_eq!(harness.deadline(), None);
}

#[test]
fn foreground_process_exit_mode_survives_alt_tab() {
    let toml = r#"
schema = 1
[[rules]]
id = "game"
debounce_on_ms = 0
debounce_off_ms = 0
cooldown_ms = 0
[rules.trigger]
on = "foreground"
apps = ["game:cs2"]
exit = "process_exit"
[rules.condition]
always = true
[[rules.actions]]
field = "performanceMode"
value = "extremePerformance"
"#;
    let mut harness = Harness::new(toml);
    let enter = harness.feed(AutomationEvent::ForegroundChanged {
        app_id: Some("game:cs2".to_owned()),
        instance: Some(42),
    });
    assert_eq!(acquired(&enter).len(), 1);

    let alt_tab = harness.feed(AutomationEvent::ForegroundChanged {
        app_id: Some("shell:desktop".to_owned()),
        instance: Some(1),
    });
    assert!(alt_tab.is_empty(), "backgrounded game keeps its lease");
    assert_eq!(harness.engine.leases().total(), 1);

    let exit = harness.feed(AutomationEvent::ProcessExited { instance: 42 });
    assert_eq!(released(&exit).len(), 1);
    assert_eq!(harness.engine.leases().total(), 0);
}

#[test]
fn power_storm_produces_zero_ec_writes() {
    for period_ms in [500u64, 250u64] {
        let toml = r#"
schema = 1
[[rules]]
id = "ac-boost"
[rules.trigger]
on = "power"
[rules.condition]
ac_connected = true
[[rules.actions]]
field = "performanceMode"
value = "extremePerformance"
"#;
        let mut harness = Harness::new(toml);
        let mut acquisitions = 0usize;
        let mut ac = true;
        let steps = 60_000 / period_ms;
        for step in 0..steps {
            harness.at(T0 + step * period_ms);
            let event = AutomationEvent::PowerSourceChanged {
                source: Some(if ac {
                    PowerSource::Ac
                } else {
                    PowerSource::Battery
                }),
            };
            acquisitions += acquired(&harness.feed(event)).len();
            ac = !ac;
        }
        let stats = harness.engine.stats();
        assert_eq!(
            acquisitions, 0,
            "{period_ms} ms period must never pass the 800 ms debounce"
        );
        assert_eq!(stats.leases_acquired, 0);
        assert_eq!(stats.governor.permits, 0, "zero EC writes during the storm");
        assert!(
            stats.flap_freezes >= 1,
            "the flap guard must trip and stay on record"
        );
    }
}

#[test]
fn misconfigured_power_rule_is_capped_and_frozen() {
    // debounce 0 is a misconfiguration: the first flips do reach the governor,
    // which then caps the burst and freezes the channel.
    let toml = r#"
schema = 1
[[rules]]
id = "ac-boost"
debounce_on_ms = 0
debounce_off_ms = 0
cooldown_ms = 0
[rules.trigger]
on = "power"
[rules.condition]
ac_connected = true
[[rules.actions]]
field = "performanceMode"
value = "extremePerformance"
"#;
    let mut harness = Harness::new(toml);
    let mut ac = true;
    for step in 0..20u64 {
        harness.at(T0 + step * 500);
        let event = AutomationEvent::PowerSourceChanged {
            source: Some(if ac {
                PowerSource::Ac
            } else {
                PowerSource::Battery
            }),
        };
        harness.feed(event);
        ac = !ac;
    }
    let stats = harness.engine.stats();
    assert_eq!(stats.governor.permits, 2, "freeze lands on the fifth flip");
    assert_eq!(stats.leases_acquired, 2);
    assert!(stats.flap_freezes >= 1, "channel and rule are both frozen");
    assert_eq!(
        harness.deadline(),
        Some(T0 + 2_000 + 120_000),
        "the engine arms one wake-up for the freeze to expire"
    );
}

#[test]
fn priority_and_specificity_arbitrate() {
    let toml = r#"
schema = 1
[[rules]]
id = "ac-only"
priority = 500
debounce_on_ms = 0
debounce_off_ms = 0
cooldown_ms = 0
[rules.trigger]
on = "power"
[rules.condition]
ac_connected = true
[[rules.actions]]
field = "performanceMode"
value = "extremePerformance"

[[rules]]
id = "ac-and-unlocked"
priority = 500
debounce_on_ms = 0
debounce_off_ms = 0
cooldown_ms = 0
[rules.trigger]
on = "power"
[rules.condition]
all = [ { ac_connected = true }, { session_unlocked = true } ]
[[rules.actions]]
field = "performanceMode"
value = "balanced"

[[rules]]
id = "high-priority"
priority = 700
debounce_on_ms = 0
debounce_off_ms = 0
cooldown_ms = 0
[rules.trigger]
on = "power"
[rules.condition]
all = [ { ac_connected = true }, { power_saving = false } ]
[[rules.actions]]
field = "performanceMode"
value = "silent"
"#;
    let mut harness = Harness::new(toml);
    harness.feed(AutomationEvent::PowerSourceChanged {
        source: Some(PowerSource::Ac),
    });
    harness.advance(300);
    harness.feed(AutomationEvent::SessionChanged { unlocked: true });
    harness.advance(300);
    harness.feed(AutomationEvent::PowerSavingChanged { enabled: false });

    assert_eq!(harness.engine.leases().leases(TargetSlot::PerformanceMode).len(), 3);
    assert_eq!(
        harness.top(TargetSlot::PerformanceMode),
        Some(ControlChange::SetPerformanceMode {
            mode: PerformanceMode::Silent
        }),
        "priority 700 outranks specificity"
    );

    // Highest priority leaves; specificity (2 leaves) now wins over 1 leaf.
    let released_actions = harness.feed(AutomationEvent::PowerSavingChanged { enabled: true });
    assert_eq!(released(&released_actions).len(), 1);
    assert_eq!(
        harness.top(TargetSlot::PerformanceMode),
        Some(ControlChange::SetPerformanceMode {
            mode: PerformanceMode::Balanced
        })
    );

    // The least specific rule is still underneath.
    let stack = harness.engine.leases().leases(TargetSlot::PerformanceMode);
    assert_eq!(stack[0].owner, LeaseOwner::Rule(harness.engine.rules()[0].owner));
    assert_eq!(stack[1].owner, LeaseOwner::Rule(harness.engine.rules()[1].owner));
}

#[test]
fn manual_override_is_never_grabbed() {
    let mut snapshot = ready_snapshot();
    snapshot.manual_overrides = vec![ControlTarget::PerformanceMode {}];
    let mut harness = Harness::with_snapshot(game_rule(), snapshot);

    let actions = harness.feed(AutomationEvent::ForegroundChanged {
        app_id: Some("game:cs2".to_owned()),
        instance: Some(9),
    });
    assert_eq!(
        acquired(&actions),
        vec![ControlChange::SetDisplayRefreshRate { hz: 240 }],
        "only the free target is proposed"
    );
    assert!(harness.top(TargetSlot::PerformanceMode).is_none());
    assert!(harness.engine.stats().blocked_manual_override > 0);

    // The user hands control back: the next event completes the acquisition.
    harness.snapshot.manual_overrides.clear();
    harness.snapshot.revision = 2;
    let actions = harness.feed(AutomationEvent::StateChanged { revision: 2 });
    assert_eq!(
        acquired(&actions),
        vec![ControlChange::SetPerformanceMode {
            mode: PerformanceMode::ExtremePerformance
        }]
    );
    assert_eq!(
        harness.top(TargetSlot::PerformanceMode),
        Some(ControlChange::SetPerformanceMode {
            mode: PerformanceMode::ExtremePerformance
        })
    );
}

#[test]
fn battery_hysteresis_holds_until_recovered() {
    let toml = r#"
schema = 1
[limits]
battery_hysteresis_percent = 3
[[rules]]
id = "battery-saver"
debounce_on_ms = 0
debounce_off_ms = 0
cooldown_ms = 0
hysteresis_percent = 3
[rules.trigger]
on = "battery"
[rules.condition]
battery_percent = { op = "lt", value = 20 }
[[rules.actions]]
field = "performanceMode"
value = "superBattery"
"#;
    let mut harness = Harness::new(toml);
    assert!(acquired(&harness.feed(AutomationEvent::BatteryPercentChanged {
        percent: Some(25)
    }))
    .is_empty());

    let low = harness.feed(AutomationEvent::BatteryPercentChanged { percent: Some(19) });
    assert_eq!(acquired(&low).len(), 1);

    for percent in [21u8, 23u8] {
        let actions = harness.feed(AutomationEvent::BatteryPercentChanged {
            percent: Some(percent),
        });
        assert!(
            actions.is_empty(),
            "{percent}% is still inside the hysteresis band"
        );
        assert_eq!(harness.engine.leases().total(), 1);
    }

    let recovered = harness.feed(AutomationEvent::BatteryPercentChanged { percent: Some(24) });
    assert_eq!(released(&recovered).len(), 1);
    assert_eq!(harness.engine.leases().total(), 0);
    assert_eq!(harness.deadline(), None, "no timer when nothing is pending");
}

#[test]
fn debounce_boundaries_and_single_deadline() {
    let toml = r#"
schema = 1
[[rules]]
id = "battery-saver"
debounce_on_ms = 800
debounce_off_ms = 1500
cooldown_ms = 0
hysteresis_percent = 3
[rules.trigger]
on = "battery"
[rules.condition]
battery_percent = { op = "lt", value = 20 }
[[rules.actions]]
field = "performanceMode"
value = "superBattery"
"#;
    let mut harness = Harness::new(toml);
    let low = AutomationEvent::BatteryPercentChanged { percent: Some(19) };

    assert!(harness.feed_ref(&low).is_empty());
    assert_eq!(harness.deadline(), Some(T0 + 800));

    harness.advance(799);
    assert!(harness.feed_ref(&low).is_empty(), "799 ms is not enough");
    assert_eq!(harness.deadline(), Some(T0 + 800));

    harness.advance(1);
    assert_eq!(acquired(&harness.feed_ref(&low)).len(), 1);
    assert_eq!(harness.deadline(), None);

    // Release is slower: the condition must stay false for 1500 ms.
    harness.advance(200);
    let high = AutomationEvent::BatteryPercentChanged { percent: Some(40) };
    assert!(harness.feed_ref(&high).is_empty());
    assert_eq!(harness.deadline(), Some(T0 + 1_000 + 1_500));

    harness.advance(1_499);
    assert!(harness.feed_ref(&high).is_empty());
    harness.advance(1);
    assert_eq!(released(&harness.feed_ref(&high)).len(), 1);
    assert_eq!(harness.deadline(), None);
}

#[test]
fn empty_rule_set_never_schedules() {
    let mut harness = Harness::new("schema = 1\n[[rules]]\nid = \"x\"\n[rules.trigger]\non = \"state\"\n[rules.condition]\nalways = true\n[[rules.actions]]\nfield = \"coolerBoost\"\nvalue = true\n");
    assert_eq!(harness.deadline(), None);
    harness.engine.load(AutomationRuleSet::default());
    harness.feed(AutomationEvent::StateChanged { revision: 1 });
    assert_eq!(harness.deadline(), None);
    assert_eq!(harness.engine.rules().len(), 0);
}

#[test]
fn governor_enforces_budget_gaps_and_freeze() {
    let limits = RuleLimits::default();
    let mut governor = EcGovernor::new(&limits);
    assert_eq!(
        governor.request(TargetSlot::PerformanceMode, 0),
        WriteDecision::Permit
    );
    assert_eq!(
        governor.request(TargetSlot::PerformanceMode, 100),
        WriteDecision::Deferred { earliest_ms: 250 },
        "global 250 ms gap"
    );
    assert_eq!(
        governor.request(TargetSlot::PerformanceMode, 250),
        WriteDecision::Permit
    );
    assert_eq!(
        governor.request(TargetSlot::CoolerBoost, 500),
        WriteDecision::Permit
    );
    assert_eq!(
        governor.request(TargetSlot::ChargeLimit, 750),
        WriteDecision::Permit
    );
    assert_eq!(
        governor.request(TargetSlot::PerformanceMode, 1_000),
        WriteDecision::Deferred { earliest_ms: 2_500 },
        "four transactions per ten seconds, then wait for a token"
    );
    assert_eq!(
        governor.request(TargetSlot::PerformanceMode, 2_500),
        WriteDecision::Permit
    );
    assert_eq!(governor.stats().permits, 5);

    // Non-EC targets never consume budget.
    assert_eq!(
        governor.request(TargetSlot::DisplayRefreshRate, 3_251),
        WriteDecision::NotGoverned
    );

    // ChargeLimit has its own, much longer floor.
    let mut slow = EcGovernor::new(&limits);
    assert_eq!(slow.request(TargetSlot::ChargeLimit, 0), WriteDecision::Permit);
    assert_eq!(
        slow.request(TargetSlot::ChargeLimit, 4_999),
        WriteDecision::Deferred { earliest_ms: 5_000 }
    );
    assert_eq!(slow.request(TargetSlot::ChargeLimit, 5_000), WriteDecision::Permit);

    // Flap freeze blocks the channel outright.
    let mut frozen = EcGovernor::new(&limits);
    for at in [0u64, 500, 1_000, 1_500] {
        assert!(!frozen.record_power_change(at));
    }
    assert!(frozen.record_power_change(2_000));
    assert_eq!(frozen.frozen_until(), Some(122_000));
    assert_eq!(
        frozen.request(TargetSlot::PerformanceMode, 2_001),
        WriteDecision::Deferred { earliest_ms: 122_000 }
    );
    assert_eq!(frozen.stats().flap_freezes, 1);

    // The hard blacklist is mirrored as a second gate.
    assert!(!register_write_allowed(0x50));
    assert!(!register_write_allowed(0x51));
    assert!(register_write_allowed(0x2E));
}

#[test]
fn governor_forecast_is_side_effect_free() {
    let limits = RuleLimits::default();
    let mut governor = EcGovernor::new(&limits);
    assert_eq!(governor.forecast(TargetSlot::PerformanceMode, 0), WriteDecision::Permit);
    assert_eq!(governor.forecast(TargetSlot::PerformanceMode, 0), WriteDecision::Permit);
    assert_eq!(governor.stats().permits, 0);
    assert_eq!(governor.request(TargetSlot::PerformanceMode, 0), WriteDecision::Permit);
    assert_eq!(
        governor.forecast(TargetSlot::PerformanceMode, 10),
        WriteDecision::Deferred { earliest_ms: 250 }
    );
}

#[test]
fn dry_run_reports_hits_and_blocks_without_mutating_state() {
    let harness = Harness::new(game_rule());
    let event = AutomationEvent::ForegroundChanged {
        app_id: Some("game:cs2".to_owned()),
        instance: Some(5),
    };
    let report = harness.engine.dry_run(DryRunInput {
        snapshot: &harness.snapshot,
        event: Some(&event),
        monotonic_ms: T0,
        unix_ms: BASE_UNIX,
        rule_id: None,
    });
    assert_eq!(report.rules.len(), 1);
    let rule = &report.rules[0];
    assert_eq!(rule.rule_id, "game-extreme");
    assert!(rule.matched);
    assert_eq!(rule.blocked, None);
    assert_eq!(rule.would_acquire.len(), 2);
    assert_eq!(report.expected.len(), 2);
    assert!(matches!(report.expected[0], DryRunAction::Acquire { .. }));
    assert_eq!(harness.engine.leases().total(), 0, "dry_run never mutates");

    let mut blocked_snapshot = ready_snapshot();
    blocked_snapshot.manual_overrides = vec![ControlTarget::PerformanceMode {}];
    let blocked = harness.engine.dry_run(DryRunInput {
        snapshot: &blocked_snapshot,
        event: Some(&event),
        monotonic_ms: T0,
        unix_ms: BASE_UNIX,
        rule_id: Some("game-extreme"),
    });
    assert_eq!(
        blocked.rules[0].blocked,
        Some(BlockReason::ManualOverride)
    );
    assert!(blocked.rules[0].would_acquire.is_empty());

    let no_event = harness.engine.dry_run(DryRunInput {
        snapshot: &harness.snapshot,
        event: None,
        monotonic_ms: T0,
        unix_ms: BASE_UNIX,
        rule_id: None,
    });
    assert!(!no_event.rules[0].matched);
    assert_eq!(
        no_event.rules[0].blocked,
        Some(BlockReason::ConditionFalse)
    );
}

#[test]
fn reload_releases_changed_rules_and_keeps_identical_ones() {
    let mut harness = Harness::new(game_rule());
    harness.feed(AutomationEvent::ForegroundChanged {
        app_id: Some("game:cs2".to_owned()),
        instance: Some(3),
    });
    assert_eq!(harness.engine.leases().total(), 2);

    // Identical reload keeps the leases alive.
    let same = AutomationRuleSet::parse(game_rule().as_bytes()).unwrap();
    let report = harness.engine.load(same);
    assert_eq!(report.released, 0);
    assert_eq!(harness.engine.leases().total(), 2);
    let actions = harness.feed(AutomationEvent::StateChanged { revision: 1 });
    assert!(actions.is_empty());

    // Same id, different action: the stale lease is released first.
    let changed = game_rule().replace(
        "field = \"displayRefreshRate\"\nvalue = 240",
        "field = \"displayBrightness\"\nvalue = 40",
    );
    let set = AutomationRuleSet::parse(changed.as_bytes()).unwrap();
    let report = harness.engine.load(set);
    assert_eq!(report.released, 1);
    let actions = harness.feed(AutomationEvent::StateChanged { revision: 1 });
    assert_eq!(
        released(&actions),
        vec![(
            ControlTarget::DisplayRefreshRate {},
            LeaseOwner::Rule(harness.engine.rules()[0].owner)
        )]
    );
    assert_eq!(
        acquired(&actions),
        vec![ControlChange::SetDisplayBrightness { percent: 40 }]
    );
    assert_eq!(harness.engine.leases().total(), 2);
}

#[test]
fn interval_trigger_uses_one_deadline_at_a_time() {
    let toml = r#"
schema = 1
[[rules]]
id = "periodic"
debounce_on_ms = 0
debounce_off_ms = 0
cooldown_ms = 0
[rules.trigger]
on = "interval"
every_ms = 60000
[rules.condition]
always = true
[[rules.actions]]
field = "coolerBoost"
value = true
"#;
    let mut harness = Harness::new(toml);
    harness.feed(AutomationEvent::StateChanged { revision: 1 });
    assert_eq!(harness.deadline(), Some(T0 + 60_000));
    assert_eq!(harness.engine.leases().total(), 0, "latch not set yet");

    harness.advance(60_000);
    let actions = harness.feed(AutomationEvent::TimerElapsed);
    assert_eq!(
        acquired(&actions),
        vec![ControlChange::SetCoolerBoost { enabled: true }]
    );
    assert_eq!(harness.deadline(), Some(T0 + 120_000));

    harness.advance(60_000);
    assert!(harness.feed(AutomationEvent::TimerElapsed).is_empty());
    assert_eq!(harness.deadline(), Some(T0 + 180_000));
}

#[test]
fn daily_trigger_uses_the_configured_local_offset() {
    let toml = r#"
schema = 1
[limits]
utc_offset_minutes = 480
[[rules]]
id = "night"
debounce_on_ms = 0
debounce_off_ms = 0
cooldown_ms = 0
[rules.trigger]
on = "daily"
at = "22:30"
days = ["mon"]
[rules.condition]
always = true
[[rules.actions]]
field = "performanceMode"
value = "silent"
"#;
    let mut harness = Harness::new(toml);
    // 2026-01-05T00:00:00Z is a Monday; local (UTC+8) is 08:00.
    harness.set_unix_base(1_767_571_200_000);
    harness.feed(AutomationEvent::StateChanged { revision: 1 });
    assert_eq!(
        harness.deadline(),
        Some(T0 + 52_200_000),
        "22:30 local is 14:30 UTC"
    );

    harness.at(T0 + 52_200_000);
    let actions = harness.feed(AutomationEvent::TimerElapsed);
    assert_eq!(
        acquired(&actions),
        vec![ControlChange::SetPerformanceMode {
            mode: PerformanceMode::Silent
        }]
    );
    // Next Monday 22:30 local, one week later.
    assert_eq!(harness.deadline(), Some(T0 + 52_200_000 + 7 * 86_400_000));
}

#[test]
fn hotkey_toggle_commits_immediately() {
    let toml = r#"
schema = 1
[[rules]]
id = "boost"
[rules.trigger]
on = "hotkey"
id = "toggle-boost"
[rules.condition]
always = true
[[rules.actions]]
field = "coolerBoost"
value = true
"#;
    let mut harness = Harness::new(toml);
    let on = harness.hotkey("toggle-boost");
    assert_eq!(
        acquired(&on),
        vec![ControlChange::SetCoolerBoost { enabled: true }],
        "hotkeys bypass debounce"
    );
    let off = harness.hotkey("TOGGLE-BOOST");
    assert_eq!(released(&off).len(), 1);
    assert_eq!(harness.engine.leases().total(), 0);
    assert!(harness.hotkey("other").is_empty());
}

#[test]
fn action_budget_is_bounded_and_converges() {
    let toml = r#"
schema = 1
[limits]
max_actions_per_event = 2
[[rules]]
id = "multi"
debounce_on_ms = 0
debounce_off_ms = 0
cooldown_ms = 0
[rules.trigger]
on = "state"
[rules.condition]
always = true
[[rules.actions]]
field = "performanceMode"
value = "balanced"
[[rules.actions]]
field = "displayRefreshRate"
value = 240
[[rules.actions]]
field = "displayBrightness"
value = 40
[[rules.actions]]
field = "colorProfile"
value = "factory-srgb"
"#;
    let mut harness = Harness::new(toml);
    let first = harness.feed(AutomationEvent::StateChanged { revision: 1 });
    assert_eq!(acquired(&first).len(), 2);
    assert!(harness.engine.stats().actions_dropped > 0);
    assert_eq!(harness.engine.leases().total(), 2);

    let second = harness.feed(AutomationEvent::StateChanged { revision: 1 });
    assert_eq!(acquired(&second).len(), 2);
    assert_eq!(harness.engine.leases().total(), 4);

    let third = harness.feed(AutomationEvent::StateChanged { revision: 1 });
    assert!(third.is_empty(), "final state is reached, not dropped");
    assert_eq!(harness.engine.leases().total(), 4);
}

#[test]
fn session_and_display_triggers_release_on_falsify() {
    let toml = r#"
schema = 1
[[rules]]
id = "docked"
debounce_on_ms = 0
debounce_off_ms = 0
cooldown_ms = 0
[rules.trigger]
on = "display"
[rules.condition]
external_displays = { op = "ge", count = 1 }
[[rules.actions]]
field = "displayRefreshRate"
value = 240

[[rules]]
id = "locked"
debounce_on_ms = 0
debounce_off_ms = 0
cooldown_ms = 0
[rules.trigger]
on = "session"
[rules.condition]
session_unlocked = false
[[rules.actions]]
field = "displayBrightness"
value = 10
"#;
    let mut harness = Harness::new(toml);
    let docked = harness.feed(AutomationEvent::DisplayTopologyChanged {
        external_display_count: 2,
    });
    assert_eq!(
        acquired(&docked),
        vec![ControlChange::SetDisplayRefreshRate { hz: 240 }]
    );
    let undocked = harness.feed(AutomationEvent::DisplayTopologyChanged {
        external_display_count: 0,
    });
    assert_eq!(released(&undocked).len(), 1);

    assert!(harness
        .feed(AutomationEvent::SessionChanged { unlocked: true })
        .is_empty());
    let locked = harness.feed(AutomationEvent::SessionChanged { unlocked: false });
    assert_eq!(
        acquired(&locked),
        vec![ControlChange::SetDisplayBrightness { percent: 10 }]
    );
    let unlocked = harness.feed(AutomationEvent::SessionChanged { unlocked: true });
    assert_eq!(released(&unlocked).len(), 1);
}

#[test]
fn idle_trigger_requires_the_threshold() {
    let toml = r#"
schema = 1
[[rules]]
id = "idle-silent"
debounce_on_ms = 0
debounce_off_ms = 0
cooldown_ms = 0
[rules.trigger]
on = "idle"
threshold_ms = 300000
[rules.condition]
idle_at_least_ms = 300000
[[rules.actions]]
field = "performanceMode"
value = "silent"
"#;
    let mut harness = Harness::new(toml);
    assert!(acquired(&harness.feed(AutomationEvent::IdleThresholdReached {
        idle_ms: 299_999
    }))
    .is_empty());
    let fired = harness.feed(AutomationEvent::IdleThresholdReached { idle_ms: 300_000 });
    assert_eq!(acquired(&fired).len(), 1);
    let woke = harness.feed(AutomationEvent::IdleThresholdReached { idle_ms: 1_000 });
    assert_eq!(released(&woke).len(), 1);
}

#[test]
fn lease_ttl_expires_and_retries_after_cooldown() {
    let toml = r#"
schema = 1
[[rules]]
id = "ttl"
debounce_on_ms = 0
debounce_off_ms = 0
cooldown_ms = 3000
ttl_ms = 1000
[rules.trigger]
on = "state"
[rules.condition]
always = true
[[rules.actions]]
field = "performanceMode"
value = "balanced"
"#;
    let mut harness = Harness::new(toml);
    let actions = harness.feed(AutomationEvent::StateChanged { revision: 1 });
    assert_eq!(acquired(&actions).len(), 1);
    assert_eq!(harness.deadline(), Some(T0 + 1_000));

    harness.advance(1_000);
    let expired = harness.feed(AutomationEvent::TimerElapsed);
    assert_eq!(released(&expired).len(), 1);
    assert_eq!(harness.engine.leases().total(), 0);
    assert_eq!(
        harness.deadline(),
        Some(T0 + 3_000),
        "cooldown gates the re-acquire"
    );

    harness.advance(2_000);
    let again = harness.feed(AutomationEvent::TimerElapsed);
    assert_eq!(acquired(&again).len(), 1);
}

#[test]
fn failed_apply_drops_the_lease_and_cools_down() {
    let mut harness = Harness::new(game_rule());
    harness.feed(AutomationEvent::ForegroundChanged {
        app_id: Some("game:cs2".to_owned()),
        instance: Some(11),
    });
    assert_eq!(harness.engine.leases().total(), 2);

    let failed = harness.feed(AutomationEvent::ControlSettled {
        target: ControlTarget::PerformanceMode {},
        result: Err(ErrorCode::TimedOut),
    });
    assert_eq!(
        released(&failed),
        vec![(
            ControlTarget::PerformanceMode {},
            LeaseOwner::Rule(harness.engine.rules()[0].owner)
        )]
    );
    assert_eq!(harness.engine.stats().control_settle_failures, 1);
    assert!(harness.top(TargetSlot::PerformanceMode).is_none());
    assert_eq!(
        harness.deadline(),
        Some(T0 + crate::engine::SETTLE_FAILURE_COOLDOWN_MS)
    );

    harness.advance(crate::engine::SETTLE_FAILURE_COOLDOWN_MS);
    let retried = harness.feed(AutomationEvent::TimerElapsed);
    assert_eq!(
        acquired(&retried),
        vec![ControlChange::SetPerformanceMode {
            mode: PerformanceMode::ExtremePerformance
        }]
    );
}

#[test]
fn daemon_refusal_latches_until_recovery() {
    let mut harness = Harness::new(game_rule());
    harness.feed(AutomationEvent::ForegroundChanged {
        app_id: Some("game:cs2".to_owned()),
        instance: Some(21),
    });
    assert_eq!(harness.engine.leases().total(), 2);

    let refused = harness.feed(AutomationEvent::ControlSettled {
        target: ControlTarget::PerformanceMode {},
        result: Err(ErrorCode::ReadOnlyFirmware),
    });
    assert_eq!(released(&refused).len(), 1);
    // A read-only firmware will never accept the write: the rule must latch,
    // not retry.
    let retry = harness.feed(AutomationEvent::StateChanged { revision: 1 });
    assert!(acquired(&retry).is_empty());
    assert_eq!(harness.deadline(), None, "no retry is scheduled for a latch");

    let report = harness.engine.dry_run(DryRunInput {
        snapshot: &harness.snapshot,
        event: None,
        monotonic_ms: harness.now,
        unix_ms: harness.unix,
        rule_id: Some("game-extreme"),
    });
    assert_eq!(
        report.rules[0].blocked,
        Some(BlockReason::RefusedByDaemon)
    );

    // The daemon confirms recovery and asks the engine to re-arm.
    harness.engine.clear_latches();
    harness.advance(300);
    let recovered = harness.feed(AutomationEvent::StateChanged { revision: 1 });
    assert_eq!(
        acquired(&recovered),
        vec![ControlChange::SetPerformanceMode {
            mode: PerformanceMode::ExtremePerformance
        }]
    );
}

#[test]
fn capability_change_unlatches_unsupported_rules() {
    let toml = r#"
schema = 1
[[rules]]
id = "needs-refresh"
debounce_on_ms = 0
debounce_off_ms = 0
cooldown_ms = 0
[rules.trigger]
on = "state"
[rules.condition]
always = true
[[rules.actions]]
field = "displayRefreshRate"
value = 240
"#;
    let mut snapshot = ready_snapshot();
    snapshot.capabilities = CapabilityFlags::PERFORMANCE_MODE;
    snapshot.writable = CapabilityFlags::PERFORMANCE_MODE;
    let mut harness = Harness::with_snapshot(toml, snapshot);

    let blocked = harness.feed(AutomationEvent::StateChanged { revision: 1 });
    assert!(blocked.is_empty());
    assert!(harness.engine.stats().blocked_capability > 0);
    assert_eq!(harness.deadline(), None, "no retry is scheduled for a latch");

    harness.snapshot.capabilities = caps();
    harness.snapshot.writable = caps();
    harness.snapshot.revision = 2;
    let actions = harness.feed(AutomationEvent::StateChanged { revision: 2 });
    assert_eq!(
        acquired(&actions),
        vec![ControlChange::SetDisplayRefreshRate { hz: 240 }]
    );
}

#[test]
fn release_always_precedes_acquire() {
    let toml = r#"
schema = 1
[[rules]]
id = "a"
priority = 600
debounce_on_ms = 0
debounce_off_ms = 0
cooldown_ms = 0
[rules.trigger]
on = "power"
[rules.condition]
ac_connected = true
[[rules.actions]]
field = "performanceMode"
value = "extremePerformance"

[[rules]]
id = "b"
priority = 500
debounce_on_ms = 0
debounce_off_ms = 0
cooldown_ms = 0
[rules.trigger]
on = "power"
[rules.condition]
ac_connected = false
[[rules.actions]]
field = "performanceMode"
value = "superBattery"
"#;
    let mut harness = Harness::new(toml);
    harness.feed(AutomationEvent::PowerSourceChanged {
        source: Some(PowerSource::Ac),
    });
    harness.advance(300);
    let switch = harness.feed(AutomationEvent::PowerSourceChanged {
        source: Some(PowerSource::Battery),
    });
    assert!(matches!(switch[0], AutomationAction::Release { .. }));
    assert!(matches!(switch[1], AutomationAction::Acquire(_)));
    assert_eq!(
        harness.top(TargetSlot::PerformanceMode),
        Some(ControlChange::SetPerformanceMode {
            mode: PerformanceMode::SuperBattery
        })
    );
}

// -------------------------------------------------------- primitive coverage ---

#[test]
fn lease_stack_ranks_and_evicts_deterministically() {
    let mut stack = LeaseStack::new(2);
    let lease = |owner: u32, priority: u16, specificity: u16, hz: u16| crate::lease::Lease {
        owner: LeaseOwner::Rule(owner),
        change: ControlChange::SetDisplayRefreshRate { hz },
        priority,
        specificity,
        seq: 0,
        expires_at_ms: None,
    };
    assert_eq!(
        stack.acquire(TargetSlot::DisplayRefreshRate, lease(1, 500, 1, 60)),
        crate::lease::AcquireOutcome::Added
    );
    assert_eq!(
        stack.acquire(TargetSlot::DisplayRefreshRate, lease(2, 500, 2, 240)),
        crate::lease::AcquireOutcome::Added
    );
    // Full: a weaker lease is rejected, the stack is untouched.
    assert_eq!(
        stack.acquire(TargetSlot::DisplayRefreshRate, lease(3, 400, 9, 60)),
        crate::lease::AcquireOutcome::Rejected
    );
    assert_eq!(stack.top(TargetSlot::DisplayRefreshRate).unwrap().owner, LeaseOwner::Rule(2));
    // A stronger lease evicts the weakest.
    assert_eq!(
        stack.acquire(TargetSlot::DisplayRefreshRate, lease(4, 700, 1, 60)),
        crate::lease::AcquireOutcome::Replaced {
            evicted: LeaseOwner::Rule(1)
        }
    );
    assert_eq!(stack.pressure(), 2);
    // Same owner updates in place instead of growing the stack.
    assert_eq!(
        stack.acquire(TargetSlot::DisplayRefreshRate, lease(2, 500, 2, 60)),
        crate::lease::AcquireOutcome::Updated
    );
    assert_eq!(stack.leases(TargetSlot::DisplayRefreshRate).len(), 2);
    assert_eq!(
        stack.release(TargetSlot::DisplayRefreshRate, LeaseOwner::Rule(4)).map(|lease| lease.change),
        Some(ControlChange::SetDisplayRefreshRate { hz: 60 })
    );
    assert!(stack.expiry_deadline().is_none());
}

#[test]
fn debounce_primitive_is_asymmetric() {
    let mut state = DebounceState::default();
    assert_eq!(
        debounce(false, false, &mut state, 800, 1_500, 0),
        DebounceStep::Steady
    );
    assert_eq!(
        debounce(false, true, &mut state, 800, 1_500, 0),
        DebounceStep::Pending { at_ms: 800 }
    );
    assert_eq!(state.deadline(800, 1_500), Some(800));
    assert_eq!(
        debounce(false, true, &mut state, 800, 1_500, 800),
        DebounceStep::Commit { value: true }
    );
    assert_eq!(state.deadline(800, 1_500), None);
    assert_eq!(
        debounce(true, false, &mut state, 800, 1_500, 1_000),
        DebounceStep::Pending { at_ms: 2_500 }
    );
    // A blip back to true cancels the pending release.
    assert_eq!(
        debounce(true, true, &mut state, 800, 1_500, 1_100),
        DebounceStep::Steady
    );
    assert_eq!(state.deadline(800, 1_500), None);
}

#[test]
fn cooldown_and_flap_guard_are_self_clearing() {
    let mut cooldown = Cooldown::default();
    assert!(!cooldown.active(0));
    cooldown.arm(0, 3_000);
    assert_eq!(cooldown.until(), Some(3_000));
    assert!(cooldown.active(2_999));
    assert!(!cooldown.active(3_000));
    assert_eq!(cooldown.until(), None);

    let mut flap = FlapGuard::new(60_000, 5, 120_000);
    assert!(!flap.record(0));
    assert!(!flap.record(1_000));
    assert!(!flap.record(2_000));
    assert!(!flap.record(3_000));
    assert!(flap.record(4_000));
    assert!(flap.is_frozen(5_000));
    assert!(!flap.record(5_000), "records are ignored while frozen");
    assert_eq!(flap.freezes(), 1);
    assert!(!flap.is_frozen(124_000));
    // Old samples fell out of the window, so the guard re-arms cleanly.
    assert!(!flap.record(130_000));
}

#[test]
fn battery_threshold_hysteresis_math() {
    use crate::world::{EvalMode, battery_threshold_satisfied};
    assert!(battery_threshold_satisfied(CmpOp::Lt, 20, 19, EvalMode::Arm, 3));
    assert!(!battery_threshold_satisfied(CmpOp::Lt, 20, 20, EvalMode::Arm, 3));
    assert!(battery_threshold_satisfied(CmpOp::Lt, 20, 23, EvalMode::Release, 3));
    assert!(!battery_threshold_satisfied(CmpOp::Lt, 20, 24, EvalMode::Release, 3));
    assert!(battery_threshold_satisfied(CmpOp::Gt, 20, 21, EvalMode::Arm, 3));
    assert!(battery_threshold_satisfied(CmpOp::Gt, 20, 17, EvalMode::Release, 3));
    assert!(!battery_threshold_satisfied(CmpOp::Gt, 20, 16, EvalMode::Release, 3));
    assert!(battery_threshold_satisfied(CmpOp::Le, 20, 23, EvalMode::Release, 3));
    assert!(battery_threshold_satisfied(CmpOp::Ge, 20, 17, EvalMode::Release, 3));
    assert!(!battery_threshold_satisfied(CmpOp::Eq, 20, 21, EvalMode::Release, 3));
}

#[test]
fn time_window_handles_midnight_wrap() {
    let window = TimeWindow {
        from: HhMm::new(22, 0).unwrap(),
        to: HhMm::new(7, 0).unwrap(),
        days: DayMask::ALL,
    };
    // 2026-01-05T15:00:00Z == 23:00 local (UTC+8)
    let night = 1_767_571_200_000 + 15 * 3_600_000;
    assert!(window.contains(night, 480));
    // 2026-01-05T23:30:00Z == 07:30 local, outside.
    assert!(!window.contains(night + 8 * 3_600_000 + 30 * 60_000, 480));
    assert!(window.next_boundary(night, 480).is_some());

    let open = TimeWindow {
        from: HhMm::new(9, 0).unwrap(),
        to: HhMm::new(17, 0).unwrap(),
        days: DayMask::WEEKDAYS,
    };
    // Monday 12:00 local is inside, Saturday 12:00 local is not.
    let monday_noon = 1_767_571_200_000 + 4 * 3_600_000;
    assert!(open.contains(monday_noon, 480));
    assert!(!open.contains(monday_noon + 5 * 86_400_000, 480));
}

#[test]
fn weekday_and_daily_helpers() {
    // 2026-01-05 is a Monday.
    assert_eq!(crate::world::weekday_index(20_458), 0);
    assert_eq!(crate::world::weekday_index(20_459), 1);
    assert_eq!(crate::world::weekday_index(20_464), 6);
    assert_eq!(DayMask::parse_day("monday"), Some(0));
    assert_eq!(DayMask::parse_day("SUN"), Some(6));
    assert_eq!(DayMask::parse_day("funday"), None);
    assert_eq!(HhMm::parse("22:30"), HhMm::new(22, 30));
    assert_eq!(HhMm::parse("7:05"), HhMm::new(7, 5));
    assert_eq!(HhMm::parse("24:00"), None);
    assert_eq!(HhMm::parse("22:3"), None);

    let next = next_daily_after(1_767_571_200_000, HhMm::new(8, 0).unwrap(), DayMask::ALL, 480)
        .expect("a boundary exists");
    assert_eq!(next, 1_767_571_200_000 + 86_400_000);
}

#[test]
fn world_matching_is_case_insensitive_and_bounded() {
    let mut world = World::default();
    world.set_foreground(Some("Game:CS2"), Some(1));
    world.mark_running("Game:CS2", 1);
    assert!(world.app_gate_matches(
        &["game:cs2".to_owned()],
        crate::world::AppGateMode::Foreground
    ));
    assert!(world.app_gate_matches(
        &["game:*".to_owned()],
        crate::world::AppGateMode::Running
    ));
    assert!(!world.app_gate_matches(
        &["game:*:valorant".to_owned()],
        crate::world::AppGateMode::Either
    ));

    for instance in 0..64u64 {
        world.mark_running("bulk:app", instance);
    }
    assert_eq!(world.running.len(), crate::world::MAX_RUNNING_APPS);
    world.mark_exited(63);
    assert!(!world.running.iter().any(|(_, instance)| *instance == 63));
}
