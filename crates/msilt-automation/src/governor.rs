//! Anti-oscillation and EC write budgeting.
//!
//! Everything here is integer-only and takes "now" as an argument, so a test can
//! drive years of simulated time in microseconds and the result is exact.
//!
//! Three independent safety layers keep a flapping power supply from turning
//! into EC writes:
//!
//! 1. `debounce` - a rule only commits after the condition held for `on_ms`;
//!    a 1-2 Hz plug/unplug storm never stays put long enough (0 writes).
//! 2. `FlapGuard` - if the source still flips 5 times in 60 s, the EC channel is
//!    frozen for 120 s regardless of rule configuration (0 writes).
//! 3. `EcGovernor` - token bucket (4 transactions / 10 s) plus a per-field and a
//!    global minimum gap; excess proposals are deferred, never dropped.

use msilt_core::BUILTIN_DENY_WRITE;

use crate::lease::TargetSlot;
use crate::rule::RuleLimits;

/// 1 token in micro-units. Integer accounting keeps `request` deterministic.
const MICRO: u64 = 1_000_000;

/// Transactions allowed in a burst (`AUTOMATION_SPEC` §4.4).
pub const TOKEN_CAPACITY: u8 = 4;
/// One token refilled per this period: 4 transactions per 10 s at steady state.
pub const TOKEN_REFILL_PERIOD_MS: u64 = 2_500;
/// Minimum gap between any two EC transactions, whatever the field.
pub const GLOBAL_MIN_GAP_MS: u64 = 250;
/// A fan-curve write is one logical transaction spanning several firmware
/// round-trips. Its real duration is HAL's measurement, not ours: this constant
/// only stops two curve transactions from queueing back to back. It is a
/// spacing floor, never a latency budget.
pub const FAN_CURVE_MIN_GAP_MS: u64 = 400;
/// `ChargeLimit` writes wear EC NVRAM (`AUTOMATION_SPEC` §5.2).
pub const CHARGE_LIMIT_MIN_GAP_MS: u64 = 5_000;

/// Mirrors `msilt_core::BUILTIN_DENY_WRITE` as a second, independent gate. The
/// engine never writes raw registers, so this exists so that any future
/// address-level action cannot bypass the hard blacklist.
pub fn register_write_allowed(address: u8) -> bool {
    !BUILTIN_DENY_WRITE.contains(&address)
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GovernorStats {
    pub permits: u64,
    pub deferred: u64,
    pub flap_freezes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteDecision {
    /// Target is not an EC field; no budget is consumed.
    NotGoverned,
    Permit,
    /// Recoverable: retry at `earliest_ms`. The final state is never dropped.
    Deferred { earliest_ms: u64 },
    /// Permanently refused (hard blacklist / safety refusal).
    Denied,
}

/// Credit-based bucket in micro-tokens. Exact, no float, no drift.
#[derive(Debug, Clone, Copy)]
pub struct TokenBucket {
    capacity_micro: u64,
    refill_micro_per_ms: u64,
    credit_micro: u64,
    last_ms: u64,
}

impl TokenBucket {
    pub fn new(capacity: u8, refill_period_ms: u64) -> Self {
        let capacity = u64::from(capacity.max(1));
        let period = refill_period_ms.clamp(1, MICRO);
        Self {
            capacity_micro: capacity * MICRO,
            refill_micro_per_ms: MICRO / period,
            credit_micro: capacity * MICRO,
            last_ms: 0,
        }
    }

    pub fn credit(&self, now_ms: u64) -> u64 {
        self.credit_at(now_ms)
    }

    fn credit_at(&self, now_ms: u64) -> u64 {
        if now_ms < self.last_ms {
            return self.capacity_micro;
        }
        let add = now_ms
            .saturating_sub(self.last_ms)
            .saturating_mul(self.refill_micro_per_ms);
        self.credit_micro.saturating_add(add).min(self.capacity_micro)
    }

    pub fn refill(&mut self, now_ms: u64) {
        if now_ms < self.last_ms {
            // Monotonic clock restarted: refill rather than starve.
            self.credit_micro = self.capacity_micro;
            self.last_ms = now_ms;
            return;
        }
        if now_ms == self.last_ms {
            return;
        }
        self.credit_micro = self.credit_at(now_ms);
        self.last_ms = now_ms;
    }

    pub fn take(&mut self, now_ms: u64) -> bool {
        self.refill(now_ms);
        if self.credit_micro >= MICRO {
            self.credit_micro -= MICRO;
            true
        } else {
            false
        }
    }

    /// Earliest instant one token is available.
    pub fn earliest(&self, now_ms: u64) -> u64 {
        let credit = self.credit_at(now_ms);
        if credit >= MICRO {
            return now_ms;
        }
        let missing = MICRO - credit;
        let rate = self.refill_micro_per_ms.max(1);
        now_ms.saturating_add(missing.div_ceil(rate))
    }
}

/// Sliding-window flip detector with a hard freeze.
#[derive(Debug, Clone)]
pub struct FlapGuard {
    window_ms: u64,
    threshold: u8,
    freeze_ms: u64,
    events: [u64; MAX_FLAP_SAMPLES],
    len: usize,
    frozen_until: Option<u64>,
    freezes: u64,
}

pub const MAX_FLAP_SAMPLES: usize = 8;

impl Default for FlapGuard {
    fn default() -> Self {
        Self::new(60_000, 5, 120_000)
    }
}

impl FlapGuard {
    pub fn new(window_ms: u64, threshold: u8, freeze_ms: u64) -> Self {
        Self {
            window_ms,
            threshold: threshold.clamp(2, MAX_FLAP_SAMPLES as u8),
            freeze_ms,
            events: [0; MAX_FLAP_SAMPLES],
            len: 0,
            frozen_until: None,
            freezes: 0,
        }
    }

    /// Record one flip. Returns `true` when this flip tripped the freeze.
    pub fn record(&mut self, now_ms: u64) -> bool {
        if self.frozen_until.is_some_and(|until| now_ms < until) {
            return false;
        }
        self.frozen_until = None;

        if self.len == MAX_FLAP_SAMPLES {
            self.events.copy_within(1.., 0);
            self.len -= 1;
        }
        self.events[self.len] = now_ms;
        self.len += 1;

        let horizon = now_ms.saturating_sub(self.window_ms);
        let live = self
            .events
            .get(..self.len)
            .unwrap_or_default()
            .iter()
            .filter(|at| **at >= horizon)
            .count();
        if live >= usize::from(self.threshold) {
            self.len = 0;
            self.frozen_until = Some(now_ms.saturating_add(self.freeze_ms));
            self.freezes = self.freezes.saturating_add(1);
            return true;
        }
        false
    }

    pub fn frozen_until(&self) -> Option<u64> {
        self.frozen_until
    }

    pub fn is_frozen(&self, now_ms: u64) -> bool {
        self.frozen_until.is_some_and(|until| now_ms < until)
    }

    pub fn freezes(&self) -> u64 {
        self.freezes
    }
}

/// The EC write gate. Pure bookkeeping: it holds no HAL handle and starts no
/// thread; the daemon executes whatever the engine proposes.
#[derive(Debug, Clone)]
pub struct EcGovernor {
    bucket: TokenBucket,
    global_min_gap_ms: u64,
    last_global_ms: Option<u64>,
    last_slot_ms: [Option<u64>; TargetSlot::COUNT],
    slot_gap_ms: [u64; TargetSlot::COUNT],
    flap: FlapGuard,
    frozen_until: Option<u64>,
    stats: GovernorStats,
}

impl EcGovernor {
    pub fn new(limits: &RuleLimits) -> Self {
        let base_gap = limits.ec_min_gap_ms.max(GLOBAL_MIN_GAP_MS);
        let mut slot_gap_ms = [base_gap; TargetSlot::COUNT];
        slot_gap_ms[TargetSlot::FanCurveCpu.index()] = base_gap.max(FAN_CURVE_MIN_GAP_MS);
        slot_gap_ms[TargetSlot::FanCurveGpu.index()] = base_gap.max(FAN_CURVE_MIN_GAP_MS);
        slot_gap_ms[TargetSlot::ChargeLimit.index()] = base_gap.max(CHARGE_LIMIT_MIN_GAP_MS);
        Self {
            bucket: TokenBucket::new(
                limits.ec_writes_per_10s.min(TOKEN_CAPACITY),
                TOKEN_REFILL_PERIOD_MS,
            ),
            global_min_gap_ms: base_gap,
            last_global_ms: None,
            last_slot_ms: [None; TargetSlot::COUNT],
            slot_gap_ms,
            flap: FlapGuard::new(
                limits.flap_window_ms,
                limits.flap_threshold,
                limits.flap_freeze_ms,
            ),
            frozen_until: None,
            stats: GovernorStats::default(),
        }
    }

    /// Rebuild budgets for a reloaded rule set without gifting fresh tokens:
    /// the refill clock and the freeze survive a config hot reload.
    pub fn apply_limits(&mut self, limits: &RuleLimits) {
        let base_gap = limits.ec_min_gap_ms.max(GLOBAL_MIN_GAP_MS);
        self.global_min_gap_ms = base_gap;
        for slot in TargetSlot::ALL {
            self.slot_gap_ms[slot.index()] = base_gap;
        }
        self.slot_gap_ms[TargetSlot::FanCurveCpu.index()] = base_gap.max(FAN_CURVE_MIN_GAP_MS);
        self.slot_gap_ms[TargetSlot::FanCurveGpu.index()] = base_gap.max(FAN_CURVE_MIN_GAP_MS);
        self.slot_gap_ms[TargetSlot::ChargeLimit.index()] =
            base_gap.max(CHARGE_LIMIT_MIN_GAP_MS);
        self.bucket = TokenBucket {
            capacity_micro: u64::from(limits.ec_writes_per_10s.min(TOKEN_CAPACITY).max(1)) * MICRO,
            ..self.bucket
        };
        self.flap.window_ms = limits.flap_window_ms;
        self.flap.threshold = limits.flap_threshold.clamp(2, MAX_FLAP_SAMPLES as u8);
        self.flap.freeze_ms = limits.flap_freeze_ms;
    }

    /// Feed a power-source flip. Returns `true` when it tripped the freeze.
    pub fn record_power_change(&mut self, now_ms: u64) -> bool {
        let froze = self.flap.record(now_ms);
        if froze {
            self.frozen_until = self.flap.frozen_until();
            self.stats.flap_freezes = self.stats.flap_freezes.saturating_add(1);
        }
        froze
    }

    pub fn frozen_until(&self) -> Option<u64> {
        self.frozen_until
    }

    pub fn stats(&self) -> GovernorStats {
        self.stats
    }

    /// Ask to spend one EC transaction.
    pub fn request(&mut self, slot: TargetSlot, now_ms: u64) -> WriteDecision {
        if !slot.is_ec() {
            return WriteDecision::NotGoverned;
        }
        if let Some(until) = self.frozen_until {
            if now_ms < until {
                self.stats.deferred = self.stats.deferred.saturating_add(1);
                return WriteDecision::Deferred { earliest_ms: until };
            }
            self.frozen_until = None;
        }
        self.bucket.refill(now_ms);
        if let Some(earliest_ms) = self.earliest_gap(slot, now_ms) {
            self.stats.deferred = self.stats.deferred.saturating_add(1);
            return WriteDecision::Deferred { earliest_ms };
        }
        if !self.bucket.take(now_ms) {
            self.stats.deferred = self.stats.deferred.saturating_add(1);
            return WriteDecision::Deferred {
                earliest_ms: self.bucket.earliest(now_ms),
            };
        }
        self.last_slot_ms[slot.index()] = Some(now_ms);
        self.last_global_ms = Some(now_ms);
        self.stats.permits = self.stats.permits.saturating_add(1);
        WriteDecision::Permit
    }

    /// Read-only variant for `dry_run`: never consumes a token.
    pub fn forecast(&self, slot: TargetSlot, now_ms: u64) -> WriteDecision {
        if !slot.is_ec() {
            return WriteDecision::NotGoverned;
        }
        if let Some(until) = self.frozen_until {
            if now_ms < until {
                return WriteDecision::Deferred { earliest_ms: until };
            }
        }
        if let Some(earliest_ms) = self.earliest_gap(slot, now_ms) {
            return WriteDecision::Deferred { earliest_ms };
        }
        if self.bucket.credit(now_ms) < MICRO {
            return WriteDecision::Deferred {
                earliest_ms: self.bucket.earliest(now_ms),
            };
        }
        WriteDecision::Permit
    }

    fn earliest_gap(&self, slot: TargetSlot, now_ms: u64) -> Option<u64> {
        let slot_next = self.last_slot_ms[slot.index()]
            .map(|last| last.saturating_add(self.slot_gap_ms[slot.index()]));
        let global_next = self
            .last_global_ms
            .map(|last| last.saturating_add(self.global_min_gap_ms));
        let next = match (slot_next, global_next) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        };
        next.filter(|next| now_ms < *next)
    }
}

/// A pending state flip and the instant it was first observed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PendingFlip {
    pub value: bool,
    pub since_ms: u64,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct DebounceState {
    pending: Option<PendingFlip>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DebounceStep {
    Steady,
    Pending { at_ms: u64 },
    Commit { value: bool },
}

impl DebounceState {
    pub fn deadline(&self, on_ms: u64, off_ms: u64) -> Option<u64> {
        self.pending
            .map(|pending| pending.since_ms.saturating_add(if pending.value { on_ms } else { off_ms }))
    }

    pub fn is_pending(&self) -> bool {
        self.pending.is_some()
    }

    pub fn clear(&mut self) {
        self.pending = None;
    }
}

/// Asymmetric debounce: slower to leave than to enter, so a one-frame focus
/// loss does not undo a scenario.
pub fn debounce(
    committed: bool,
    condition: bool,
    state: &mut DebounceState,
    on_ms: u64,
    off_ms: u64,
    now_ms: u64,
) -> DebounceStep {
    if condition == committed {
        state.pending = None;
        return DebounceStep::Steady;
    }
    let pending = match state.pending {
        Some(pending) if pending.value == condition => pending,
        _ => {
            let pending = PendingFlip {
                value: condition,
                since_ms: now_ms,
            };
            state.pending = Some(pending);
            pending
        }
    };
    let at_ms = pending
        .since_ms
        .saturating_add(if pending.value { on_ms } else { off_ms });
    if now_ms >= at_ms {
        state.pending = None;
        DebounceStep::Commit { value: condition }
    } else {
        DebounceStep::Pending { at_ms }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Cooldown {
    until: Option<u64>,
}

impl Cooldown {
    pub fn arm(&mut self, now_ms: u64, duration_ms: u64) {
        self.until = if duration_ms == 0 {
            None
        } else {
            Some(now_ms.saturating_add(duration_ms))
        };
    }

    /// True while cooling down; clears itself once elapsed.
    pub fn active(&mut self, now_ms: u64) -> bool {
        match self.until {
            Some(until) if now_ms < until => true,
            Some(_) => {
                self.until = None;
                false
            }
            None => false,
        }
    }

    pub fn until(&self) -> Option<u64> {
        self.until
    }

    pub fn clear(&mut self) {
        self.until = None;
    }
}
