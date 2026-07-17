//! FaultTable — the fault bookkeeping half of the ST SignalManager, plus
//! the notification machinery ([`Notifier`]).
//!
//! ST semantics, kept exactly (SignalManager.end_cycle):
//! * a fault *stamps* on its rising edge (a fault active at boot stamps on
//!   the first sample);
//! * the table lists every stamped fault until it is **cleared**, even after
//!   the value drops — `active` says which rows are still true;
//! * bookkeeping runs only while `connected`: disconnected, the table shows
//!   empty, but the stamps persist and the rows return on reconnect;
//! * [`clear`](FaultTable::clear) is the ST `clear_fault`: stamp reset, the
//!   display copy drops now, and the owner gets an unlock-override-to-false
//!   followed by an immediate relock — evaluation resumes, so a persisting
//!   condition re-trips (and re-stamps).
//!
//! The blink policy lives here too, not in the core signal types:
//! `blink_slow` (700 ms) / `blink_fast` (300 ms) free-run, and
//! `blink_fault` = slow blink gated on a non-empty table.
//!
//! Flagged deviations from ST:
//! * Stamps are ambient-clock ms (`u64`), not RTC date strings — RTC comes
//!   in through a host trait later; hosts format.
//! * `description` is real (from [`PmFault::describe`], fed by io
//!   `PhysInfo`); ST shipped the literal `'TODO'`.
//! * Edge detection is table-private (`prev` per fault) instead of reusing
//!   the fault's own `PmEdge`, so the table also works on the owning node,
//!   where the app's `set()` already drives that edge each scan.
//! * The fault-snapshot blackbox copy is recording's job (next chunk), and
//!   `notify`'s ST `prio` input was dead code — dropped.

use crate::fault::PmFault;
use crate::net::NetworkManager;
use crate::signal::{Register, Registrar};
use crate::timers::{PmBlinker, PmTimer};
use alloc::string::{String, ToString};
use alloc::vec::Vec;

/// One row of the fault table (ST `FaultRecord`).
#[derive(Clone, Debug)]
pub struct FaultRecord {
    /// Registration index — the handle [`FaultTable::clear`] takes.
    pub index: usize,
    /// Ambient-clock time of the stamping rise. 0 only for a fault that is
    /// active again right after a clear (re-tripped, not yet re-stamped).
    pub stamp_ms: u64,
    pub name: String,
    pub description: String,
    /// Still true right now; a stamped-but-dropped fault stays listed with
    /// `active == false` until cleared.
    pub active: bool,
}

pub struct FaultTable {
    faults: Vec<PmFault>,
    /// Rise time per fault; 0 = not stamped (no row).
    stamps: Vec<u64>,
    prev: Vec<bool>,
    /// Rebuilt every [`update`](Self::update), in registration order.
    pub records: Vec<FaultRecord>,
    /// Slow blink AND table non-empty (ST `blink_fault`).
    pub blink_fault: bool,
    pub blink_slow: PmBlinker,
    pub blink_fast: PmBlinker,
}

impl Default for FaultTable {
    fn default() -> Self {
        Self::new()
    }
}

impl FaultTable {
    pub fn new() -> Self {
        FaultTable {
            faults: Vec::new(),
            stamps: Vec::new(),
            prev: Vec::new(),
            records: Vec::new(),
            blink_fault: false,
            blink_slow: PmBlinker::new().period(700),
            blink_fast: PmBlinker::new().period(300),
        }
    }

    /// Register everything under a `Register` root (the ST `add_fault` calls
    /// from `Fault.fb_init`, batched).
    pub fn add(&mut self, root: &impl Register) {
        self.add_registered(&Registrar::collect(root));
    }

    /// Register everything a `Registrar::collect` pass found.
    pub fn add_registered(&mut self, reg: &Registrar) {
        for f in &reg.faults {
            self.faults.push(f.clone());
            self.stamps.push(0);
            self.prev.push(false);
        }
    }

    /// Call once per scan, after the app has evaluated its faults (bottom of
    /// scan, like the ST end_cycle). `connected` gates bookkeeping the way
    /// ST did — pass `net.status().connected`, or `true` with no network.
    pub fn update(&mut self, connected: bool) {
        self.records.clear();
        if connected {
            // 0 means "not stamped", so a rise at boot (clock 0) stamps 1.
            let now = crate::clock::now_ms().max(1);
            for (i, f) in self.faults.iter().enumerate() {
                let val = f.val();
                if val && !self.prev[i] && self.stamps[i] == 0 {
                    self.stamps[i] = now;
                }
                self.prev[i] = val;
                // Stamped OR active: an active fault is never invisible,
                // even in the clear/re-trip window before its new stamp.
                if self.stamps[i] != 0 || val {
                    self.records.push(FaultRecord {
                        index: i,
                        stamp_ms: self.stamps[i],
                        name: f.meta().name(),
                        description: f.description(),
                        active: val,
                    });
                }
            }
        }
        self.blink_slow.update(true);
        self.blink_fast.update(true);
        self.blink_fault = self.blink_slow.out() && !self.records.is_empty();
    }

    /// ST `clear_fault`: drop the row, drop the display copy now, and pulse
    /// the owner — unlock-override to false, then immediate relock, so the
    /// owner's evaluation resumes and a persisting condition re-trips. For a
    /// locally-owned fault the pulse resolves to nothing and the local clear
    /// is the whole story.
    pub fn clear(&mut self, index: usize, net: &mut NetworkManager) {
        let Some(f) = self.faults.get(index) else {
            return;
        };
        self.stamps[index] = 0;
        f.clear(); // display copy drops now
        // A remote owner keeps broadcasting the stale true value until the
        // pulse lands — arm `prev` so that window can't re-stamp; the first
        // false sample re-arms rise detection. A local fault cleared
        // synchronously has no such window.
        self.prev[index] = net.pulse(&f.meta().name());
    }
}

/// Operator notification machinery (ST `SignalManager.notify` + outputs).
///
/// Call [`notify`](Self::notify) every scan from wherever conditions are
/// evaluated; the latest true condition owns the message. `show` holds for
/// `show_off_ms` after the condition drops (0 = only while held); `audio`
/// follows the condition but goes quiet after `audio_off_ms` (0 = never).
#[derive(Default)]
pub struct Notifier {
    pub msg: String,
    pub warning: bool,
    pub audio: bool,
    pub show: bool,
    show_timer: PmTimer,
    audio_timer: PmTimer,
}

impl Notifier {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn notify(
        &mut self,
        cond: bool,
        msg: &str,
        show_on_ms: u64,
        show_off_ms: u64,
        audio_off_ms: u64,
        warn: bool,
    ) {
        self.show_timer.on_ms = show_on_ms;
        self.show_timer.off_ms = show_off_ms;
        let show_out = self.show_timer.update(cond);
        if cond {
            self.msg = msg.to_string();
            self.warning = warn;
        }
        self.show = show_out || (cond && show_off_ms == 0);
        self.audio_timer.on_ms = audio_off_ms;
        let audio_out = self.audio_timer.update(self.show);
        self.audio = self.show && (audio_off_ms == 0 || !audio_out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{clock, pm_group, NetworkManager, PmBool};

    pm_group! {
        struct App {
            run: PmBool,
            over_flt: PmFault = PmFault::new().describe("motor overspeed"),
            latch_flt: PmFault = PmFault::new().latch(),
        }
    }

    fn table_for(app: &App) -> FaultTable {
        let mut t = FaultTable::new();
        t.add(app);
        t
    }

    #[test]
    fn stamps_on_rise_and_lists_until_cleared() {
        clock::set(1_000);
        let app = App::new();
        let mut t = table_for(&app);
        let mut net = NetworkManager::new();

        t.update(true);
        assert!(t.records.is_empty());

        app.over_flt.set(true);
        t.update(true);
        assert_eq!(t.records.len(), 1);
        assert_eq!(t.records[0].name, "over_flt");
        assert_eq!(t.records[0].description, "motor overspeed");
        assert_eq!(t.records[0].stamp_ms, 1_000);
        assert!(t.records[0].active);

        // Condition drops: the row stays, marked inactive, stamp unchanged.
        clock::set(2_000);
        app.over_flt.set(false);
        t.update(true);
        assert_eq!(t.records.len(), 1);
        assert!(!t.records[0].active);
        assert_eq!(t.records[0].stamp_ms, 1_000);

        t.clear(t.records[0].index, &mut net);
        t.update(true);
        assert!(t.records.is_empty());
    }

    #[test]
    fn disconnect_empties_display_but_keeps_stamps() {
        clock::set(0);
        let app = App::new();
        let mut t = table_for(&app);
        app.over_flt.set(true);
        t.update(true);
        assert_eq!(t.records.len(), 1);

        t.update(false); // link lost: table shows empty
        assert!(t.records.is_empty());

        t.update(true); // back: the stamped row returns
        assert_eq!(t.records.len(), 1);
        assert_eq!(t.records[0].stamp_ms, 1); // boot-time stamp survived (0 → clamped to 1)
    }

    #[test]
    fn cleared_latched_fault_retrips_if_condition_persists() {
        clock::set(0);
        let app = App::new();
        let mut t = table_for(&app);
        let mut net = NetworkManager::new();

        app.latch_flt.set(true);
        t.update(true);
        app.latch_flt.set(false); // latched: stays up
        t.update(true);
        assert!(t.records.iter().any(|r| r.name == "latch_flt" && r.active));

        let idx = t.records.iter().find(|r| r.name == "latch_flt").unwrap().index;
        t.clear(idx, &mut net);
        t.update(true);
        assert!(t.records.is_empty());
        assert!(!app.latch_flt.val());

        // Condition still true next scan: re-trips and re-stamps.
        clock::set(100);
        app.latch_flt.set(true);
        t.update(true);
        assert_eq!(t.records.len(), 1);
        assert_eq!(t.records[0].stamp_ms, 100);
    }

    #[test]
    fn blink_gates_on_table_not_value() {
        clock::set(0);
        let app = App::new();
        let mut t = table_for(&app);
        t.update(true);
        assert!(!t.blink_fault);
        app.over_flt.set(true);
        t.update(true);
        assert!(t.blink_fault); // blinker ON-first phase
        app.over_flt.set(false);
        t.update(true);
        assert!(t.blink_fault); // row still stamped → still blinking
    }

    #[test]
    fn notifier_show_and_audio_windows() {
        clock::set(0);
        let mut n = Notifier::new();
        n.notify(true, "low fuel", 0, 2_000, 1_000, true);
        assert!(n.show && n.audio && n.warning);
        assert_eq!(n.msg, "low fuel");

        // Audio quiets after audio_off_ms while the condition holds.
        clock::set(1_000);
        n.notify(true, "low fuel", 0, 2_000, 1_000, true);
        assert!(n.show && !n.audio);

        // Condition drops: the message keeps showing for show_off_ms.
        clock::set(1_500);
        n.notify(false, "low fuel", 0, 2_000, 1_000, true);
        assert!(n.show);
        clock::set(4_000);
        n.notify(false, "low fuel", 0, 2_000, 1_000, true);
        assert!(!n.show && !n.audio);
    }
}
