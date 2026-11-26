//! Priority/specificity lease stack.
//!
//! A hardware field is never "commanded" by the engine: the engine proposes a
//! lease and the stack decides which value is in effect. Releasing a lease is
//! the whole recovery story - there is no reverse-apply code anywhere, because
//! the effective value is always recomputed from the surviving stack.

use msilt_core::{ControlChange, ControlTarget, FanChannel, LeaseOwner};

/// Hard cap from `AUTOMATION_SPEC` §6.1. Config may only lower it.
pub const MAX_LEASE_DEPTH: usize = 8;

/// One independently arbitrated hardware field.
///
/// `ControlTarget` has no `Ord`/`Hash` in Core v1, so the stack uses this
/// compact projection as its key. Fan channels are separate targets on purpose.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum TargetSlot {
    PerformanceMode,
    FanCurveCpu,
    FanCurveGpu,
    CoolerBoost,
    ChargeLimit,
    DisplayRefreshRate,
    DisplayBrightness,
    ColorProfile,
    KeyboardLighting,
}

impl TargetSlot {
    pub const COUNT: usize = 9;

    pub const ALL: [Self; Self::COUNT] = [
        Self::PerformanceMode,
        Self::FanCurveCpu,
        Self::FanCurveGpu,
        Self::CoolerBoost,
        Self::ChargeLimit,
        Self::DisplayRefreshRate,
        Self::DisplayBrightness,
        Self::ColorProfile,
        Self::KeyboardLighting,
    ];

    pub const fn index(self) -> usize {
        self as usize
    }

    pub fn of(target: &ControlTarget) -> Option<Self> {
        match target {
            ControlTarget::PerformanceMode {} => Some(Self::PerformanceMode),
            ControlTarget::FanCurve {
                channel: FanChannel::Cpu,
            } => Some(Self::FanCurveCpu),
            ControlTarget::FanCurve {
                channel: FanChannel::Gpu,
            } => Some(Self::FanCurveGpu),
            ControlTarget::CoolerBoost {} => Some(Self::CoolerBoost),
            ControlTarget::ChargeLimit {} => Some(Self::ChargeLimit),
            ControlTarget::DisplayRefreshRate {} => Some(Self::DisplayRefreshRate),
            ControlTarget::DisplayBrightness {} => Some(Self::DisplayBrightness),
            ControlTarget::ColorProfile {} => Some(Self::ColorProfile),
            ControlTarget::KeyboardLighting {} => Some(Self::KeyboardLighting),
            _ => None,
        }
    }

    /// Same as [`Self::of`] for a proposed change.
    pub fn of_change(change: &ControlChange) -> Option<Self> {
        Self::of(&change.target())
    }

    pub fn target(self) -> ControlTarget {
        match self {
            Self::PerformanceMode => ControlTarget::PerformanceMode {},
            Self::FanCurveCpu => ControlTarget::FanCurve {
                channel: FanChannel::Cpu,
            },
            Self::FanCurveGpu => ControlTarget::FanCurve {
                channel: FanChannel::Gpu,
            },
            Self::CoolerBoost => ControlTarget::CoolerBoost {},
            Self::ChargeLimit => ControlTarget::ChargeLimit {},
            Self::DisplayRefreshRate => ControlTarget::DisplayRefreshRate {},
            Self::DisplayBrightness => ControlTarget::DisplayBrightness {},
            Self::ColorProfile => ControlTarget::ColorProfile {},
            Self::KeyboardLighting => ControlTarget::KeyboardLighting {},
        }
    }

    /// Only these targets reach the EC and are therefore subject to the
    /// governor's token bucket. Display, colour and RGB writes are non-EC
    /// (`AUTOMATION_SPEC` §5.3).
    pub const fn is_ec(self) -> bool {
        matches!(
            self,
            Self::PerformanceMode
                | Self::FanCurveCpu
                | Self::FanCurveGpu
                | Self::CoolerBoost
                | Self::ChargeLimit
        )
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PerformanceMode => "performanceMode",
            Self::FanCurveCpu => "fanCurve.cpu",
            Self::FanCurveGpu => "fanCurve.gpu",
            Self::CoolerBoost => "coolerBoost",
            Self::ChargeLimit => "chargeLimit",
            Self::DisplayRefreshRate => "displayRefreshRate",
            Self::DisplayBrightness => "displayBrightness",
            Self::ColorProfile => "colorProfile",
            Self::KeyboardLighting => "keyboardLighting",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Lease {
    pub owner: LeaseOwner,
    pub change: ControlChange,
    pub priority: u16,
    pub specificity: u16,
    /// Acquisition order; breaks ties between equally ranked owners so the
    /// newest proposal wins, deterministically.
    pub seq: u64,
    pub expires_at_ms: Option<u64>,
}

impl Lease {
    /// `(priority, specificity, seq)`, ascending.
    const fn rank(&self) -> (u16, u16, u64) {
        (self.priority, self.specificity, self.seq)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AcquireOutcome {
    Added,
    /// Same owner re-proposed a value; value and acquisition time updated.
    Updated,
    /// Stack was full and this lease outranked the bottom entry, which was
    /// evicted and must be released by the caller.
    Replaced { evicted: LeaseOwner },
    /// Stack was full and this lease ranks at or below the bottom entry.
    Rejected,
}

#[derive(Debug, Clone, Default)]
struct SlotStack {
    leases: Vec<Lease>,
}

#[derive(Debug)]
pub struct LeaseStack {
    slots: [SlotStack; TargetSlot::COUNT],
    depth_limit: usize,
    next_seq: u64,
    pressure: u64,
}

impl Default for LeaseStack {
    fn default() -> Self {
        Self::new(MAX_LEASE_DEPTH)
    }
}

impl LeaseStack {
    pub fn new(depth_limit: usize) -> Self {
        Self {
            slots: std::array::from_fn(|_| SlotStack::default()),
            depth_limit: depth_limit.clamp(1, MAX_LEASE_DEPTH),
            next_seq: 0,
            pressure: 0,
        }
    }

    pub fn depth_limit(&self) -> usize {
        self.depth_limit
    }

    /// Number of acquisitions refused or evicted because the stack was full.
    pub fn pressure(&self) -> u64 {
        self.pressure
    }

    /// Ascending `(priority, specificity, seq)`; the last entry is in effect.
    pub fn leases(&self, slot: TargetSlot) -> &[Lease] {
        &self.slots[slot.index()].leases
    }

    pub fn top(&self, slot: TargetSlot) -> Option<&Lease> {
        self.slots[slot.index()].leases.last()
    }

    pub fn find(&self, slot: TargetSlot, owner: LeaseOwner) -> Option<&Lease> {
        self.slots[slot.index()]
            .leases
            .iter()
            .find(|lease| lease.owner == owner)
    }

    pub fn total(&self) -> usize {
        self.slots.iter().map(|slot| slot.leases.len()).sum()
    }

    pub fn acquire(&mut self, slot: TargetSlot, mut lease: Lease) -> AcquireOutcome {
        self.next_seq = self.next_seq.wrapping_add(1);
        lease.seq = self.next_seq;
        let stack = &mut self.slots[slot.index()].leases;

        if let Some(position) = stack.iter().position(|held| held.owner == lease.owner) {
            stack.remove(position);
            insert_sorted(stack, lease);
            return AcquireOutcome::Updated;
        }

        if stack.len() >= self.depth_limit {
            let bottom = stack[0].rank();
            if lease.rank() > bottom {
                let evicted = stack.remove(0).owner;
                insert_sorted(stack, lease);
                self.pressure = self.pressure.saturating_add(1);
                return AcquireOutcome::Replaced { evicted };
            }
            self.pressure = self.pressure.saturating_add(1);
            return AcquireOutcome::Rejected;
        }

        insert_sorted(stack, lease);
        AcquireOutcome::Added
    }

    pub fn release(&mut self, slot: TargetSlot, owner: LeaseOwner) -> Option<Lease> {
        let stack = &mut self.slots[slot.index()].leases;
        let position = stack.iter().position(|lease| lease.owner == owner)?;
        Some(stack.remove(position))
    }

    /// Earliest lease expiry, for the single nearest-deadline scheduler.
    pub fn expiry_deadline(&self) -> Option<u64> {
        self.slots
            .iter()
            .flat_map(|slot| slot.leases.iter())
            .filter_map(|lease| lease.expires_at_ms)
            .min()
    }

    /// Drain every lease (used when the rule set is replaced).
    pub fn drain_leases<F: FnMut(TargetSlot, Lease)>(&mut self, mut visit: F) {
        for slot in TargetSlot::ALL {
            for lease in self.slots[slot.index()].leases.drain(..) {
                visit(slot, lease);
            }
        }
    }

    pub fn first_expired(&self, now_ms: u64) -> Option<(TargetSlot, LeaseOwner)> {
        for slot in TargetSlot::ALL {
            for lease in &self.slots[slot.index()].leases {
                if lease.expires_at_ms.is_some_and(|at| at <= now_ms) {
                    return Some((slot, lease.owner));
                }
            }
        }
        None
    }
}

fn insert_sorted(stack: &mut Vec<Lease>, lease: Lease) {
    let rank = lease.rank();
    let position = stack
        .iter()
        .position(|held| held.rank() > rank)
        .unwrap_or(stack.len());
    stack.insert(position, lease);
}
