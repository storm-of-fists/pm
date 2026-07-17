//! Timing helpers whose observable state is made of real signals, so a
//! recording captures it and playback can drive it.
//!
//! * `PmEdge`    → signals `rise`, `fall`
//! * `PmTimer`   → signals `out`, `rise`, `fall`, `dur`  (latching TON/TOF)
//! * `PmBlinker` → signals `out`, `rise`, `fall`         (ON-first square wave)
//! * `PmPulse`   → signals `out`, `rise`, `fall`         (fixed width on rise)
//!
//! Names flatten under the field: a group field `debounce: PmTimer` yields
//! `debounce.out`, `debounce.rise`, `debounce.fall`, `debounce.dur`.
//!
//! Playback mechanism: `update()` writes its outputs through the normal
//! lock-gated `set()`. Live, the signals are locked (app-owned) and update()
//! drives them. In playback the manager unlocks and applies recorded values,
//! and the still-running update() calls bounce off the lock — no fighting,
//! same trick the network unlock-override uses. Purely internal state
//! (phase bookkeeping, prev samples, config on_ms/off_ms) stays plain fields.
//!
//! PmEdge boot rule (R_TRIG-flavored): TRUE on the very first sample fires
//! `rise` (a fault active at boot must stamp); a FALSE first sample emits
//! no phantom fall.

use crate::clock;
use crate::pm_group;
use crate::signal::{PmBool, PmU64};

pm_group! {
    /// Edge detector: `rise`/`fall` pulse for one sample on a transition.
    #[derive(Clone)]
    pub struct PmEdge {
        pub rise: PmBool,
        pub fall: PmBool,
        @skip prev: bool,
    }
}

impl PmEdge {
    pub fn update(&mut self, input: bool) {
        self.rise.set(input && !self.prev);
        self.fall.set(!input && self.prev);
        self.prev = input;
    }
}

/// The `out` + `edge` passenger set every timing helper carries: the
/// accessors and the end-of-update write (through the lock gate, so
/// playback can override). Registration comes from `pm_group!` — `out`
/// as a named child, `edge` flattened (`@flat`) to `.rise`/`.fall`.
macro_rules! out_edge_api {
    ($T:ty) => {
        impl $T {
            pub fn out(&self) -> bool {
                self.out.val()
            }
            pub fn rise(&self) -> bool {
                self.edge.rise.val()
            }
            pub fn fall(&self) -> bool {
                self.edge.fall.val()
            }
            /// Write the scan's output and refresh the edges from whatever
            /// value actually stuck (a playback unlock wins over `v`).
            fn apply(&mut self, v: bool) -> bool {
                self.out.set(v);
                self.edge.update(self.out.val());
                self.out.val()
            }
        }
    };
}

pm_group! {
    /// Latching TON/TOF timer (delays in ms; 0 = track the input).
    #[derive(Clone)]
    pub struct PmTimer {
        @skip pub on_ms: u64,
        @skip pub off_ms: u64,
        pub out: PmBool,
        @flat pub edge: PmEdge,
        pub dur: PmU64,
        @skip in_prev: bool,
        @skip edge_at: u64, // time of the last `in` transition
        @skip started: bool,
    }
}

impl PmTimer {
    pub fn on_delay(mut self, ms: u64) -> Self {
        self.on_ms = ms;
        self
    }
    pub fn off_delay(mut self, ms: u64) -> Self {
        self.off_ms = ms;
        self
    }

    /// Elapsed ms since the last `in` transition (the ST `.dur`).
    pub fn dur(&self) -> u64 {
        self.dur.val()
    }

    pub fn update(&mut self, input: bool) -> bool {
        let now = clock::now_ms();
        if !self.started || input != self.in_prev {
            self.edge_at = now;
            self.in_prev = input;
            self.started = true;
        }
        let held = now.saturating_sub(self.edge_at);
        self.dur.set(held);
        // Latching TON/TOF: between the delays the output holds its level.
        let v = if input { held >= self.on_ms || self.out() } else { held < self.off_ms && self.out() };
        self.apply(v)
    }
}

out_edge_api!(PmTimer);

pm_group! {
    /// ON-first square wave while the input holds (see module docs).
    #[derive(Clone)]
    pub struct PmBlinker {
        @skip pub on_ms: u64,
        @skip pub off_ms: u64,
        pub out: PmBool,
        @flat pub edge: PmEdge,
        @skip running: bool,
        @skip phase_on: bool,
        @skip phase_at: u64,
    }
}

impl PmBlinker {
    /// Symmetric square wave; chain [`off_ms`](Self::off_ms) after it for
    /// an asymmetric duty cycle.
    pub fn period(mut self, on_ms: u64) -> Self {
        self.on_ms = on_ms;
        self.off_ms = on_ms;
        self
    }
    pub fn off_ms(mut self, ms: u64) -> Self {
        self.off_ms = ms;
        self
    }

    pub fn update(&mut self, input: bool) -> bool {
        let now = clock::now_ms();
        if !input {
            self.running = false;
        } else if !self.running {
            // ON-first (see module docs).
            self.running = true;
            self.phase_on = true;
            self.phase_at = now;
        } else {
            let span = if self.phase_on { self.on_ms } else { self.off_ms };
            if now.saturating_sub(self.phase_at) >= span {
                self.phase_on = !self.phase_on;
                self.phase_at = now;
            }
        }
        self.apply(input && self.phase_on)
    }
}

out_edge_api!(PmBlinker);

pm_group! {
    /// Fixed-width pulse fired on each input rise.
    #[derive(Clone)]
    pub struct PmPulse {
        @skip pub width_ms: u64,
        pub out: PmBool,
        @flat pub edge: PmEdge,
        @skip trig_prev: bool,
        @skip fired_at: u64,
        @skip active: bool,
    }
}

impl PmPulse {
    pub fn width(mut self, ms: u64) -> Self {
        self.width_ms = ms;
        self
    }

    pub fn update(&mut self, input: bool) -> bool {
        let now = clock::now_ms();
        if input && !self.trig_prev {
            self.active = true;
            self.fired_at = now;
        }
        self.trig_prev = input;
        if self.active && now.saturating_sub(self.fired_at) >= self.width_ms {
            self.active = false;
        }
        self.apply(self.active)
    }
}

out_edge_api!(PmPulse);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock;
    use crate::signal::Registrar;

    #[test]
    fn timer_is_latching_ton_tof() {
        clock::set(0);
        let mut t = PmTimer::new().on_delay(500).off_delay(200);
        assert!(!t.update(true));
        clock::set(499);
        assert!(!t.update(true));
        assert_eq!(t.dur(), 499);
        clock::set(500);
        assert!(t.update(true));
        assert!(t.rise());
        clock::set(10_000);
        assert!(t.update(true)); // latched while held
        assert!(!t.rise());
        clock::set(10_001);
        assert!(t.update(false)); // off-delay holds it
        clock::set(10_201);
        assert!(!t.update(false));
        assert!(t.fall());
    }

    #[test]
    fn timer_zero_delays_track_input() {
        clock::set(0);
        let mut t = PmTimer::new();
        assert!(t.update(true));
        assert!(t.rise()); // first-sample TRUE fires (R_TRIG boot rule)
        assert!(!t.update(false));
    }

    #[test]
    fn blinker_on_first_square_wave() {
        clock::set(0);
        let mut b = PmBlinker::new().period(100);
        assert!(b.update(true)); // on-first
        assert!(b.rise());
        clock::set(99);
        assert!(b.update(true));
        clock::set(100);
        assert!(!b.update(true)); // off phase
        assert!(b.fall());
        clock::set(200);
        assert!(b.update(true)); // on again
        assert!(b.rise());
        assert!(!b.update(false)); // drops immediately with input
    }

    #[test]
    fn pulse_fires_fixed_width_on_rise() {
        clock::set(0);
        let mut p = PmPulse::new().width(300);
        assert!(p.update(true));
        assert!(p.rise());
        clock::set(299);
        assert!(p.update(false)); // input gone, pulse holds
        clock::set(300);
        assert!(!p.update(false));
        assert!(p.fall());
        clock::set(301);
        assert!(p.update(true)); // fresh rise re-fires
        assert!(p.rise());
    }

    #[test]
    fn edge_boot_rule() {
        let mut e = PmEdge::default();
        e.update(false);
        assert!(!e.fall.val()); // silent first-FALSE (no F_TRIG phantom)
        let mut e2 = PmEdge::default();
        e2.update(true);
        assert!(e2.rise.val()); // first-TRUE fires (boot-active fault must stamp)
        e2.update(false);
        assert!(e2.fall.val());
    }

    #[test]
    fn timer_state_registers_as_signals() {
        let t = PmTimer::new().on_delay(100);
        let reg = Registrar::collect(&t);
        let names: Vec<String> = reg.signals.iter().map(|s| s.meta().name()).collect();
        assert_eq!(names, vec!["out", "rise", "fall", "dur"]);
    }

    #[test]
    fn playback_unlock_overrides_live_update() {
        clock::set(0);
        let mut t = PmTimer::new();
        t.update(true);
        assert!(t.out());
        // Playback takes ownership: unlock, apply recorded value.
        t.out.meta().locked.set(false);
        t.out.set_raw(false); // manager-side raw apply
        t.update(true); // live eval keeps running…
        assert!(!t.out()); // …but bounces off the lock
    }
}
