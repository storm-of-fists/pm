//! Presentation-side interpolation for replicated state: the
//! [`InterpBuffer`] sample rings and the [`pool_interp`] mirror pass
//! behind [`PmClient::interp_pool`](crate::PmClient::interp_pool).
//! (pool_mirror and coast_blend — the deleted demos' dead-reckoning
//! teaching seams — were removed 2026-07-23 with their last callers.)

// TODO(roadmap): interp draw-pool per-frame cost is a watch item once
// visible-entity counts grow (it rebuilds every entity every frame;
// cullable).

use std::collections::{HashMap, VecDeque};

use crate::id::Id;
use crate::kernel::PoolHandle;

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
/// Crate-internal: the one public door is
/// [`PmClient::interp_pool`](crate::PmClient::interp_pool). This is the
/// per-entity CHANGE-POINT projection of a pool's past — same tick axis
/// as the server's `Journal`, different query ("when did this entity
/// last actually move", which is what smooth rendering interpolates
/// across); it folds into a client-side `Journal` if kill-cam ever
/// gives that store a second customer.
pub(crate) struct InterpBuffer<T> {
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
/// authoritative values in stamped at `push_now`, then write each entity's
/// `now - delay` interpolated value out (via the game's angle-/field-aware
/// `lerp`). Entities gone from `auth` drop from both the buffer and
/// `draw`. The snapshot-interpolation counterpart to `pool_mirror` +
/// `coast_blend`.
///
/// `push_now` and `now` are separate so samples can live on the TICK
/// AXIS (v2 item 2): stamp pushes at the applied snapshot's label time
/// (exact server pacing, immune to arrival jitter) while `now` is the
/// smoothly-advancing render estimate on that same axis. A caller on a
/// single local clock passes the same value for both — the old behavior.
///
/// This is the manual seam; the per-pool modifier it was promoted into is
/// [`PmClient::interp_pool`](crate::PmClient::interp_pool) — one call
/// installs the task, owns the buffer and both clocks, and hands back the
/// draw pool.
pub(crate) fn pool_interp<T: Copy + PartialEq + 'static>(
    auth: &PoolHandle<T>,
    draw: &PoolHandle<T>,
    buf: &mut InterpBuffer<T>,
    push_now: f64,
    now: f64,
    lerp: impl Fn(&T, &T, f32) -> T,
) {
    {
        let auth = auth.get();
        for (id, a) in auth.iter() {
            buf.push(id, push_now, *a);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn samples_between_known_points_never_past_newest() {
        let mut buf = InterpBuffer::new(0.1); // render 100 ms behind newest
        let id = Id::new(0, 0, 1);
        buf.push(id, 0.0, 0.0f32); // sample at t=0.0
        buf.push(id, 0.1, 10.0); // sample at t=0.1 (moved 0 -> 10)
        // At now=0.15 we render now-delay=0.05 — halfway BETWEEN the two
        // known-true samples, never extrapolated past the newest.
        let lerp = |a: &f32, b: &f32, t: f32| a + (b - a) * t;
        assert_eq!(buf.sample(id, 0.15, lerp), Some(5.0));
        // Pure-interp default: past the newest sample it holds.
        assert_eq!(buf.sample(id, 0.5, lerp), Some(10.0));
    }
}
