//! The `AutomationEngine` implementation: condition evaluation, lease
//! arbitration, governor gating, diagnostics and the single nearest deadline.
//!
//! Invariants this file is built around:
//!
//! * **No I/O, no clock, no thread.** Every fact and every instant comes from
//!   the `AutomationContext`; the engine is a pure state machine.
//! * **Releases before acquires.** A rule that stops matching is un-leased in
//!   the same event that another rule takes over, so the daemon never sees a
//!   gap where the field would be re-applied from a stale plan.
//! * **Bounded output.** At most `limits.max_actions_per_event` (<= 32) actions
//!   per event, deterministic order, leftovers counted instead of panicking.
//! * **Manual override wins.** While a target sits in
//!   `SystemSnapshot.manual_overrides` the engine never proposes it.

use msilt_core::{
    AutomationAction, AutomationContext, AutomationEngine, AutomationEvent, AutomationLease,
    CapabilityFlags, ControlChange, ControlTarget, ErrorCode, FanChannel, HardwareState, LeaseOwner,
    OsdKind, SystemSnapshot,
};

use crate::governor::{
    Cooldown, DebounceState, DebounceStep, EcGovernor, FlapGuard, GovernorStats, WriteDecision,
    debounce,
};
use crate::lease::{AcquireOutcome, Lease, LeaseStack, MAX_LEASE_DEPTH, TargetSlot};
use crate::rule::{AutomationRule, AutomationRuleSet, MAX_RULES, RuleLimits, Trigger};
use crate::world::{DayMask, EvalCtx, EvalMode, HhMm, World};

/// `CONTRACTS.md` §2: the coordinator accepts at most 32 actions per event.
pub const MAX_ACTIONS_PER_EVENT: usize = 32;
pub const MAX_PENDING_RELEASES: usize = 128;
/// After a failed apply the same target is left alone for a while; retrying a
/// firmware write in a tight loop is how machines get bricked.
pub const SETTLE_FAILURE_COOLDOWN_MS: u64 = 30_000;

/// Why a rule did not produce the action the user expected. Surfaced by
/// `dry_run` and by the engine's own diagnostics counters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockReason {
    /// Rule is disabled in configuration.
    Disabled,
    /// Latched off after a capability/refusal verdict; cleared on capability change.
    Unsupported,
    /// The trigger's own pulse has not been observed yet.
    NotTriggered,
    /// The user holds a manual override on this target.
    ManualOverride,
    /// `capabilities`/`writable` does not advertise the target.
    CapabilityMissing,
    /// The value is not in the snapshot's verified candidate set.
    UnsupportedValue,
    /// The daemon refused the write; retrying the same value cannot succeed.
    RefusedByDaemon,
    /// The EC channel needs a documented recovery before any further write.
    SafetyHalt,
    Cooldown {
        until_ms: u64,
    },
    Frozen {
        until_ms: u64,
    },
    GovernorDeferred {
        earliest_ms: u64,
    },
    GovernorDenied,
    LeasePressure,
    ActionBudgetExhausted,
    ConditionFalse,
    Debouncing {
        at_ms: u64,
    },
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EngineStats {
    pub events: u64,
    pub rules_evaluated: u64,
    pub rules_fired: u64,
    pub leases_acquired: u64,
    pub leases_released: u64,
    pub lease_pressure: u64,
    pub actions_emitted: u64,
    pub actions_dropped: u64,
    pub blocked_manual_override: u64,
    pub blocked_capability: u64,
    pub blocked_unsupported_value: u64,
    pub blocked_governor: u64,
    pub blocked_cooldown: u64,
    pub blocked_frozen: u64,
    pub flap_freezes: u64,
    pub control_settle_failures: u64,
    pub config_reloads: u64,
    pub governor: GovernorStats,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoadReport {
    pub version: u64,
    pub rules: usize,
    pub enabled: usize,
    /// Leases dropped because their rule disappeared or changed meaning.
    pub released: usize,
}

#[derive(Debug, Clone, Default)]
struct RuleRuntime {
    /// Post-debounce intent.
    desired: bool,
    /// Bitmask of slots currently leased in the stack.
    held: u16,
    /// Interval/daily/hotkey pulse latch.
    latch: bool,
    disabled: Option<BlockReason>,
    debounce: DebounceState,
    cooldown: Cooldown,
    frozen_until: Option<u64>,
    deferred_until: Option<u64>,
    next_pulse: Option<u64>,
    flap: FlapGuard,
}

impl RuleRuntime {
    fn with_limits(limits: &RuleLimits) -> Self {
        Self {
            flap: FlapGuard::new(
                limits.flap_window_ms,
                limits.flap_threshold,
                limits.flap_freeze_ms,
            ),
            ..Self::default()
        }
    }
}

/// The rule engine. Owns the compiled rules, the lease stacks and the EC
/// governor; owns no handle, no thread and no timer.
#[derive(Debug)]
pub struct RuleEngine {
    rules: Vec<AutomationRule>,
    runtime: Vec<RuleRuntime>,
    limits: RuleLimits,
    world: World,
    leases: LeaseStack,
    governor: EcGovernor,
    pending_releases: Vec<(ControlTarget, LeaseOwner)>,
    /// Last value proposed per target, so a re-proposal of the same value does
    /// not spend an EC transaction.
    applied: [Option<u64>; TargetSlot::COUNT],
    caps_seen: (CapabilityFlags, CapabilityFlags),
    stats: EngineStats,
    config_version: u64,
    now_ms: u64,
    unix_ms: u64,
    next_deadline: Option<u64>,
}

impl Default for RuleEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl RuleEngine {
    pub fn new() -> Self {
        let limits = RuleLimits::default();
        Self {
            rules: Vec::new(),
            runtime: Vec::new(),
            limits,
            world: World::default(),
            leases: LeaseStack::new(usize::from(limits.lease_depth)),
            governor: EcGovernor::new(&limits),
            pending_releases: Vec::new(),
            applied: [None; TargetSlot::COUNT],
            caps_seen: (CapabilityFlags::NONE, CapabilityFlags::NONE),
            stats: EngineStats::default(),
            config_version: 0,
            now_ms: 0,
            unix_ms: 0,
            next_deadline: None,
        }
    }

    /// Replace the rule set. Leases survive for rules that kept both their id and
    /// their action; everything else is released on the next event, before any
    /// new acquisition is proposed.
    pub fn load(&mut self, set: AutomationRuleSet) -> LoadReport {
        let previous_rules = std::mem::take(&mut self.rules);
        let previous_runtime = std::mem::take(&mut self.runtime);
        self.limits = set.limits;
        self.governor.apply_limits(&self.limits);

        let mut kept: Vec<(TargetSlot, Lease)> = Vec::new();
        self.leases.drain_leases(|slot, lease| kept.push((slot, lease)));
        let mut fresh = LeaseStack::new(usize::from(self.limits.lease_depth));
        let mut released = 0usize;
        for (slot, lease) in kept {
            let keep = match lease.owner {
                LeaseOwner::Scenario(_) => true,
                LeaseOwner::Rule(owner) => set.rule_by_owner(owner).is_some_and(|rule| {
                    rule.enabled && rule.actions.iter().any(|action| *action == lease.change)
                }),
            };
            if !keep {
                self.pending_releases.push((slot.target(), lease.owner));
                released += 1;
                continue;
            }
            let (priority, specificity) = match lease.owner {
                LeaseOwner::Rule(owner) => set
                    .rule_by_owner(owner)
                    .map_or((lease.priority, lease.specificity), |rule| {
                        (rule.priority, rule.specificity)
                    }),
                LeaseOwner::Scenario(_) => (lease.priority, lease.specificity),
            };
            fresh.acquire(
                slot,
                Lease {
                    priority,
                    specificity,
                    ..lease
                },
            );
        }
        self.leases = fresh;

        let previous: Vec<(u32, RuleRuntime)> = previous_rules
            .into_iter()
            .zip(previous_runtime)
            .map(|(rule, runtime)| (rule.owner, runtime))
            .collect();

        self.rules = set.rules;
        self.rules.truncate(MAX_RULES);
        self.runtime = Vec::with_capacity(self.rules.len());
        for index in 0..self.rules.len() {
            let owner = self.rules[index].owner;
            let mut runtime = previous
                .iter()
                .find(|(previous_owner, _)| *previous_owner == owner)
                .map_or_else(|| RuleRuntime::with_limits(&self.limits), |(_, runtime)| runtime.clone());
            let mut held = 0u16;
            for slot in TargetSlot::ALL {
                if self.leases.find(slot, LeaseOwner::Rule(owner)).is_some() {
                    held |= 1 << slot.index();
                }
            }
            runtime.held = held;
            runtime.deferred_until = None;
            runtime.disabled = None;
            if !self.rules[index].enabled {
                runtime.desired = false;
                runtime.debounce.clear();
            }
            self.runtime.push(runtime);
        }

        self.applied = [None; TargetSlot::COUNT];
        self.caps_seen = (CapabilityFlags::NONE, CapabilityFlags::NONE);
        // Re-read the read-back hardware facts on the next event: a reload is a
        // good moment to distrust a snapshot that may have aged.
        self.world.synced = false;
        self.config_version = self.config_version.wrapping_add(1);
        self.next_deadline = None;
        self.stats.config_reloads = self.stats.config_reloads.saturating_add(1);

        LoadReport {
            version: self.config_version,
            rules: self.rules.len(),
            enabled: self.rules.iter().filter(|rule| rule.enabled).count(),
            released,
        }
    }

    pub fn config_version(&self) -> u64 {
        self.config_version
    }

    pub fn rules(&self) -> &[AutomationRule] {
        &self.rules
    }

    pub fn limits(&self) -> &RuleLimits {
        &self.limits
    }

    pub fn world(&self) -> &World {
        &self.world
    }

    pub fn leases(&self) -> &LeaseStack {
        &self.leases
    }

    pub fn stats(&self) -> EngineStats {
        EngineStats {
            governor: self.governor.stats(),
            lease_pressure: self.leases.pressure(),
            ..self.stats
        }
    }

    /// Drop every latched `Unsupported`/`CapabilityMissing` verdict. The daemon
    /// calls this after a hardware re-inspection that may have widened the
    /// verified candidate sets (capability bits alone do not cover that).
    pub fn clear_latches(&mut self) {
        for runtime in &mut self.runtime {
            runtime.disabled = None;
        }
    }

    pub fn is_manual_override(snapshot: &SystemSnapshot, slot: TargetSlot) -> bool {
        snapshot
            .manual_overrides
            .iter()
            .any(|target| TargetSlot::of(target) == Some(slot))
    }

    /// Manual/hotkey entry point. The coordinator calls this from the hotkey
    /// message; it is not part of `AutomationEvent` yet (see the Wave 2 report).
    pub fn on_hotkey(
        &mut self,
        hotkey_id: &str,
        context: AutomationContext<'_>,
        actions: &mut Vec<AutomationAction>,
    ) {
        self.begin(context);
        let budget = self.budget();
        for index in 0..self.rules.len() {
            let matched = match &self.rules[index].trigger {
                Trigger::Hotkey { id } => id.eq_ignore_ascii_case(hotkey_id),
                _ => false,
            };
            if matched {
                self.runtime[index].latch = !self.runtime[index].latch;
                self.runtime[index].debounce.clear();
            }
        }
        self.flush_pending_releases(actions, budget);
        self.sync_capabilities(context.snapshot);
        self.sync_world(context.snapshot);
        self.advance(context.snapshot, actions, budget);
    }

    fn begin(&mut self, context: AutomationContext<'_>) {
        self.stats.events = self.stats.events.saturating_add(1);
        self.now_ms = context.monotonic_ms;
        self.unix_ms = context.unix_ms;
    }

    fn handle_event(
        &mut self,
        event: &AutomationEvent,
        context: AutomationContext<'_>,
        actions: &mut Vec<AutomationAction>,
    ) {
        self.begin(context);
        let budget = self.budget();
        self.flush_pending_releases(actions, budget);
        self.sync_capabilities(context.snapshot);
        self.sync_world(context.snapshot);
        let change = self.world.apply_event(event);
        if change.power_source_changed && self.governor.record_power_change(self.now_ms) {
            self.stats.flap_freezes = self.stats.flap_freezes.saturating_add(1);
        }
        self.handle_settled(event, actions, budget);
        if matches!(event, AutomationEvent::ClockChanged) {
            self.reset_schedules();
        }
        self.advance(context.snapshot, actions, budget);
    }

    fn advance(
        &mut self,
        snapshot: &SystemSnapshot,
        actions: &mut Vec<AutomationAction>,
        budget: usize,
    ) {
        self.expire_leases(actions, budget);
        self.evaluate(snapshot, actions, budget);
        self.next_deadline = self.compute_deadline();
    }

    fn budget(&self) -> usize {
        usize::from(
            self.limits
                .max_actions_per_event
                .clamp(1, MAX_ACTIONS_PER_EVENT as u8),
        )
    }

    fn sync_world(&mut self, snapshot: &SystemSnapshot) {
        if self.world.synced && snapshot.revision <= self.world.revision {
            return;
        }
        self.world.sync_from_snapshot(snapshot);
    }

    fn sync_capabilities(&mut self, snapshot: &SystemSnapshot) {
        let seen = (snapshot.capabilities, snapshot.writable);
        if seen == self.caps_seen {
            return;
        }
        self.caps_seen = seen;
        for runtime in &mut self.runtime {
            runtime.disabled = None;
        }
    }

    fn handle_settled(
        &mut self,
        event: &AutomationEvent,
        actions: &mut Vec<AutomationAction>,
        budget: usize,
    ) {
        let AutomationEvent::ControlSettled { target, result } = event else {
            return;
        };
        let Some(slot) = TargetSlot::of(target) else {
            return;
        };
        let Err(code) = result else {
            return;
        };
        let verdict = settle_verdict(*code);
        self.stats.control_settle_failures = self.stats.control_settle_failures.saturating_add(1);
        self.applied[slot.index()] = None;
        let now = self.now_ms;
        for index in 0..self.rules.len() {
            let owner = LeaseOwner::Rule(self.rules[index].owner);
            if self.leases.find(slot, owner).is_none() {
                continue;
            }
            match verdict {
                SettleVerdict::Retry => {
                    self.runtime[index]
                        .cooldown
                        .arm(now, SETTLE_FAILURE_COOLDOWN_MS);
                }
                SettleVerdict::Refused => {
                    self.runtime[index].disabled = Some(BlockReason::RefusedByDaemon);
                    self.runtime[index].desired = false;
                }
                SettleVerdict::Halt => {
                    self.runtime[index].disabled = Some(BlockReason::SafetyHalt);
                    self.runtime[index].desired = false;
                }
            }
        }
        // The apply failed, so no lease on this target is in effect. Drop them
        // all; the rules re-acquire after their cooldown instead of pretending
        // the value landed.
        let mut guard = 0usize;
        while let Some(owner) = self.leases.top(slot).map(|lease| lease.owner) {
            self.release_owner(owner, slot, actions, budget);
            guard += 1;
            if guard > MAX_LEASE_DEPTH {
                break;
            }
        }
    }

    fn reset_schedules(&mut self) {
        for index in 0..self.rules.len() {
            if matches!(&self.rules[index].trigger, Trigger::Daily { .. }) {
                self.runtime[index].next_pulse = None;
            }
        }
    }

    fn expire_leases(&mut self, actions: &mut Vec<AutomationAction>, budget: usize) {
        let now = self.now_ms;
        let mut guard = 0usize;
        while let Some((slot, owner)) = self.leases.first_expired(now) {
            self.release_owner(owner, slot, actions, budget);
            guard += 1;
            if guard > MAX_RULES * TargetSlot::COUNT {
                break;
            }
        }
    }

    fn flush_pending_releases(&mut self, actions: &mut Vec<AutomationAction>, budget: usize) {
        while let Some((target, owner)) = self.pending_releases.pop() {
            if actions.len() >= budget {
                self.pending_releases.push((target, owner));
                break;
            }
            actions.push(AutomationAction::Release { target, owner });
            self.stats.actions_emitted = self.stats.actions_emitted.saturating_add(1);
        }
    }

    /// Pass 1: fold every rule through its debounce and release what stopped
    /// matching. Releases always precede acquisitions in the action stream.
    fn evaluate(
        &mut self,
        snapshot: &SystemSnapshot,
        actions: &mut Vec<AutomationAction>,
        budget: usize,
    ) {
        let now = self.now_ms;
        for index in 0..self.rules.len() {
            self.stats.rules_evaluated = self.stats.rules_evaluated.saturating_add(1);
            if !self.rules[index].enabled {
                self.runtime[index].desired = false;
                self.runtime[index].debounce.clear();
                if self.runtime[index].held != 0 {
                    self.release_rule(index, actions, budget);
                }
                continue;
            }
            self.pulse(index);
            let committed = self.runtime[index].desired;
            let mode = if committed {
                EvalMode::Release
            } else {
                EvalMode::Arm
            };
            let condition = self.rule_condition(index, mode);
            let (on_ms, off_ms) = debounce_ms(&self.rules[index]);
            let step = debounce(
                committed,
                condition,
                &mut self.runtime[index].debounce,
                on_ms,
                off_ms,
                now,
            );
            if let DebounceStep::Commit { value } = step {
                self.runtime[index].desired = value;
                let froze = self.runtime[index].flap.record(now);
                if froze {
                    self.runtime[index].frozen_until = self.runtime[index].flap.frozen_until();
                    self.stats.flap_freezes = self.stats.flap_freezes.saturating_add(1);
                }
            }
            if !self.runtime[index].desired && self.runtime[index].held != 0 {
                self.release_rule(index, actions, budget);
            }
        }

        // Pass 2: acquire, highest (priority, specificity) first, owner id
        // ascending to break ties deterministically.
        let mut candidates = [0u16; MAX_RULES];
        let mut count = 0usize;
        for index in 0..self.rules.len() {
            let runtime = &self.runtime[index];
            let pending = self.rules[index].target_mask & !runtime.held;
            if runtime.desired && pending != 0 && runtime.disabled.is_none() {
                if count >= MAX_RULES {
                    break;
                }
                candidates[count] = index as u16;
                count += 1;
            }
        }
        for position in 1..count {
            let mut cursor = position;
            while cursor > 0 && self.outranks(candidates[cursor], candidates[cursor - 1]) {
                candidates.swap(cursor, cursor - 1);
                cursor -= 1;
            }
        }
        for candidate in &candidates[..count] {
            if actions.len() >= budget {
                self.stats.actions_dropped = self.stats.actions_dropped.saturating_add(1);
                break;
            }
            self.try_acquire(usize::from(*candidate), snapshot, actions, budget);
        }
    }

    fn outranks(&self, left: u16, right: u16) -> bool {
        let left = &self.rules[usize::from(left)];
        let right = &self.rules[usize::from(right)];
        match (left.priority, left.specificity).cmp(&(right.priority, right.specificity)) {
            std::cmp::Ordering::Greater => true,
            std::cmp::Ordering::Less => false,
            std::cmp::Ordering::Equal => left.owner < right.owner,
        }
    }

    fn rule_condition(&self, index: usize, mode: EvalMode) -> bool {
        let rule = &self.rules[index];
        if rule.trigger.needs_latch() && !self.runtime[index].latch {
            return false;
        }
        if let Some((apps, gate)) = rule.trigger.app_gate() {
            if !self.world.app_gate_matches(apps, gate) {
                return false;
            }
        }
        if let Trigger::Idle { threshold_ms } = &rule.trigger {
            if !self.world.idle_ms.is_some_and(|idle| idle >= *threshold_ms) {
                return false;
            }
        }
        rule.condition.eval(EvalCtx {
            world: &self.world,
            mode,
            hysteresis_percent: rule.hysteresis_percent,
            utc_offset_minutes: self.limits.utc_offset_minutes,
            unix_ms: self.unix_ms,
        })
    }

    fn pulse(&mut self, index: usize) {
        let now = self.now_ms;
        let interval = match &self.rules[index].trigger {
            Trigger::Interval { every_ms } => Some(*every_ms),
            _ => None,
        };
        if let Some(every_ms) = interval {
            match self.runtime[index].next_pulse {
                None => self.runtime[index].next_pulse = Some(now.saturating_add(every_ms)),
                Some(next) if now >= next => {
                    self.runtime[index].latch = true;
                    let mut advance = next;
                    let mut guard = 0u32;
                    while advance <= now && guard < 4_096 {
                        advance = advance.saturating_add(every_ms);
                        guard += 1;
                    }
                    self.runtime[index].next_pulse =
                        Some(if advance > now { advance } else { now + every_ms });
                }
                Some(_) => {}
            }
        }
        let daily = match &self.rules[index].trigger {
            Trigger::Daily { at, days } => Some((*at, *days)),
            _ => None,
        };
        if let Some((at, days)) = daily {
            match self.runtime[index].next_pulse {
                None => self.runtime[index].next_pulse = self.daily_deadline(at, days),
                Some(next) if now >= next => {
                    self.runtime[index].latch = true;
                    self.runtime[index].next_pulse = self.daily_deadline(at, days);
                }
                Some(_) => {}
            }
        }
    }

    fn daily_deadline(&self, at: HhMm, days: DayMask) -> Option<u64> {
        let unix = crate::world::next_daily_after(
            self.unix_ms,
            at,
            days,
            self.limits.utc_offset_minutes,
        )?;
        Some(self.mono_from_unix(unix))
    }

    fn mono_from_unix(&self, unix_ms: u64) -> u64 {
        self.now_ms
            .saturating_add(unix_ms.saturating_sub(self.unix_ms))
    }

    fn try_acquire(
        &mut self,
        index: usize,
        snapshot: &SystemSnapshot,
        actions: &mut Vec<AutomationAction>,
        budget: usize,
    ) {
        let now = self.now_ms;
        let owner = LeaseOwner::Rule(self.rules[index].owner);
        let priority = self.rules[index].priority;
        let specificity = self.rules[index].specificity;
        let cooldown_ms = self.rules[index].cooldown_ms;
        let expires_at_ms = self.rules[index]
            .ttl_ms
            .map(|ttl| now.saturating_add(ttl));

        if self.runtime[index].cooldown.active(now) {
            self.stats.blocked_cooldown = self.stats.blocked_cooldown.saturating_add(1);
            return;
        }
        if let Some(until) = self.runtime[index].frozen_until {
            if now < until {
                self.stats.blocked_frozen = self.stats.blocked_frozen.saturating_add(1);
                return;
            }
            self.runtime[index].frozen_until = None;
        }
        if let Some(until) = self.runtime[index].deferred_until {
            if now < until {
                self.stats.blocked_governor = self.stats.blocked_governor.saturating_add(1);
                return;
            }
            self.runtime[index].deferred_until = None;
        }

        let mut acquired = 0u16;
        let mut deferred: Option<u64> = None;
        let mut refused: Option<BlockReason> = None;
        let action_count = self.rules[index].actions.len();
        for action_index in 0..action_count {
            if actions.len() >= budget {
                self.stats.actions_dropped = self
                    .stats
                    .actions_dropped
                    .saturating_add((action_count - action_index) as u64);
                refused.get_or_insert(BlockReason::ActionBudgetExhausted);
                break;
            }
            let change = self.rules[index].actions[action_index].clone();
            let Some(slot) = TargetSlot::of_change(&change) else {
                continue;
            };
            if Self::is_manual_override(snapshot, slot) {
                self.stats.blocked_manual_override =
                    self.stats.blocked_manual_override.saturating_add(1);
                refused.get_or_insert(BlockReason::ManualOverride);
                continue;
            }
            let capability = change.capability();
            if !snapshot.capabilities.contains(capability)
                || !snapshot.writable.contains(capability)
            {
                self.stats.blocked_capability = self.stats.blocked_capability.saturating_add(1);
                refused.get_or_insert(BlockReason::CapabilityMissing);
                continue;
            }
            if value_supported(&change, &snapshot.hardware) == ValueSupport::Unsupported {
                self.stats.blocked_unsupported_value =
                    self.stats.blocked_unsupported_value.saturating_add(1);
                refused.get_or_insert(BlockReason::UnsupportedValue);
                continue;
            }
            if self.leases.find(slot, owner).is_some() {
                continue;
            }
            let lease = Lease {
                owner,
                change: change.clone(),
                priority,
                specificity,
                seq: 0,
                expires_at_ms,
            };
            match self.leases.acquire(slot, lease) {
                AcquireOutcome::Rejected => {
                    self.stats.lease_pressure = self.stats.lease_pressure.saturating_add(1);
                    refused.get_or_insert(BlockReason::LeasePressure);
                    continue;
                }
                AcquireOutcome::Replaced { evicted } => {
                    self.stats.lease_pressure = self.stats.lease_pressure.saturating_add(1);
                    self.release_owner(evicted, slot, actions, budget);
                }
                AcquireOutcome::Added | AcquireOutcome::Updated => {}
            }
            // Only an effective (stack-top) change costs an EC transaction. A
            // lease that lands under an existing winner is mirrored to the
            // daemon for fallback but writes nothing.
            let effective = self
                .leases
                .top(slot)
                .filter(|lease| lease.owner == owner)
                .map(|lease| change_fingerprint(&lease.change));
            if let Some(fingerprint) = effective {
                if self.applied[slot.index()] != Some(fingerprint) {
                    match self.governor.request(slot, now) {
                        WriteDecision::Permit | WriteDecision::NotGoverned => {
                            self.applied[slot.index()] = Some(fingerprint);
                        }
                        WriteDecision::Deferred { earliest_ms } => {
                            self.leases.release(slot, owner);
                            self.stats.blocked_governor =
                                self.stats.blocked_governor.saturating_add(1);
                            deferred = Some(
                                deferred.map_or(earliest_ms, |current| current.min(earliest_ms)),
                            );
                            continue;
                        }
                        WriteDecision::Denied => {
                            self.leases.release(slot, owner);
                            refused.get_or_insert(BlockReason::GovernorDenied);
                            continue;
                        }
                    }
                }
            }
            if actions.len() >= budget {
                // Evicting a lease above may have consumed the last slot of the
                // budget; never hold a lease the daemon was not told about.
                self.leases.release(slot, owner);
                self.stats.actions_dropped = self.stats.actions_dropped.saturating_add(1);
                refused.get_or_insert(BlockReason::ActionBudgetExhausted);
                break;
            }
            self.stats.leases_acquired = self.stats.leases_acquired.saturating_add(1);
            acquired |= 1 << slot.index();
            actions.push(AutomationAction::Acquire(AutomationLease {
                owner,
                change,
                priority,
                specificity,
                expires_at_monotonic_ms: expires_at_ms,
            }));
            self.stats.actions_emitted = self.stats.actions_emitted.saturating_add(1);
        }

        let runtime = &mut self.runtime[index];
        runtime.held |= acquired;
        if acquired != 0 {
            // Only a fully armed rule starts its cooldown; a partial acquisition
            // must be allowed to complete on the next event.
            if runtime.held == self.rules[index].target_mask {
                runtime.cooldown.arm(now, cooldown_ms);
            }
            self.stats.rules_fired = self.stats.rules_fired.saturating_add(1);
            for notify_index in 0..self.rules[index].notify.len() {
                let kind = self.rules[index].notify[notify_index];
                if actions.len() < budget {
                    actions.push(AutomationAction::Notify(kind));
                    self.stats.actions_emitted = self.stats.actions_emitted.saturating_add(1);
                } else {
                    self.stats.actions_dropped = self.stats.actions_dropped.saturating_add(1);
                }
            }
        }
        if let Some(until) = deferred {
            self.runtime[index].deferred_until = Some(until);
        }
        if self.runtime[index].held == 0 && deferred.is_none() {
            if let Some(reason) = refused {
                if matches!(
                    reason,
                    BlockReason::CapabilityMissing
                        | BlockReason::UnsupportedValue
                        | BlockReason::GovernorDenied
                ) {                    // Latch: do not retry until capabilities change. No log
                    // spam, no retry storm (A9).
                    self.runtime[index].disabled = Some(reason);
                    self.runtime[index].desired = false;
                }
            }
        }
    }

    fn release_rule(
        &mut self,
        index: usize,
        actions: &mut Vec<AutomationAction>,
        budget: usize,
    ) {
        let owner = LeaseOwner::Rule(self.rules[index].owner);
        for slot in TargetSlot::ALL {
            if self.leases.find(slot, owner).is_some() {
                self.release_owner(owner, slot, actions, budget);
            }
        }
        self.runtime[index].held = 0;
        self.runtime[index].deferred_until = None;
    }

    fn release_owner(
        &mut self,
        owner: LeaseOwner,
        slot: TargetSlot,
        actions: &mut Vec<AutomationAction>,
        budget: usize,
    ) {
        if self.leases.release(slot, owner).is_none() {
            return;
        }
        self.applied[slot.index()] = None;
        self.stats.leases_released = self.stats.leases_released.saturating_add(1);
        if let LeaseOwner::Rule(id) = owner {
            if let Some(index) = self.rule_index(id) {
                self.runtime[index].held &= !(1 << slot.index());
            }
        }
        self.push_release(slot.target(), owner, actions, budget);
    }

    fn push_release(
        &mut self,
        target: ControlTarget,
        owner: LeaseOwner,
        actions: &mut Vec<AutomationAction>,
        budget: usize,
    ) {
        if actions.len() < budget {
            actions.push(AutomationAction::Release { target, owner });
            self.stats.actions_emitted = self.stats.actions_emitted.saturating_add(1);
        } else if self.pending_releases.len() < MAX_PENDING_RELEASES {
            self.pending_releases.push((target, owner));
            self.stats.actions_dropped = self.stats.actions_dropped.saturating_add(1);
        } else {
            self.stats.actions_dropped = self.stats.actions_dropped.saturating_add(1);
        }
    }

    fn rule_index(&self, owner: u32) -> Option<usize> {
        self.rules.iter().position(|rule| rule.owner == owner)
    }

    /// Single nearest deadline over every timing source. `None` means "cancel
    /// the timer": a rule set with no pending debounce, schedule, freeze or
    /// lease expiry costs exactly zero wake-ups.
    fn compute_deadline(&self) -> Option<u64> {
        let now = self.now_ms;
        let mut best: Option<u64> = None;
        for (index, rule) in self.rules.iter().enumerate() {
            let runtime = &self.runtime[index];
            if !rule.enabled || runtime.disabled.is_some() {
                continue;
            }
            let (on_ms, off_ms) = debounce_ms(rule);
            consider(&mut best, runtime.debounce.deadline(on_ms, off_ms), now);
            consider(&mut best, runtime.deferred_until, now);
            consider(&mut best, runtime.frozen_until, now);
            if runtime.desired && (rule.target_mask & !runtime.held) != 0 {
                consider(&mut best, runtime.cooldown.until(), now);
            }
            consider(&mut best, runtime.next_pulse, now);
            consider(
                &mut best,
                rule.condition
                    .next_time_boundary(self.unix_ms, self.limits.utc_offset_minutes)
                    .map(|unix| self.mono_from_unix(unix)),
                now,
            );
        }
        consider(&mut best, self.leases.expiry_deadline(), now);
        consider(&mut best, self.governor.frozen_until(), now);
        best
    }

    /// Simulate an arbitrary event/snapshot without touching engine state.
    pub fn dry_run(&self, input: DryRunInput<'_>) -> DryRunReport {
        let mut world = World::default();
        world.sync_from_snapshot(input.snapshot);
        if let Some(event) = input.event {
            world.apply_event(event);
        }
        let now = input.monotonic_ms;
        let mut report = DryRunReport {
            rules: Vec::new(),
            expected: Vec::new(),
            next_deadline_monotonic_ms: self.next_deadline,
        };

        for (index, rule) in self.rules.iter().enumerate() {
            if input.rule_id.is_some_and(|filter| filter != rule.id) {
                continue;
            }
            let runtime = &self.runtime[index];
            let gated = rule
                .trigger
                .app_gate()
                .is_none_or(|(apps, mode)| world.app_gate_matches(apps, mode));
            let latched = !rule.trigger.needs_latch() || runtime.latch;
            let condition = rule.condition.eval(EvalCtx {
                world: &world,
                mode: EvalMode::Arm,
                hysteresis_percent: rule.hysteresis_percent,
                utc_offset_minutes: self.limits.utc_offset_minutes,
                unix_ms: input.unix_ms,
            });
            let matched = gated && latched && condition;
            let blocked = self.dry_run_block(rule, runtime, input.snapshot, matched, now);

            let mut would_acquire = Vec::new();
            let mut would_release = Vec::new();
            for slot in TargetSlot::ALL {
                if !rule
                    .actions
                    .iter()
                    .any(|action| TargetSlot::of_change(action) == Some(slot))
                {
                    continue;
                }
                let held = self
                    .leases
                    .find(slot, LeaseOwner::Rule(rule.owner))
                    .is_some();
                if held && (!matched || blocked.is_some()) {
                    would_release.push(slot.target());
                } else if !held && matched && blocked.is_none() {
                    would_acquire.push(slot.target());
                }
            }
            for target in &would_acquire {
                report.expected.push(DryRunAction::Acquire {
                    rule_id: rule.id.clone(),
                    target: target.clone(),
                    priority: rule.priority,
                    specificity: rule.specificity,
                });
            }
            for target in &would_release {
                report.expected.push(DryRunAction::Release {
                    rule_id: rule.id.clone(),
                    target: target.clone(),
                });
            }
            report.rules.push(DryRunRule {
                rule_id: rule.id.clone(),
                owner: rule.owner,
                trigger: rule.trigger.kind_str(),
                priority: rule.priority,
                specificity: rule.specificity,
                matched,
                blocked,
                would_acquire,
                would_release,
            });
        }
        report
    }

    fn dry_run_block(
        &self,
        rule: &AutomationRule,
        runtime: &RuleRuntime,
        snapshot: &SystemSnapshot,
        matched: bool,
        now: u64,
    ) -> Option<BlockReason> {
        if !rule.enabled {
            return Some(BlockReason::Disabled);
        }
        if let Some(reason) = runtime.disabled {
            return Some(reason);
        }
        if rule.trigger.needs_latch() && !runtime.latch {
            return Some(BlockReason::NotTriggered);
        }
        if !matched {
            return Some(BlockReason::ConditionFalse);
        }
        if let Some(until) = runtime.frozen_until {
            if now < until {
                return Some(BlockReason::Frozen { until_ms: until });
            }
        }
        if let Some(until) = runtime.cooldown.until() {
            if now < until {
                return Some(BlockReason::Cooldown { until_ms: until });
            }
        }
        if let Some(at_ms) = runtime
            .debounce
            .deadline(debounce_ms(rule).0, debounce_ms(rule).1)
        {
            return Some(BlockReason::Debouncing { at_ms });
        }
        for action in &rule.actions {
            let Some(slot) = TargetSlot::of_change(action) else {
                continue;
            };
            if Self::is_manual_override(snapshot, slot) {
                return Some(BlockReason::ManualOverride);
            }
            let capability = action.capability();
            if !snapshot.capabilities.contains(capability)
                || !snapshot.writable.contains(capability)
            {
                return Some(BlockReason::CapabilityMissing);
            }
            if value_supported(action, &snapshot.hardware) == ValueSupport::Unsupported {
                return Some(BlockReason::UnsupportedValue);
            }
            if let WriteDecision::Deferred { earliest_ms } = self.governor.forecast(slot, now) {
                return Some(BlockReason::GovernorDeferred { earliest_ms });
            }
        }
        None
    }
}

impl AutomationEngine for RuleEngine {
    fn on_event(
        &mut self,
        event: &AutomationEvent,
        context: AutomationContext<'_>,
        actions: &mut Vec<AutomationAction>,
    ) {
        self.handle_event(event, context, actions);
    }

    fn next_deadline_monotonic_ms(&self) -> Option<u64> {
        self.next_deadline
    }
}

#[derive(Debug, Clone)]
pub struct DryRunInput<'a> {
    pub snapshot: &'a SystemSnapshot,
    pub event: Option<&'a AutomationEvent>,
    pub monotonic_ms: u64,
    pub unix_ms: u64,
    pub rule_id: Option<&'a str>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DryRunReport {
    pub rules: Vec<DryRunRule>,
    pub expected: Vec<DryRunAction>,
    pub next_deadline_monotonic_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DryRunRule {
    pub rule_id: String,
    pub owner: u32,
    pub trigger: &'static str,
    pub priority: u16,
    pub specificity: u16,
    pub matched: bool,
    pub blocked: Option<BlockReason>,
    pub would_acquire: Vec<ControlTarget>,
    pub would_release: Vec<ControlTarget>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DryRunAction {
    Acquire {
        rule_id: String,
        target: ControlTarget,
        priority: u16,
        specificity: u16,
    },
    Release {
        rule_id: String,
        target: ControlTarget,
    },
    Notify {
        rule_id: String,
        kind: OsdKind,
    },
}

/// Hotkeys are an explicit user command, not a noisy signal: they commit
/// without debounce.
fn debounce_ms(rule: &AutomationRule) -> (u64, u64) {
    if matches!(&rule.trigger, Trigger::Hotkey { .. }) {
        (0, 0)
    } else {
        (rule.debounce_on_ms, rule.debounce_off_ms)
    }
}

fn consider(best: &mut Option<u64>, at: Option<u64>, now: u64) {
    let Some(at) = at else {
        return;
    };
    if at <= now {
        return;
    }
    *best = Some(match *best {
        Some(current) => current.min(at),
        None => at,
    });
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SettleVerdict {
    /// Worth retrying after a cooldown (timeout, transient I/O, verification
    /// failure that the guard already rolled back).
    Retry,
    /// The same value can never succeed (read-only firmware, denied write,
    /// validation failure). Latch the rule off.
    Refused,
    /// The channel needs a documented recovery first. Latch and wait for
    /// `clear_latches()` after the daemon confirms recovery.
    Halt,
}

/// Classify a settled-apply failure by outcome, never by string matching.
fn settle_verdict(code: ErrorCode) -> SettleVerdict {
    match code {
        ErrorCode::Unsupported
        | ErrorCode::ReadOnlyFirmware
        | ErrorCode::DeniedWrite
        | ErrorCode::WriteNotAllowed
        | ErrorCode::ValidationFailed => SettleVerdict::Refused,
        ErrorCode::RecoveryRequired | ErrorCode::RollbackFailed | ErrorCode::ConflictDetected => {
            SettleVerdict::Halt
        }
        _ => SettleVerdict::Retry,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ValueSupport {
    Supported,
    Unsupported,
    /// The snapshot has no candidate list for this field yet; do not latch a
    /// rule off just because HAL has not reported in.
    Unknown,
}

fn value_supported(change: &ControlChange, hardware: &HardwareState) -> ValueSupport {
    match change {
        ControlChange::SetPerformanceMode { mode } => {
            if hardware.supported_performance_modes.is_empty() {
                ValueSupport::Unknown
            } else if hardware.supported_performance_modes.contains(mode) {
                ValueSupport::Supported
            } else {
                ValueSupport::Unsupported
            }
        }
        ControlChange::SetFanCurve { channel, .. } => {
            let fan = match channel {
                FanChannel::Cpu => hardware.cpu_fan,
                FanChannel::Gpu => hardware.gpu_fan,
            };
            match fan.and_then(|fan| fan.limits) {
                Some(_) => ValueSupport::Supported,
                None => ValueSupport::Unknown,
            }
        }
        ControlChange::SetChargeLimit { percent } => {
            if hardware.supported_charge_limits.is_empty() {
                ValueSupport::Unknown
            } else if hardware.supported_charge_limits.contains(percent) {
                ValueSupport::Supported
            } else {
                ValueSupport::Unsupported
            }
        }
        ControlChange::SetDisplayRefreshRate { hz } => {
            if hardware.supported_refresh_rates.is_empty() {
                ValueSupport::Unknown
            } else if hardware.supported_refresh_rates.contains(hz) {
                ValueSupport::Supported
            } else {
                ValueSupport::Unsupported
            }
        }
        ControlChange::SetColorProfile { profile_id } => {
            if hardware.supported_color_profiles.is_empty() {
                ValueSupport::Unknown
            } else if hardware.supported_color_profiles.contains(profile_id) {
                ValueSupport::Supported
            } else {
                ValueSupport::Unsupported
            }
        }
        _ => ValueSupport::Unknown,
    }
}

/// Zero-allocation identity of a proposed value, used to skip a redundant EC
/// transaction when the same rule re-proposes the same target.
fn change_fingerprint(change: &ControlChange) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;

    fn feed(hash: &mut u64, byte: u8) {
        *hash ^= u64::from(byte);
        *hash = (*hash).wrapping_mul(PRIME);
    }

    let mut hash = OFFSET;
    match change {
        ControlChange::SetPerformanceMode { mode } => {
            feed(&mut hash, 1);
            for byte in mode.protocol_name().bytes() {
                feed(&mut hash, byte);
            }
        }
        ControlChange::SetFanCurve { channel, curve } => {
            feed(&mut hash, 2);
            feed(
                &mut hash,
                match channel {
                    FanChannel::Cpu => 0,
                    FanChannel::Gpu => 1,
                },
            );
            for point in curve.points {
                feed(&mut hash, point.temperature_c);
                feed(&mut hash, point.duty_percent);
            }
        }
        ControlChange::SetCoolerBoost { enabled } => {
            feed(&mut hash, 3);
            feed(&mut hash, u8::from(*enabled));
        }
        ControlChange::SetChargeLimit { percent } => {
            feed(&mut hash, 4);
            feed(&mut hash, *percent);
        }
        ControlChange::SetDisplayRefreshRate { hz } => {
            feed(&mut hash, 5);
            for byte in hz.to_le_bytes() {
                feed(&mut hash, byte);
            }
        }
        ControlChange::SetDisplayBrightness { percent } => {
            feed(&mut hash, 6);
            feed(&mut hash, *percent);
        }
        ControlChange::SetColorProfile { profile_id } => {
            feed(&mut hash, 7);
            for byte in profile_id.bytes() {
                feed(&mut hash, byte);
            }
        }
        ControlChange::SetKeyboardLighting { zones, effect, speed } => {
            feed(&mut hash, 8);
            feed(&mut hash, *effect as u8);
            feed(&mut hash, *speed as u8);
            for zone in zones {
                feed(&mut hash, zone.r);
                feed(&mut hash, zone.g);
                feed(&mut hash, zone.b);
            }
        }
        _ => feed(&mut hash, 0),
    }
    hash
}
