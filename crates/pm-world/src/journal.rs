//! THE JOURNAL: a window of *past* ticks of a pool, so the server can
//! rewind other entities to the view an acting peer actually saw (lag
//! compensation) — and the store recordings/kill-cams derive from. One
//! public door: [`PmServer::journal_pool`](crate::PmServer::journal_pool)
//! returns the [`JournalHandle`](crate::JournalHandle) tasks read;
//! [`Journal`] here is its storage. (TTL expiry — the OTHER server time
//! modifier — is not a journal and lives with `ttl_pool` in the role
//! module; the old shared "duration.rs" home made them look like
//! siblings.)

// TODO(roadmap): watch item, no action until measured — history-ring
// memory and rewind scans past a few thousand colliders (each rewound
// query walks a full frame copy).

use std::collections::VecDeque;

use crate::id::Id;


/// A bounded window of past pool frames, one per tick: `(tick label,
/// entries)` oldest-first, at most `cap` frames. The server-side memory
/// that lag compensation rewinds into — where the client's
/// [`InterpBuffer`](crate::InterpBuffer) holds a *per-entity* ring of
/// samples to render the recent past, this holds *whole-pool* frames so
/// the server can reconstruct it.
///
/// Push once per tick; look up with [`frame`](Journal::frame), which
/// clamps out-of-range ticks to the window edge (a rewind deeper than the
/// window is served the oldest frame — bounded rewind, never a miss).
/// Crate-internal: games read through the handle only.
pub(crate) struct Journal<T> {
    /// `(tick label, frame)` oldest-first; labels strictly increase.
    frames: VecDeque<(u32, Vec<(Id, T)>)>,
    cap: usize,
}

impl<T> Journal<T> {
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
                    "[netdbg journal] rewind to {tick} clamped to window start {oldest} ({} ticks past the back; {n} clamps so far)",
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
        let mut ring = Journal::new(4);
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
        let mut ring = Journal::new(8);
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

}
