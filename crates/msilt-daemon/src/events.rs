use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU32, Ordering},
    mpsc::{Receiver, SyncSender, sync_channel},
};

use msilt_core::{ControlChange, EffectivePowerMode, ErrorCode, PowerSource, SystemSnapshot};

use crate::iocp::{Port, WAKE_STOP, WAKE_WINDOW};

pub(crate) const BITS_DISPLAY: u32 = 1;
pub(crate) const BITS_CLOCK: u32 = 2;
pub(crate) const BITS_TRAY_RESTART: u32 = 4;
pub(crate) const BITS_POWER_STATUS: u32 = 8;
const LOCKED: u32 = 1;
const SUSPENDED: u32 = 2;
const STOPPED: u32 = 4;

#[derive(Debug)]
pub(crate) enum UserEvent {
    Menu {
        x: i32,
        y: i32,
    },
    OpenUi,
    Control {
        expected: u64,
        change: ControlChange,
    },
}

pub(crate) enum UiEvent {
    Menu {
        x: i32,
        y: i32,
        snapshot: Arc<SystemSnapshot>,
    },
    OpenUi,
    ShowOsd {
        title: &'static str,
        detail: String,
        duration_ms: u16,
        done: Option<SyncSender<Result<(), ErrorCode>>>,
    },
    HideOsd,
    RestoreTray,
}

#[derive(Default)]
pub(crate) struct WindowEvents {
    pub(crate) flags: u32,
    pub(crate) source: Option<Option<PowerSource>>,
    pub(crate) battery: Option<Option<u8>>,
    pub(crate) saver: Option<bool>,
    pub(crate) effective: Option<Option<EffectivePowerMode>>,
    pub(crate) display_on: Option<bool>,
    pub(crate) unlocked: Option<bool>,
    pub(crate) suspended: Option<bool>,
}

pub(crate) struct Inbox {
    port: Port,
    queued: AtomicBool,
    flags: AtomicU32,
    source: AtomicU32,
    battery: AtomicU32,
    saver: AtomicU32,
    effective: AtomicU32,
    display: AtomicU32,
    unlocked: AtomicU32,
    suspended: AtomicU32,
    work_blocks: AtomicU32,
    actions: SyncSender<UserEvent>,
}

impl Inbox {
    pub(crate) fn new(port: &Port) -> (Arc<Self>, Receiver<UserEvent>) {
        let (actions, receiver) = sync_channel(16);
        (
            Arc::new(Self {
                port: port.clone(),
                queued: AtomicBool::new(false),
                flags: AtomicU32::new(0),
                source: AtomicU32::new(0),
                battery: AtomicU32::new(0),
                saver: AtomicU32::new(0),
                effective: AtomicU32::new(0),
                display: AtomicU32::new(0),
                unlocked: AtomicU32::new(0),
                suspended: AtomicU32::new(0),
                work_blocks: AtomicU32::new(0),
                actions,
            }),
            receiver,
        )
    }

    fn signal(&self) {
        if !self.queued.swap(true, Ordering::AcqRel) {
            let _ = self.port.post(WAKE_WINDOW);
        }
    }

    pub(crate) fn stop(&self) {
        if self.work_blocks.fetch_or(STOPPED, Ordering::AcqRel) & STOPPED == 0 {
            let _ = self.port.post(WAKE_STOP);
        }
    }

    pub(crate) fn can_work(&self) -> bool {
        self.work_blocks.load(Ordering::Acquire) == 0
    }

    pub(crate) fn can_dim_locked_keyboard(&self) -> bool {
        self.work_blocks.load(Ordering::Acquire) == LOCKED
    }

    fn block(&self, flag: u32, blocked: bool) {
        if blocked {
            self.work_blocks.fetch_or(flag, Ordering::AcqRel);
        } else {
            self.work_blocks.fetch_and(!flag, Ordering::AcqRel);
        }
    }

    pub(crate) fn flag(&self, bits: u32) {
        self.flags.fetch_or(bits, Ordering::Relaxed);
        self.signal();
    }

    pub(crate) fn action(&self, action: UserEvent) {
        if self.actions.try_send(action).is_ok() {
            self.signal();
        }
    }

    pub(crate) fn power_source(&self, source: u32) {
        self.source.store(
            match source {
                0 => 1,
                1 => 2,
                _ => 3,
            },
            Ordering::Relaxed,
        );
        self.signal();
    }

    pub(crate) fn battery(&self, percent: u32) {
        self.battery.store(
            if percent <= 100 { percent + 1 } else { 102 },
            Ordering::Relaxed,
        );
        self.signal();
    }

    fn boolean(&self, slot: &AtomicU32, value: bool) {
        slot.store(u32::from(value) + 1, Ordering::Relaxed);
        self.signal();
    }

    pub(crate) fn saver(&self, value: bool) {
        self.boolean(&self.saver, value);
    }
    pub(crate) fn effective(&self, mode: i32) {
        self.effective.store(
            if (0..=6).contains(&mode) {
                (mode + 1) as u32
            } else {
                8
            },
            Ordering::Relaxed,
        );
        self.signal();
    }
    pub(crate) fn display(&self, value: bool) {
        self.boolean(&self.display, value);
    }
    pub(crate) fn session(&self, value: bool) {
        self.block(LOCKED, !value);
        self.boolean(&self.unlocked, value);
    }
    pub(crate) fn suspended(&self, value: bool) {
        self.block(SUSPENDED, value);
        self.boolean(&self.suspended, value);
    }

    pub(crate) fn take(&self) -> WindowEvents {
        self.queued.swap(false, Ordering::AcqRel);
        let take = |slot: &AtomicU32| slot.swap(0, Ordering::Relaxed);
        let boolean = |slot| match take(slot) {
            1 => Some(false),
            2 => Some(true),
            _ => None,
        };
        WindowEvents {
            flags: take(&self.flags),
            source: match take(&self.source) {
                1 => Some(Some(PowerSource::Ac)),
                2 => Some(Some(PowerSource::Battery)),
                3 => Some(None),
                _ => None,
            },
            battery: match take(&self.battery) {
                0 => None,
                value @ 1..=101 => Some(Some((value - 1) as u8)),
                _ => Some(None),
            },
            saver: boolean(&self.saver),
            effective: match take(&self.effective) {
                0 => None,
                1 => Some(Some(EffectivePowerMode::BatterySaver)),
                2 => Some(Some(EffectivePowerMode::BetterBattery)),
                3 => Some(Some(EffectivePowerMode::Balanced)),
                4 => Some(Some(EffectivePowerMode::HighPerformance)),
                5 => Some(Some(EffectivePowerMode::MaxPerformance)),
                6 => Some(Some(EffectivePowerMode::GameMode)),
                7 => Some(Some(EffectivePowerMode::MixedReality)),
                _ => Some(None),
            },
            display_on: boolean(&self.display),
            unlocked: boolean(&self.unlocked),
            suspended: boolean(&self.suspended),
        }
    }
}
