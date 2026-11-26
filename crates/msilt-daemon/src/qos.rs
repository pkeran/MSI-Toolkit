use std::{
    ffi::c_void,
    time::{Duration, Instant},
};

use windows::Win32::System::{
    Memory::{SETPROCESSWORKINGSETSIZEEX_FLAGS, SetProcessWorkingSetSizeEx},
    Threading::{
        GetCurrentProcess, IDLE_PRIORITY_CLASS, NORMAL_PRIORITY_CLASS,
        PROCESS_POWER_THROTTLING_CURRENT_VERSION, PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
        PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION, PROCESS_POWER_THROTTLING_STATE,
        ProcessPowerThrottling, SetPriorityClass, SetProcessInformation,
    },
};

const TRIM_COOLDOWN: Duration = Duration::from_secs(2);

#[derive(Default)]
enum IdleTrim {
    #[default]
    Busy,
    Cooling(Instant),
    Settled,
}

impl IdleTrim {
    fn update(&mut self, idle: bool, now: Instant) -> bool {
        if !idle {
            *self = Self::Busy;
        } else {
            match *self {
                Self::Busy => *self = Self::Cooling(now + TRIM_COOLDOWN),
                Self::Cooling(due) if now >= due => {
                    *self = Self::Settled;
                    return true;
                }
                _ => {}
            }
        }
        false
    }

    fn deadline(&self) -> Option<Instant> {
        match *self {
            Self::Cooling(due) => Some(due),
            _ => None,
        }
    }
}

#[derive(Default)]
pub(crate) struct Qos {
    enabled: bool,
    interactive: Option<bool>,
    idle_trim: IdleTrim,
}

impl Qos {
    pub(crate) fn new(enabled: bool) -> Self {
        Self {
            enabled,
            interactive: None,
            idle_trim: IdleTrim::default(),
        }
    }

    pub(crate) fn set_interactive(&mut self, interactive: bool) {
        if !self.enabled || self.interactive == Some(interactive) {
            return;
        }
        self.interactive = Some(interactive);
        unsafe {
            let process = GetCurrentProcess();
            let _ = SetPriorityClass(
                process,
                if interactive {
                    NORMAL_PRIORITY_CLASS
                } else {
                    IDLE_PRIORITY_CLASS
                },
            );
            let speed = PROCESS_POWER_THROTTLING_EXECUTION_SPEED;
            let update = |mask, state| {
                let policy = PROCESS_POWER_THROTTLING_STATE {
                    Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
                    ControlMask: mask,
                    StateMask: state,
                };
                SetProcessInformation(
                    process,
                    ProcessPowerThrottling,
                    (&raw const policy).cast::<c_void>(),
                    size_of_val(&policy) as u32,
                )
            };
            let mask = speed | PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION;
            if update(mask, if interactive { 0 } else { mask }).is_err() {
                let _ = update(speed, if interactive { 0 } else { speed });
            }
        }
    }

    pub(crate) fn update_idle(&mut self, idle: bool, now: Instant) {
        let idle = idle && self.interactive == Some(false);
        if self.enabled && self.idle_trim.update(idle, now) {
            // The sentinel pair releases cold pages without setting a permanent working-set limit.
            unsafe {
                let _ = SetProcessWorkingSetSizeEx(
                    GetCurrentProcess(),
                    usize::MAX,
                    usize::MAX,
                    SETPROCESSWORKINGSETSIZEEX_FLAGS(0),
                );
            }
        }
    }

    pub(crate) fn trim_deadline(&self) -> Option<Instant> {
        self.idle_trim.deadline()
    }
}

impl Drop for Qos {
    fn drop(&mut self) {
        self.set_interactive(true);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trim_fires_once_after_cooling_and_never_polls_while_idle() {
        let start = Instant::now();
        let due = start + TRIM_COOLDOWN;
        let mut trim = IdleTrim::default();
        assert!(!trim.update(true, start));
        assert_eq!(trim.deadline(), Some(due));
        assert!(!trim.update(true, due - Duration::from_millis(1)));
        assert_eq!(trim.deadline(), Some(due));
        assert!(trim.update(true, due));
        assert_eq!(trim.deadline(), None);
        assert!(!trim.update(true, due + TRIM_COOLDOWN));
        assert_eq!(trim.deadline(), None);
    }

    #[test]
    fn activity_cancels_a_due_trim_and_rearms_on_the_next_idle_edge() {
        let start = Instant::now();
        let due = start + TRIM_COOLDOWN;
        let mut trim = IdleTrim::default();
        assert!(!trim.update(true, start));
        assert!(!trim.update(false, due));
        assert_eq!(trim.deadline(), None);
        let idle_again = due + Duration::from_millis(300);
        assert!(!trim.update(true, idle_again));
        assert!(!trim.update(true, idle_again + Duration::from_secs(1)));
        assert!(trim.update(true, idle_again + TRIM_COOLDOWN));
        assert!(!trim.update(false, idle_again + TRIM_COOLDOWN));
        assert!(!trim.update(true, idle_again + TRIM_COOLDOWN));
        assert_eq!(trim.deadline(), Some(idle_again + 2 * TRIM_COOLDOWN));
    }
}
