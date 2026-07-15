//! Duration-side helpers for replicated pools — "a duration of stuff kept
//! around", in two sibling flavors. **TTL** ([`pool_expire`]): an entry
//! that hasn't been written for a lifetime is removed, which is what makes
//! transient facts (a contact point, a hit marker) safe as ordinary pool
//! entries — the server stops hand-rolling a removal that races the
//! resend window. **History** ([`HistoryRing`]): a window of *past* ticks
//! of a pool, so the server can rewind other entities to the view an
//! acting peer actually saw (lag compensation).
//!
//! Both are the manual seams; the installed forms are
//! [`PmServer::ttl_pool`](crate::PmServer::ttl_pool) and
//! [`PmServer::history_pool`](crate::PmServer::history_pool) — the
//! server-side counterparts of the client's presentation modifiers
//! ([`PmClient::interp_pool`](crate::PmClient::interp_pool)).

use std::collections::VecDeque;

use crate::id::Id;
use crate::kernel::{Pm, PoolHandle};

/// Remove every entity whose entry in `pool` was last written more than
/// `ttl_ticks` ago. Removal goes through the normal deferred path
/// ([`Pm::id_remove`]), so on a server it replicates like any other
/// removal.
///
/// The clock is **ticks since last write** (the pool's change stamps): a
/// mutated entry refreshes its lifetime; the immutable transient facts
/// this exists for age from birth. Expiry removes the *entity*, not just
/// the pool entry — a TTL'd pool owns its entities (each occurrence gets a
/// fresh [`Id`]; see the contact-points rule in the crate netcode docs).
pub fn pool_expire<T: 'static>(pm: &mut Pm, pool: &PoolHandle<T>, ttl_ticks: u32) {
    let now = pm.tick();
    let expired: Vec<Id> = {
        let pool = pool.get();
        pool.ids()
            .iter()
            .zip(pool.changed_ticks())
            .filter(|&(_, &t)| now.saturating_sub(t) > ttl_ticks)
            .map(|(&id, _)| id)
            .collect()
    };
    for id in expired {
        pm.id_remove(id);
    }
}

/// A bounded window of past pool frames, one per tick: `(tick label,
/// entries)` oldest-first, at most `cap` frames. The server-side memory
/// that lag compensation rewinds into — where the client's
/// [`InterpBuffer`](crate::InterpBuffer) holds a *per-entity* ring of
/// samples to render the recent past, this holds *whole-pool* frames so
/// the server can reconstruct it.
///
/// Push once per tick; look up with [`frame`](HistoryRing::frame), which
/// clamps out-of-range ticks to the window edge (a rewind deeper than the
/// window is served the oldest frame — bounded rewind, never a miss).
///
/// ```
/// use pm::{HistoryRing, Id};
///
/// let mut ring = HistoryRing::new(3);
/// let id = Id::new(0, 0, 1);
/// ring.push(10, vec![(id, 1.0f32)]);
/// ring.push(11, vec![(id, 2.0)]);
/// ring.push(12, vec![(id, 3.0)]);
/// ring.push(13, vec![(id, 4.0)]); // cap 3: the tick-10 frame drops
///
/// assert_eq!(ring.frame(12), Some(&[(id, 3.0)][..]));
/// assert_eq!(ring.frame(5), ring.frame(11)); // too old: oldest kept
/// assert_eq!(ring.frame(99), ring.frame(13)); // future: newest
/// ```
pub struct HistoryRing<T> {
    /// `(tick label, frame)` oldest-first; labels strictly increase.
    frames: VecDeque<(u32, Vec<(Id, T)>)>,
    cap: usize,
}

impl<T> HistoryRing<T> {
    /// A ring keeping at most `cap` frames (at least 1).
    pub fn new(cap: usize) -> Self {
        Self {
            frames: VecDeque::new(),
            cap: cap.max(1),
        }
    }

    /// Adjust the window size; excess oldest frames drop immediately.
    pub fn cap_set(&mut self, cap: usize) {
        self.cap = cap.max(1);
        while self.frames.len() > self.cap {
            self.frames.pop_front();
        }
    }

    /// Record the pool's entries as the frame for `tick`. Call once per
    /// tick with an increasing label; a label at or below the newest is
    /// ignored (frames are facts about completed ticks — they don't
    /// rewrite).
    pub fn push(&mut self, tick: u32, frame: Vec<(Id, T)>) {
        if self.frames.back().is_some_and(|&(t, _)| tick <= t) {
            return;
        }
        self.frames.push_back((tick, frame));
        while self.frames.len() > self.cap {
            self.frames.pop_front();
        }
    }

    /// The frame recorded at `tick`, clamped to the window: older than the
    /// window serves the oldest frame, newer than the newest serves the
    /// newest. `None` only before anything was pushed.
    ///
    /// The clamp is silent by design (bounded rewind) — but a rewind
    /// falling off the back of the window means the caller is judging
    /// against state older than the whole ring, which is how the
    /// acked-starvation bug hid for weeks. `PM_NETDBG=1` makes it loud.
    pub fn frame(&self, tick: u32) -> Option<&[(Id, T)]> {
        if crate::netmod::netdbg()
            && let Some(&(oldest, _)) = self.frames.front()
            && tick < oldest
        {
            // Rate-limited: under full starvation this fires per bullet
            // per tick.
            thread_local! {
                static NTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
            }
            let n = NTH.with(|c| {
                let n = c.get();
                c.set(n.wrapping_add(1));
                n
            });
            if n % 64 == 0 {
                eprintln!(
                    "[netdbg hist] rewind to {tick} clamped to window start {oldest} ({} ticks past the back; {n} clamps so far)",
                    oldest - tick
                );
            }
        }
        // partition_point = binary search for the first frame with a label
        // PAST tick; the one before it is the newest frame at-or-before.
        let at = self.frames.partition_point(|&(t, _)| t <= tick);
        self.frames
            .get(at.saturating_sub(1))
            .map(|(_, f)| f.as_slice())
    }

    /// The newest recorded tick label, if any.
    pub fn newest(&self) -> Option<u32> {
        self.frames.back().map(|&(t, _)| t)
    }

    /// The oldest recorded tick label, if any.
    pub fn oldest(&self) -> Option<u32> {
        self.frames.front().map(|&(t, _)| t)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(n: u32) -> Id {
        Id::new(0, 0, n)
    }

    #[test]
    fn ring_caps_and_clamps() {
        let mut ring = HistoryRing::new(4);
        assert_eq!(ring.frame(0), None, "empty ring has no frames");
        for t in 1..=10u32 {
            ring.push(t, vec![(id(1), t as f32)]);
        }
        assert_eq!(ring.oldest(), Some(7));
        assert_eq!(ring.newest(), Some(10));
        // Exact hit, clamp-below, clamp-above.
        assert_eq!(ring.frame(8), Some(&[(id(1), 8.0)][..]));
        assert_eq!(ring.frame(2), Some(&[(id(1), 7.0)][..]));
        assert_eq!(ring.frame(50), Some(&[(id(1), 10.0)][..]));
    }

    #[test]
    fn ring_ignores_stale_labels_and_recaps() {
        let mut ring = HistoryRing::new(8);
        ring.push(5, vec![(id(1), 1)]);
        ring.push(5, vec![(id(1), 2)]); // same label: ignored
        ring.push(4, vec![(id(1), 3)]); // older label: ignored
        assert_eq!(ring.frame(5), Some(&[(id(1), 1)][..]));
        for t in 6..=12u32 {
            ring.push(t, vec![(id(1), t)]);
        }
        ring.cap_set(3);
        assert_eq!(ring.oldest(), Some(10), "cap_set drops oldest frames");
    }

    #[test]
    fn expire_removes_after_ttl_and_refreshes_on_write() {
        #[derive(Clone, Copy)]
        struct P(f32);

        let mut pm = Pm::new();
        let pool = pm.pool::<P>("p");
        pm.task_add("ttl", 5.0, 0.0, {
            let pool = pool.clone();
            move |pm| pool_expire(pm, &pool, 5)
        });

        let a = pm.id_add();
        let b = pm.id_add();
        pool.get_mut().add(a, P(0.0));
        pool.get_mut().add(b, P(0.0));
        for i in 0..4 {
            // Keep writing `b`: its TTL clock restarts every write.
            if let Some(mut e) = pool.get_mut().get_mut(b) {
                e.0 = i as f32;
            }
            pm.loop_once(1.0 / 60.0);
        }
        assert!(pool.get().contains(a), "still inside ttl");
        for _ in 0..4 {
            pm.loop_once(1.0 / 60.0);
        }
        assert!(!pool.get().contains(a), "expired after ttl ticks");
        assert!(pool.get().contains(b), "writes refreshed b's ttl");
        assert!(
            !pm.id_alive(a),
            "expiry removes the entity, not just the entry"
        );
    }
}
