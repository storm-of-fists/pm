//! Generational peer-owned entity ids: 32-bit `[peer:8 | gen:8 | index:16]`.
//!
//! Indices recycle through a FIFO free list; each reuse bumps the slot's
//! generation, so stale handles fail the liveness/lookup check. On a
//! networked server, recycling is gated by the kernel's removal log: an
//! index is released only after every peer has acked the removal.
//! Peer 0 = server/single-player.

use std::collections::VecDeque;

use crate::paged::PagedArray;

pub const GEN_BITS: u32 = 8;
pub const INDEX_BITS: u32 = 16;
pub const INDEX_MASK: u32 = (1 << INDEX_BITS) - 1;

#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub struct Id(pub u32);

impl Id {
    #[inline]
    pub fn new(peer: u8, generation: u8, index: u32) -> Self {
        debug_assert!(index <= INDEX_MASK, "index {index} exceeds {INDEX_BITS} bits");
        Self(((peer as u32) << (GEN_BITS + INDEX_BITS)) | ((generation as u32) << INDEX_BITS) | index)
    }

    #[inline]
    pub fn peer(self) -> u8 {
        (self.0 >> (GEN_BITS + INDEX_BITS)) as u8
    }

    #[inline]
    pub fn generation(self) -> u8 {
        (self.0 >> INDEX_BITS) as u8
    }

    #[inline]
    pub fn index(self) -> u32 {
        self.0 & INDEX_MASK
    }

    /// Generation-less storage key `[peer:8 | index:16]` — what sparse
    /// arrays and the wire identify entities by.
    #[inline]
    pub(crate) fn slot(self) -> u32 {
        ((self.peer() as u32) << INDEX_BITS) | self.index()
    }
}

#[derive(Default)]
struct PeerSlots {
    next_index: u32, // high-water mark
    free: VecDeque<u16>,
}

pub struct IdAllocator {
    peers: Vec<PeerSlots>,
    gens: PagedArray<u8>,       // current generation per slot
    occupied: PagedArray<bool>, // slot currently holds a live entity
}

impl Default for IdAllocator {
    fn default() -> Self {
        Self {
            peers: (0..256).map(|_| PeerSlots::default()).collect(),
            gens: PagedArray::new(0),
            occupied: PagedArray::new(false),
        }
    }
}

impl IdAllocator {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add(&mut self, peer: u8) -> Id {
        let slots = &mut self.peers[peer as usize];
        let index = match slots.free.pop_front() {
            Some(i) => u32::from(i),
            None => {
                let i = slots.next_index;
                assert!(i <= INDEX_MASK, "peer {peer} exceeded {} concurrent entities", INDEX_MASK + 1);
                slots.next_index = i + 1;
                i
            }
        };
        let slot = ((peer as u32) << INDEX_BITS) | index;
        let id = Id::new(peer, self.gens.get(slot), index);
        self.occupied.set(slot, true);
        id
    }

    pub fn alive(&self, id: Id) -> bool {
        let slot = id.slot();
        self.occupied.get(slot) && self.gens.get(slot) == id.generation()
    }

    /// Accept a remote id (networking): mark it alive locally and record
    /// its generation. Safe because remote ids live in another peer's
    /// index space — the local allocator never hands them out.
    pub fn sync(&mut self, id: Id) {
        let slot = id.slot();
        self.occupied.set(slot, true);
        self.gens.set(slot, id.generation());
    }

    /// Mark dead and bump the generation so stale handles fail immediately.
    /// Does NOT recycle the index — see `release`.
    pub(crate) fn kill(&mut self, id: Id) {
        let slot = id.slot();
        self.occupied.set(slot, false);
        self.gens.set(slot, id.generation().wrapping_add(1));
    }

    /// Return the index to the free list for reuse. Called by the kernel
    /// when the removal log prunes (i.e. all peers acked the removal).
    pub(crate) fn release(&mut self, id: Id) {
        self.peers[id.peer() as usize].free.push_back(id.index() as u16);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_encoding_round_trips() {
        let id = Id::new(7, 12, 54_321);
        assert_eq!(id.peer(), 7);
        assert_eq!(id.generation(), 12);
        assert_eq!(id.index(), 54_321);
        assert_eq!(id.slot(), (7 << 16) | 54_321);
    }

    #[test]
    fn add_allocates_fresh_indices_per_peer() {
        let mut ids = IdAllocator::new();
        let a = ids.add(0);
        let b = ids.add(0);
        let c = ids.add(3);
        assert_eq!((a.index(), b.index()), (0, 1));
        assert_eq!((c.peer(), c.index()), (3, 0));
        assert!(ids.alive(a) && ids.alive(b) && ids.alive(c));
    }

    #[test]
    fn kill_bumps_generation_and_release_recycles_fifo() {
        let mut ids = IdAllocator::new();
        let a = ids.add(0);
        let b = ids.add(0);
        ids.kill(a);
        assert!(!ids.alive(a));
        ids.release(a);
        ids.kill(b);
        ids.release(b);

        let a2 = ids.add(0); // FIFO: a's index comes back first
        assert_eq!(a2.index(), a.index());
        assert_eq!(a2.generation(), a.generation() + 1);
        assert!(ids.alive(a2));
        assert!(!ids.alive(a), "stale handle must fail the gen check");

        let b2 = ids.add(0);
        assert_eq!(b2.index(), b.index());
    }

    #[test]
    fn killed_but_unreleased_index_is_not_reused() {
        let mut ids = IdAllocator::new();
        let a = ids.add(0);
        ids.kill(a); // no release: removal not yet acked by all peers
        let b = ids.add(0);
        assert_ne!(a.index(), b.index());
    }

    #[test]
    fn sync_accepts_remote_ids() {
        let mut ids = IdAllocator::new();
        let remote = Id::new(0, 5, 99); // server-owned id arriving at a client
        assert!(!ids.alive(remote));
        ids.sync(remote);
        assert!(ids.alive(remote));
        let stale = Id::new(0, 4, 99); // previous occupant of the slot
        assert!(!ids.alive(stale));
    }
}
