use msilt_core::{
    AutomationLease, ControlChange, ControlTarget, FanChannel, HardwareState, LeaseOwner,
};

pub(crate) const TARGETS: [ControlTarget; 10] = [
    ControlTarget::PerformanceMode {},
    ControlTarget::CoolerBoost {},
    ControlTarget::FanCurve {
        channel: FanChannel::Cpu,
    },
    ControlTarget::FanCurve {
        channel: FanChannel::Gpu,
    },
    ControlTarget::ChargeLimit {},
    ControlTarget::DisplayRefreshRate {},
    ControlTarget::DisplayBrightness {},
    ControlTarget::ColorProfile {},
    ControlTarget::KeyboardLighting {},
    ControlTarget::GpuClockOffset {},
];
const MAX_LEASES: usize = 8;

#[derive(Default)]
struct Slot {
    baseline: Option<ControlChange>,
    manual: Option<Manual>,
    automatic: Vec<AutomationLease>,
    dirty: bool,
}

struct Manual {
    until: Option<u64>,
}

impl Slot {
    fn winner(&self) -> Option<&ControlChange> {
        // max_by_key returns the last tie; acquire moves renewed owners to the back.
        self.automatic
            .iter()
            .max_by_key(|entry| (entry.priority, entry.specificity))
            .map(|entry| &entry.change)
    }

    fn restore(&self) -> Option<&ControlChange> {
        self.winner().or(self.baseline.as_ref())
    }
}

#[derive(Default)]
pub(crate) struct LeaseArbiter {
    slots: [Slot; TARGETS.len()],
}

impl LeaseArbiter {
    fn slot(&self, target: ControlTarget) -> Option<&Slot> {
        TARGETS
            .iter()
            .position(|t| *t == target)
            .map(|i| &self.slots[i])
    }
    fn slot_mut(&mut self, target: ControlTarget) -> Option<&mut Slot> {
        TARGETS
            .iter()
            .position(|t| *t == target)
            .map(|i| &mut self.slots[i])
    }

    pub(crate) fn acquire(&mut self, lease: AutomationLease, now: u64) -> bool {
        if matches!(lease.change, ControlChange::SetGpuClockOffset { .. }) {
            return false;
        }
        if lease
            .expires_at_monotonic_ms
            .is_some_and(|until| until <= now)
        {
            return false;
        }
        let Some(slot) = self.slot_mut(lease.change.target()) else {
            return false;
        };
        if let Some(index) = slot
            .automatic
            .iter()
            .position(|old| old.owner == lease.owner)
        {
            slot.automatic.remove(index);
        } else if slot.automatic.len() >= MAX_LEASES {
            let (lowest, rank) = slot
                .automatic
                .iter()
                .enumerate()
                .map(|(index, item)| (index, (item.priority, item.specificity)))
                .min_by_key(|(_, rank)| *rank)
                .expect("nonempty lease stack");
            if (lease.priority, lease.specificity) <= rank {
                return false;
            }
            slot.automatic.remove(lowest);
        }
        slot.automatic.push(lease);
        slot.dirty = true;
        true
    }

    pub(crate) fn release(&mut self, target: ControlTarget, owner: LeaseOwner) {
        if let Some(slot) = self.slot_mut(target) {
            let len = slot.automatic.len();
            slot.automatic.retain(|lease| lease.owner != owner);
            slot.dirty |= slot.automatic.len() != len;
        }
    }

    pub(crate) fn disable_automation(&mut self) {
        for slot in &mut self.slots {
            slot.dirty |= !slot.automatic.is_empty();
            slot.automatic.clear();
        }
    }

    pub(crate) fn manual_applied(
        &mut self,
        target: ControlTarget,
        original: Option<ControlChange>,
        until: Option<u64>,
    ) {
        if let Some(slot) = self.slot_mut(target) {
            if slot.baseline.is_none() {
                slot.baseline = original;
            }
            // OC needs explicit release/reset; timed restores must not reapply it on resume.
            let until = if matches!(target, ControlTarget::GpuClockOffset {}) { None } else { until };
            slot.manual = Some(Manual { until });
        }
    }

    pub(crate) fn manual_destination(&self, target: ControlTarget) -> Option<ControlChange> {
        let slot = self.slot(target)?;
        slot.manual.as_ref()?;
        slot.restore().cloned()
    }

    pub(crate) fn has_manual(&self, target: ControlTarget) -> bool {
        self.slot(target).is_some_and(|slot| slot.manual.is_some())
    }

    pub(crate) fn release_manual(
        &mut self,
        target: ControlTarget,
        applied: Option<&ControlChange>,
    ) {
        if let Some(slot) = self.slot_mut(target) {
            slot.manual = None;
            // The automatic winner can change while the explicit release is in flight.
            slot.dirty = slot
                .restore()
                .is_some_and(|desired| Some(desired) != applied);
            if !slot.dirty && slot.automatic.is_empty() {
                slot.baseline = None;
            }
        }
    }

    pub(crate) fn expire(&mut self, now: u64, manual: bool) {
        for slot in &mut self.slots {
            let len = slot.automatic.len();
            slot.automatic.retain(|lease| {
                lease
                    .expires_at_monotonic_ms
                    .is_none_or(|until| now < until)
            });
            slot.dirty |= slot.automatic.len() != len;
            if manual
                && slot
                    .manual
                    .as_ref()
                    .and_then(|lease| lease.until)
                    .is_some_and(|until| until <= now)
            {
                slot.manual = None;
                slot.dirty = true;
            }
        }
    }

    pub(crate) fn next_deadline(&self, manual: bool) -> Option<u64> {
        self.slots
            .iter()
            .flat_map(|slot| {
                slot.automatic
                    .iter()
                    .filter_map(|lease| lease.expires_at_monotonic_ms)
                    .chain(
                        slot.manual
                            .as_ref()
                            .and_then(|lease| if manual { lease.until } else { None }),
                    )
            })
            .min()
    }

    pub(crate) fn manual_targets(&self) -> impl Iterator<Item = ControlTarget> + '_ {
        TARGETS
            .into_iter()
            .zip(&self.slots)
            .filter_map(|(target, slot)| slot.manual.is_some().then_some(target))
    }

    pub(crate) fn next_automatic(&mut self, boost_on: bool) -> Option<ControlChange> {
        for (target, slot) in TARGETS.into_iter().zip(&mut self.slots) {
            if !slot.dirty
                || slot.manual.is_some()
                || (boost_on && matches!(target, ControlTarget::FanCurve { .. }))
            {
                continue;
            }
            slot.dirty = false;
            if let Some(change) = slot.restore() {
                return Some(change.clone());
            }
        }
        None
    }

    pub(crate) fn take_locked_keyboard(&mut self) -> Option<ControlChange> {
        let slot = self.slot_mut(ControlTarget::KeyboardLighting {})?;
        let change = slot
            .winner()
            .filter(|change| crate::automation_rules::dark_keyboard(change))?
            .clone();
        if !slot.dirty || slot.manual.is_some() {
            return None;
        }
        slot.dirty = false;
        Some(change)
    }

    pub(crate) fn auto_applied(
        &mut self,
        applied: &ControlChange,
        original: Option<ControlChange>,
    ) {
        if let Some(slot) = self.slot_mut(applied.target()) {
            if slot.automatic.is_empty() && slot.baseline.as_ref() == Some(applied) {
                slot.baseline = None;
                slot.dirty = false;
            } else if slot.baseline.is_none() {
                slot.baseline = original;
            }
        }
    }

    pub(crate) fn reconcile_observation(&mut self) {
        for (target, slot) in TARGETS.into_iter().zip(&mut self.slots) {
            // Refresh/resume must never reapply an OC offset without an explicit request.
            if !matches!(target, ControlTarget::GpuClockOffset {}) {
                slot.dirty |= slot.restore().is_some();
            }
        }
    }
}

pub(crate) fn observed(state: &HardwareState, target: ControlTarget) -> Option<ControlChange> {
    Some(match target {
        ControlTarget::PerformanceMode {} => ControlChange::SetPerformanceMode {
            mode: state.performance_mode?,
        },
        ControlTarget::CoolerBoost {} => ControlChange::SetCoolerBoost {
            enabled: state.cooler_boost?,
        },
        ControlTarget::FanCurve { channel } => ControlChange::SetFanCurve {
            channel,
            curve: match channel {
                FanChannel::Cpu => state.cpu_fan?,
                FanChannel::Gpu => state.gpu_fan?,
            }
            .curve,
        },
        ControlTarget::ChargeLimit {} => ControlChange::SetChargeLimit {
            percent: state.charge_limit_percent?,
        },
        ControlTarget::DisplayRefreshRate {} => ControlChange::SetDisplayRefreshRate {
            hz: state.display_refresh_hz?,
        },
        ControlTarget::DisplayBrightness {} => ControlChange::SetDisplayBrightness {
            percent: state.display_brightness_percent?,
        },
        ControlTarget::ColorProfile {} => ControlChange::SetColorProfile {
            profile_id: state.color_profile_id.clone()?,
        },
        ControlTarget::KeyboardLighting {} => ControlChange::SetKeyboardLighting {
            zones: state.keyboard_zones?,
            effect: state.keyboard_effect.unwrap_or_default(),
            speed: state.keyboard_speed.unwrap_or_default(),
        },
        ControlTarget::GpuClockOffset {} => {
            let offset = state.gpu_clock_offset?;
            ControlChange::SetGpuClockOffset { core_mhz: offset.core_mhz, mem_mhz: offset.mem_mhz }
        }
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const TARGET: ControlTarget = ControlTarget::DisplayRefreshRate {};
    fn hz(hz: u16) -> ControlChange {
        ControlChange::SetDisplayRefreshRate { hz }
    }
    fn lease(
        owner: u32,
        priority: u16,
        specificity: u16,
        hz_: u16,
        expiry: Option<u64>,
    ) -> AutomationLease {
        AutomationLease {
            owner: LeaseOwner::Rule(owner),
            priority,
            specificity,
            change: hz(hz_),
            expires_at_monotonic_ms: expiry,
        }
    }

    #[test]
    fn manual_priority_and_automatic_stack_restore_the_first_baseline() {
        let mut arbiter = LeaseArbiter::default();
        arbiter.acquire(lease(1, 500, 1, 120, None), 0);
        assert_eq!(arbiter.next_automatic(false), Some(hz(120)));
        arbiter.auto_applied(&hz(120), Some(hz(60)));
        arbiter.manual_applied(TARGET, Some(hz(120)), Some(100));
        arbiter.acquire(lease(2, 500, 2, 240, Some(200)), 30);
        assert_eq!(arbiter.next_automatic(false), None);
        assert_eq!(arbiter.next_deadline(true), Some(100));
        assert_eq!(arbiter.manual_destination(TARGET), Some(hz(240)));
        arbiter.expire(100, true);
        assert_eq!(arbiter.next_automatic(false), Some(hz(240)));
        arbiter.auto_applied(&hz(240), Some(hz(144)));
        arbiter.expire(200, true);
        assert_eq!(arbiter.next_automatic(false), Some(hz(120)));
        arbiter.release(TARGET, LeaseOwner::Rule(1));
        assert_eq!(arbiter.next_automatic(false), Some(hz(60)));
        arbiter.auto_applied(&hz(60), Some(hz(120)));
        assert_eq!(arbiter.slot(TARGET).unwrap().baseline, None);
    }

    #[test]
    fn bounded_stack_keeps_the_best_and_breaks_ties_by_latest_acquire() {
        let mut arbiter = LeaseArbiter::default();
        for owner in 0..8 {
            assert!(arbiter.acquire(lease(owner, 500, 1, 100 + owner as u16, None), 0));
        }
        assert!(!arbiter.acquire(lease(10, 499, 2, 240, None), 0));
        assert!(arbiter.acquire(lease(0, 500, 1, 144, None), 0));
        assert_eq!(arbiter.next_automatic(false), Some(hz(144)));
        assert!(arbiter.acquire(lease(10, 500, 2, 240, None), 0));
        assert_eq!(arbiter.next_automatic(false), Some(hz(240)));
    }

    #[test]
    fn gpu_manual_target_is_observed_and_refresh_does_not_reapply_oc() {
        let target = ControlTarget::GpuClockOffset {};
        let reset = ControlChange::SetGpuClockOffset { core_mhz: 0, mem_mhz: 0 };
        let oc = ControlChange::SetGpuClockOffset { core_mhz: 100, mem_mhz: 200 };
        let state = HardwareState {
            gpu_clock_offset: Some(msilt_core::GpuClockOffsetState {
                core_mhz: 100, mem_mhz: 200,
                core_min_mhz: 0, core_max_mhz: 200, mem_min_mhz: 0, mem_max_mhz: 200,
            }),
            ..Default::default()
        };
        assert_eq!(observed(&state, target), Some(oc.clone()));
        assert_eq!(observed(&HardwareState::default(), target), None);
        let mut arbiter = LeaseArbiter::default();
        arbiter.manual_applied(target, Some(reset.clone()), Some(100));
        assert_eq!(arbiter.manual_targets().collect::<Vec<_>>(), [target]);
        assert_eq!(arbiter.manual_destination(target), Some(reset.clone()));
        arbiter.expire(100, true);
        assert!(arbiter.has_manual(target));
        assert_eq!(arbiter.next_deadline(true), None);
        assert!(!arbiter.acquire(AutomationLease {
            owner: LeaseOwner::Rule(1), change: oc.clone(), priority: 100, specificity: 1,
            expires_at_monotonic_ms: None,
        }, 100));
        arbiter.reconcile_observation();
        assert_eq!(arbiter.next_automatic(false), None);
        arbiter.release_manual(target, Some(&reset));
        assert!(!arbiter.has_manual(target));
        arbiter.auto_applied(&reset, Some(oc));
        arbiter.reconcile_observation();
        assert_eq!(arbiter.next_automatic(false), None);
    }
}
