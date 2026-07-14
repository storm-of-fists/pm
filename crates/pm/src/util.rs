//! PLC-inspired logic helpers (hysteresis, cooldowns, edges, latches).
//! All time is float seconds, matching `pm.loop_dt()`.
//!
//! Rust note: Rust has no implicit conversions, so these expose
//! explicit `get()` / `is_on()` accessors rather than converting to
//! their value.
//!
//! ```
//! use pm::{Cooldown, RisingEdge};
//!
//! // Fire once per interval, accumulating dt across ticks.
//! let mut fire = Cooldown::new(0.25);
//! let mut shots = 0;
//! for _ in 0..60 {
//!     if fire.ready(1.0 / 60.0) {
//!         shots += 1;
//!     }
//! }
//! assert_eq!(shots, 4); // one second / 0.25 s
//!
//! // One-tick pulse on a false -> true transition (debounced intent).
//! let mut jump = RisingEdge::default();
//! assert!(jump.update(true)); // pressed this tick
//! assert!(!jump.update(true)); // still held: no repeat
//! assert!(!jump.update(false));
//! assert!(jump.update(true)); // pressed again
//! ```

/// Value with dead-zone persistence: changes are blocked for `hold`
/// seconds after each transition (anti-flicker).
#[derive(Clone, Copy, Debug, Default)]
pub struct Hysteresis<T: Copy + PartialEq> {
    value: T,
    hold: f32,
    cooldown: f32,
}

impl<T: Copy + PartialEq> Hysteresis<T> {
    pub fn new(initial: T, hold_sec: f32) -> Self {
        Self {
            value: initial,
            hold: hold_sec,
            cooldown: 0.0,
        }
    }

    pub fn update(&mut self, dt: f32) {
        if self.cooldown > 0.0 {
            self.cooldown -= dt;
        }
    }

    /// Applies only if the hold cooldown has expired.
    pub fn set(&mut self, v: T) {
        if self.cooldown > 0.0 {
            return;
        }
        if self.value != v {
            self.value = v;
            self.cooldown = self.hold;
        }
    }

    pub fn get(&self) -> T {
        self.value
    }
}

/// Fire-once-per-interval timer.
#[derive(Clone, Copy, Debug, Default)]
pub struct Cooldown {
    pub interval: f32,
    pub elapsed: f32,
}

impl Cooldown {
    pub fn new(sec: f32) -> Self {
        Self {
            interval: sec,
            elapsed: 0.0,
        }
    }

    /// Accumulates `dt`; returns true once per interval.
    pub fn ready(&mut self, dt: f32) -> bool {
        self.elapsed += dt;
        if self.elapsed >= self.interval {
            self.elapsed -= self.interval;
            return true;
        }
        false
    }

    pub fn reset(&mut self) {
        self.elapsed = 0.0;
    }

    pub fn remaining(&self) -> f32 {
        self.interval - self.elapsed
    }
}

/// Unified on-delay / off-delay timer (TON + TOF). Output goes true
/// after the input has been true for `on_delay` seconds; false after the
/// input has been false for `off_delay` seconds.
#[derive(Clone, Copy, Debug, Default)]
pub struct DelayTimer {
    pub on_delay: f32,
    pub off_delay: f32,
    elapsed: f32,
    output: bool,
}

impl DelayTimer {
    pub fn new(on_sec: f32, off_sec: f32) -> Self {
        Self {
            on_delay: on_sec,
            off_delay: off_sec,
            elapsed: 0.0,
            output: false,
        }
    }

    pub fn update(&mut self, input: bool, dt: f32) {
        if self.output != input {
            self.elapsed += dt;
            let delay = if self.output {
                self.off_delay
            } else {
                self.on_delay
            };
            if self.elapsed >= delay {
                self.output = input;
                self.elapsed = 0.0;
            }
        } else {
            self.elapsed = 0.0;
        }
    }

    pub fn reset(&mut self) {
        self.output = false;
        self.elapsed = 0.0;
    }

    pub fn is_on(&self) -> bool {
        self.output
    }
}

/// One-tick pulse when the input goes false → true.
#[derive(Clone, Copy, Debug, Default)]
pub struct RisingEdge {
    previous: bool,
}

impl RisingEdge {
    pub fn update(&mut self, input: bool) -> bool {
        let fired = input && !self.previous;
        self.previous = input;
        fired
    }
}

/// One-tick pulse when the input goes true → false.
#[derive(Clone, Copy, Debug, Default)]
pub struct FallingEdge {
    previous: bool,
}

impl FallingEdge {
    pub fn update(&mut self, input: bool) -> bool {
        let fired = !input && self.previous;
        self.previous = input;
        fired
    }
}

/// Set-reset flip-flop. When set and reset are both true,
/// `reset_dominant` decides which wins (default: reset).
#[derive(Clone, Copy, Debug)]
pub struct Latch {
    output: bool,
    pub reset_dominant: bool,
}

impl Default for Latch {
    fn default() -> Self {
        Self {
            output: false,
            reset_dominant: true,
        }
    }
}

impl Latch {
    pub fn new(reset_dominant: bool) -> Self {
        Self {
            output: false,
            reset_dominant,
        }
    }

    pub fn update(&mut self, set: bool, reset: bool) {
        if set && reset {
            self.output = !self.reset_dominant;
        } else if set {
            self.output = true;
        } else if reset {
            self.output = false;
        }
    }

    pub fn is_on(&self) -> bool {
        self.output
    }
}

/// `RisingEdge` for pool membership: which entities appeared since the
/// last call? Replication converges STATE ("what exists"), but one-shot
/// reactions — sounds, camera shake, particles — need the EDGE ("it
/// just appeared"). Point one of these at a synced pool (impacts,
/// bullets) and react to what it returns each tick.
///
/// The first call primes silently and reports nothing: on connect, the
/// initial snapshot dumps the whole existing world, and a join
/// shouldn't replay every standing entity as if it just happened.
///
/// ```
/// use pm::{Births, Id, Pool};
///
/// let mut pool: Pool<f32> = Pool::new();
/// let mut sfx = Births::default();
/// pool.add(Id::new(0, 0, 1), 1.0);
/// assert!(sfx.drain(&pool).is_empty()); // first call: prime, no replay
/// pool.add(Id::new(0, 0, 2), 2.0);
/// assert_eq!(sfx.drain(&pool), vec![Id::new(0, 0, 2)]); // the newborn
/// assert!(sfx.drain(&pool).is_empty()); // an entity is born once
/// ```
#[derive(Default)]
pub struct Births {
    seen: std::collections::HashSet<crate::Id>,
    primed: bool,
}

impl Births {
    /// Ids present in `pool` that weren't present last call, and forget
    /// ids that have since been removed (so a recycled slot's next
    /// occupant — a new generation — still counts as born).
    pub fn drain<T>(&mut self, pool: &crate::Pool<T>) -> Vec<crate::Id> {
        let mut born = Vec::new();
        for &id in pool.ids() {
            if self.seen.insert(id) && self.primed {
                born.push(id);
            }
        }
        self.seen.retain(|id| pool.contains(*id));
        self.primed = true;
        born
    }
}

/// Count up/down toward a preset with a done flag. Compose with
/// `RisingEdge` for edge-triggered counting.
#[derive(Clone, Copy, Debug, Default)]
pub struct Counter {
    pub count: i32,
    pub preset: i32,
    pub done: bool,
}

impl Counter {
    pub fn new(preset: i32) -> Self {
        Self {
            count: 0,
            preset,
            done: false,
        }
    }

    pub fn increment(&mut self) {
        if self.done {
            return;
        }
        self.count += 1;
        if self.count >= self.preset {
            self.done = true;
        }
    }

    pub fn decrement(&mut self) {
        if self.done {
            return;
        }
        self.count -= 1;
        if self.count <= 0 {
            self.count = 0;
            self.done = true;
        }
    }

    pub fn reset(&mut self) {
        self.count = 0;
        self.done = false;
    }

    pub fn reset_to(&mut self, new_preset: i32) {
        self.preset = new_preset;
        self.reset();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hysteresis_blocks_flicker() {
        let mut h = Hysteresis::new(false, 0.1);
        h.set(true);
        assert!(h.get());
        h.set(false); // inside hold window: ignored
        assert!(h.get());
        h.update(0.11);
        h.set(false);
        assert!(!h.get());
    }

    #[test]
    fn cooldown_fires_once_per_interval() {
        let mut c = Cooldown::new(0.5);
        let mut fires = 0;
        for _ in 0..60 {
            if c.ready(1.0 / 60.0) {
                fires += 1;
            }
        }
        assert_eq!(fires, 2);
    }

    #[test]
    fn delay_timer_on_off() {
        let mut t = DelayTimer::new(0.2, 0.1);
        for _ in 0..11 {
            t.update(true, 1.0 / 60.0);
        }
        assert!(!t.is_on()); // 11/60 s < 0.2
        t.update(true, 0.1);
        assert!(t.is_on());
        t.update(false, 0.05);
        assert!(t.is_on()); // off-delay not elapsed
        t.update(false, 0.06);
        assert!(!t.is_on());
    }

    #[test]
    fn edges_and_latch_and_counter() {
        let mut re = RisingEdge::default();
        assert!(re.update(true));
        assert!(!re.update(true));
        let mut fe = FallingEdge::default();
        fe.update(true);
        assert!(fe.update(false));

        let mut l = Latch::default();
        l.update(true, false);
        assert!(l.is_on());
        l.update(true, true); // reset-dominant
        assert!(!l.is_on());

        let mut c = Counter::new(2);
        c.increment();
        assert!(!c.done);
        c.increment();
        assert!(c.done);
        c.increment();
        assert_eq!(c.count, 2);
    }
}
