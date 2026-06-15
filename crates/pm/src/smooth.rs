//! Presentation-side helpers for replicated state: mirror an
//! authoritative pool into a draw pool with a game-supplied blend, and
//! the standard coast+blend math for dead reckoning. Both games (demo,
//! hellfire) wrote this by hand before it was hoisted here.

use std::collections::{HashMap, VecDeque};

use crate::id::Id;
use crate::kernel::PoolHandle;
use crate::math::Vec2;

/// Mirror `auth` into `draw`: new entities copy in, existing ones go
/// through `blend(id, previous_draw, auth) -> next_draw`, entities gone
/// from `auth` drop out. Call once per tick from a smoothing task; the
/// draw pool is what rendering should read.
pub fn pool_mirror<T: Copy + 'static>(
    auth: &PoolHandle<T>,
    draw: &PoolHandle<T>,
    mut blend: impl FnMut(crate::Id, T, &T) -> T,
) {
    let auth = auth.get();
    let mut draw = draw.get_mut();
    for (id, a) in auth.iter() {
        match draw.get_mut(id) {
            Some(mut d) => *d = blend(id, *d, a),
            None => draw.add(id, *a),
        }
    }
    draw.retain(|id, _| auth.contains(id));
}

/// Dead-reckoning step: coast the previous draw position along its
/// velocity for `dt`, then ease toward the authoritative position by
/// `blend` (0..1). Hides the bounded staleness of budget-rotated
/// snapshots without visible snapping.
pub fn coast_blend(pos: Vec2, vel: Vec2, auth_pos: Vec2, dt: f32, blend: f32) -> Vec2 {
    let coast = pos + vel * dt;
    coast + (auth_pos - coast) * blend
}

/// Snapshot interpolation: a per-entity ring of authoritative samples, so
/// rendering can show a remote entity at `now - delay` — *between* two
/// known-true samples — instead of extrapolating past the newest one. The
/// difference from `coast_blend` is the difference between interpolation
/// and dead reckoning: extrapolation guesses the future and snaps when the
/// guess was wrong (every direction change, every dropped snapshot);
/// interpolation only ever fills *between* facts, so loss just widens the
/// bracket it spans — no snap. The cost is a fixed `delay` of apparent
/// latency on the entities it smooths, which for non-owned entities is the
/// standard trade (your own avatar stays on the predictor, instant).
///
/// Drive against `pool_interp` once per smoothing tick; tune `delay` to a
/// snapshot interval or two (long enough to ride the worst loss burst,
/// short enough that other entities don't feel laggy).
pub struct InterpBuffer<T> {
    /// Per entity, `(sample_time, value)` oldest-first. Only genuinely new
    /// values append — a held/stationary entity adds nothing and reads its
    /// last value straight back.
    samples: HashMap<Id, VecDeque<(f64, T)>>,
    /// Render this far behind the newest sample (seconds).
    pub delay: f64,
    /// When the buffer runs dry (a loss burst leaves nothing at `now -
    /// delay`), extrapolate along the last segment for at most this long
    /// before holding. 0 = pure interpolation (hold newest, never guess);
    /// a small value (~a snapshot interval) rides short bursts without the
    /// snap. The Source `cl_interp` + capped-extrapolate model.
    pub extrap_max: f64,
    /// Samples older than `delay + window` are dropped to bound the rings.
    window: f64,
}

impl<T: Copy + PartialEq> InterpBuffer<T> {
    /// `delay` seconds behind newest is where rendering samples. A good
    /// start is one-to-two snapshot intervals (e.g. 0.1 at 60 Hz).
    /// Extrapolation is off by default; set `extrap_max` to ride loss
    /// bursts without snapping.
    pub fn new(delay: f64) -> Self {
        Self {
            samples: HashMap::new(),
            delay,
            extrap_max: 0.0,
            window: 0.5,
        }
    }

    /// Record an authoritative sample for `id` at time `now`. A no-op when
    /// the value equals the last one stored (a stationary entity grows no
    /// ring), so call it every tick — only real changes cost anything.
    pub fn push(&mut self, id: Id, now: f64, v: T) {
        let ring = self.samples.entry(id).or_default();
        if ring.back().is_none_or(|&(_, last)| last != v) {
            ring.push_back((now, v));
        }
        let cutoff = now - (self.delay + self.window);
        while ring.len() > 2 && ring.front().is_some_and(|&(t, _)| t < cutoff) {
            ring.pop_front();
        }
    }

    /// Value at `now - delay`, via the game's `lerp`. Clamps to the oldest
    /// sample before the ring starts. Past the newest sample (buffer dry)
    /// it extrapolates along the last segment, capped at `extrap_max`, then
    /// holds — with `extrap_max == 0` that's a pure hold (no guessing).
    pub fn sample(&self, id: Id, now: f64, lerp: impl Fn(&T, &T, f32) -> T) -> Option<T> {
        let ring = self.samples.get(&id)?;
        let front = *ring.front()?;
        let target = now - self.delay;
        if target <= front.0 {
            return Some(front.1);
        }
        let mut prev = front;
        for &cur in ring.iter().skip(1) {
            if target <= cur.0 {
                let span = cur.0 - prev.0;
                let t = if span > 1e-9 {
                    ((target - prev.0) / span) as f32
                } else {
                    1.0
                };
                return Some(lerp(&prev.1, &cur.1, t));
            }
            prev = cur;
        }
        // Buffer dry: extrapolate along the last segment up to extrap_max,
        // then hold. `prev` is the newest sample; pair it with the one
        // before to get the segment's slope and run `lerp` past t == 1.
        let newest = prev;
        if self.extrap_max > 0.0
            && let Some(&prev2) = ring.iter().rev().nth(1)
        {
            let span = newest.0 - prev2.0;
            let capped = target.min(newest.0 + self.extrap_max);
            if span > 1e-9 {
                let t = ((capped - prev2.0) / span) as f32; // > 1
                return Some(lerp(&prev2.1, &newest.1, t));
            }
        }
        Some(newest.1)
    }

    pub fn contains(&self, id: Id) -> bool {
        self.samples.contains_key(&id)
    }

    fn retain_live(&mut self, keep: impl Fn(Id) -> bool) {
        self.samples.retain(|&id, _| keep(id));
    }
}

/// Mirror `auth` into `draw` through an [`InterpBuffer`]: feed this tick's
/// authoritative values in, then write each entity's `now - delay`
/// interpolated value out (via the game's angle-/field-aware `lerp`).
/// Entities gone from `auth` drop from both the buffer and `draw`. The
/// snapshot-interpolation counterpart to `pool_mirror` + `coast_blend`.
pub fn pool_interp<T: Copy + PartialEq + 'static>(
    auth: &PoolHandle<T>,
    draw: &PoolHandle<T>,
    buf: &mut InterpBuffer<T>,
    now: f64,
    lerp: impl Fn(&T, &T, f32) -> T,
) {
    {
        let auth = auth.get();
        for (id, a) in auth.iter() {
            buf.push(id, now, *a);
        }
        buf.retain_live(|id| auth.contains(id));
    }
    let ids: Vec<Id> = buf.samples.keys().copied().collect();
    let mut draw = draw.get_mut();
    for id in ids {
        if let Some(v) = buf.sample(id, now, &lerp) {
            match draw.get_mut(id) {
                Some(mut d) => *d = v,
                None => draw.add(id, v),
            }
        }
    }
    draw.retain(|id, _| buf.contains(id));
}
