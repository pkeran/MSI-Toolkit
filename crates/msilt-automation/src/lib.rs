//! MSI-Toolkit automation: a deterministic, allocation-frugal rule engine.
//!
//! The crate implements [`msilt_core::AutomationEngine`] and owns three things:
//!
//! * [`rule`] - the strict `automation.toml` schema and its compiled form.
//! * [`lease`] - the priority/specificity lease stack. Releasing a lease *is*
//!   the recovery path, so no reverse-apply code exists anywhere.
//! * [`governor`] - debounce, cooldown, hysteresis, flap freeze and the EC
//!   token bucket (4 transactions / 10 s).
//!
//! It deliberately depends on `msilt-core` only: no Win32, no async runtime, no
//! thread, no system clock. Every instant arrives in
//! [`msilt_core::AutomationContext`], so the whole engine is a pure state
//! machine that can be driven from a unit test on any host.
//!
//! ```no_run
//! use msilt_automation::{AutomationRuleSet, RuleEngine};
//!
//! let set = AutomationRuleSet::parse(br#"
//! schema = 1
//! [[rules]]
//! id = "ac-extreme"
//! [rules.trigger]
//! on = "power"
//! [rules.condition]
//! ac_connected = true
//! [[rules.actions]]
//! field = "performanceMode"
//! value = "extremePerformance"
//! "#).unwrap();
//! let mut engine = RuleEngine::new();
//! engine.load(set);
//! ```

#![forbid(unsafe_code)]

pub mod engine;
pub mod governor;
pub mod lease;
pub mod rule;
pub mod world;

#[cfg(test)]
mod tests;

pub use msilt_core::{
    AutomationAction, AutomationContext, AutomationEngine, AutomationEvent, AutomationLease,
    LeaseOwner,
};

pub use engine::{
    BlockReason, DryRunAction, DryRunInput, DryRunReport, DryRunRule, EngineStats, LoadReport,
    MAX_ACTIONS_PER_EVENT, RuleEngine,
};
pub use governor::{
    DebounceState, DebounceStep, EcGovernor, FlapGuard, GovernorStats, TokenBucket, WriteDecision,
};
pub use lease::{AcquireOutcome, Lease, LeaseStack, TargetSlot};
pub use rule::{
    AutomationRule, AutomationRuleSet, Condition, ForegroundExit, RuleError, RuleLimits, Trigger,
};
pub use world::{AppGateMode, CmpOp, DayMask, EvalMode, HhMm, TimeWindow, World};
