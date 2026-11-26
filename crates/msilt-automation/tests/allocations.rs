//! Verifies the engine's steady-state allocation budget from outside the crate.
//!
//! `AUTOMATION_SPEC` §8 puts the whole rule engine in a 1.2 MiB working-set
//! budget, which is only reachable if an event does not allocate. This test
//! installs a counting allocator, warms the engine up with the exact loop it is
//! about to measure, and then asserts that a further 400 events allocate
//! **nothing**.
//!
//! The exceptions, all human-paced rather than hot-path, are:
//! * `ForegroundChanged` / `ProcessStarted` clone one small app id;
//! * a `ColorProfile` or `KeyboardLighting` action clones its payload when a
//!   lease is proposed.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use msilt_automation::{
    AutomationAction, AutomationContext, AutomationEngine, AutomationEvent, AutomationRuleSet,
    RuleEngine,
};
use msilt_core::{
    CapabilityFlags, DaemonStatus, EcWriteStatus, HardwareState, PerformanceMode, PowerSource,
    SystemSnapshot,
};

struct CountingAllocator;

static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);

// SAFETY: every method forwards to `System` unchanged; the counter is the only
// added work and it is atomic.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.realloc(ptr, layout, new_size) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.alloc_zeroed(layout) }
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

const RULES: &str = r#"
schema = 1
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

[[rules]]
id = "ac-boost"
debounce_on_ms = 800
debounce_off_ms = 1500
cooldown_ms = 0
[rules.trigger]
on = "power"
[rules.condition]
ac_connected = true
[[rules.actions]]
field = "coolerBoost"
value = true
"#;

fn snapshot() -> SystemSnapshot {
    SystemSnapshot {
        revision: 1,
        status: DaemonStatus::Ready,
        ec_write_status: EcWriteStatus::Enabled,
        capabilities: CapabilityFlags::PERFORMANCE_MODE | CapabilityFlags::COOLER_BOOST,
        writable: CapabilityFlags::PERFORMANCE_MODE | CapabilityFlags::COOLER_BOOST,
        hardware: HardwareState {
            power_source: Some(PowerSource::Ac),
            battery_percent: Some(80),
            supported_performance_modes: vec![
                PerformanceMode::SuperBattery,
                PerformanceMode::Balanced,
            ],
            performance_mode: Some(PerformanceMode::Balanced),
            ..HardwareState::default()
        },
        manual_overrides: Vec::new(),
    }
}

fn drive(
    engine: &mut RuleEngine,
    snapshot: &SystemSnapshot,
    actions: &mut Vec<AutomationAction>,
    now: &mut u64,
) {
    for step in 0..200u64 {
        *now += 1_000;
        let percent = if step % 2 == 0 { 19 } else { 80 };
        actions.clear();
        let event = AutomationEvent::BatteryPercentChanged {
            percent: Some(percent),
        };
        engine.on_event(
            &event,
            AutomationContext {
                snapshot,
                monotonic_ms: *now,
                unix_ms: 1_767_225_600_000 + *now,
            },
            actions,
        );
        let source = if step % 3 == 0 {
            PowerSource::Battery
        } else {
            PowerSource::Ac
        };
        actions.clear();
        let event = AutomationEvent::PowerSourceChanged {
            source: Some(source),
        };
        engine.on_event(
            &event,
            AutomationContext {
                snapshot,
                monotonic_ms: *now,
                unix_ms: 1_767_225_600_000 + *now,
            },
            actions,
        );
    }
}

#[test]
fn steady_state_events_allocate_nothing() {
    let set = AutomationRuleSet::parse(RULES.as_bytes()).expect("rule set parses");
    let mut engine = RuleEngine::new();
    engine.load(set);
    let snapshot = snapshot();
    let mut actions: Vec<AutomationAction> = Vec::with_capacity(32);
    let mut now = 1_000_000u64;

    // Warm-up with the identical loop so every `Vec` growth is already paid for.
    drive(&mut engine, &snapshot, &mut actions, &mut now);

    ALLOCATIONS.store(0, Ordering::SeqCst);
    drive(&mut engine, &snapshot, &mut actions, &mut now);
    let allocations = ALLOCATIONS.load(Ordering::SeqCst);

    assert_eq!(
        allocations, 0,
        "the steady-state event path must not touch the heap"
    );
    assert!(engine.stats().events > 400);
}
