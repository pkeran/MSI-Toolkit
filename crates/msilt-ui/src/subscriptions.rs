//! Acknowledged, full-replacement subscriptions. The first request is state-only.
use msilt_core::Subscriptions;

pub const STATE_ONLY: Subscriptions = Subscriptions { state: true, telemetry: false, osd: false };

pub fn for_page(page: i32, paused: bool) -> Subscriptions {
    Subscriptions {
        state: true,
        telemetry: matches!(page, 0 | 1) || (page == 6 && !paused),
        osd: page == 6,
    }
}

#[derive(Default)]
pub struct SubscriptionPlan {
    applied: Option<Subscriptions>,
    pending: Option<Subscriptions>,
}

impl SubscriptionPlan {
    pub fn next(&mut self, desired: Subscriptions) -> Option<Subscriptions> {
        if self.pending.is_some() { return None; }
        let next = if self.applied.is_none() { STATE_ONLY } else { desired };
        if self.applied == Some(next) { return None; }
        self.pending = Some(next);
        Some(next)
    }

    pub fn acknowledge(&mut self, topics: Subscriptions) -> bool {
        if self.pending != Some(topics) { return false; }
        self.applied = Some(topics);
        self.pending = None;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_page_has_exact_topics() {
        for page in 0..8 {
            for paused in [false, true] {
                let topics = for_page(page, paused);
                assert!(topics.state);
                assert_eq!(topics.telemetry, page == 0 || page == 1 || (page == 6 && !paused));
                assert_eq!(topics.osd, page == 6);
            }
        }
    }

    #[test]
    fn initial_subscription_is_state_only_then_tracks_active_page() {
        let mut plan = SubscriptionPlan::default();
        assert_eq!(plan.next(for_page(0, false)), Some(STATE_ONLY));
        assert_eq!(plan.next(for_page(1, false)), None);
        assert!(plan.acknowledge(STATE_ONLY));
        let active = for_page(0, false);
        assert_eq!(plan.next(active), Some(active));
        assert!(plan.acknowledge(active));
        assert_eq!(plan.next(for_page(1, false)), None);
        assert_eq!(plan.next(for_page(2, false)), Some(STATE_ONLY));
        assert!(plan.acknowledge(STATE_ONLY));
        for page in [2, 3, 4, 5, 7, 7] { assert_eq!(plan.next(for_page(page, false)), None); }
    }

    #[test]
    fn rapid_navigation_coalesces_and_pause_stops_sampling() {
        let mut plan = SubscriptionPlan::default();
        plan.next(STATE_ONLY);
        plan.acknowledge(STATE_ONLY);
        let live = for_page(6, false);
        assert_eq!(plan.next(live), Some(live));
        assert_eq!(plan.next(for_page(6, true)), None);
        assert_eq!(plan.next(for_page(7, false)), None);
        assert!(plan.acknowledge(live));
        assert_eq!(plan.next(for_page(7, false)), Some(STATE_ONLY));
        plan.acknowledge(STATE_ONLY);
        let paused = for_page(6, true);
        assert_eq!(plan.next(paused), Some(paused));
        plan.acknowledge(paused);
        assert_eq!(plan.next(paused), None);
        assert_eq!(plan.next(live), Some(live));
    }
}
