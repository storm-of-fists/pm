//! Sparse-set component pool: paged sparse array (slot -> dense index),
//! parallel dense arrays for ids, values, and per-entity change ticks.
//!
//! The sparse array is keyed by `Id::slot()` (peer|index, no generation);
//! lookups verify the full id against the dense array, so stale handles
//! from a recycled slot miss instead of aliasing the new occupant.

use std::ops::{Deref, DerefMut};

use crate::id::Id;
use crate::paged::PagedArray;

const EMPTY: u32 = u32::MAX;

pub struct Pool<T> {
    sparse: PagedArray<u32>,
    ids: Vec<Id>,
    values: Vec<T>,
    changed: Vec<u32>, // kernel tick of last insert/mutation, for sync diffing
    tick: u32,         // current kernel tick, pushed in each loop_once
}

/// Mutable handle to a pool entry. Derefs like `&mut T`, but stamps the
/// entry's changed-tick only on mutable deref — reading through it is free.
pub struct Mut<'a, T> {
    value: &'a mut T,
    changed: &'a mut u32,
    tick: u32,
}

impl<T> Deref for Mut<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        self.value
    }
}

impl<T> DerefMut for Mut<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        *self.changed = self.tick;
        self.value
    }
}

impl<T> Default for Pool<T> {
    fn default() -> Self {
        Self {
            sparse: PagedArray::new(EMPTY),
            ids: Vec::new(),
            values: Vec::new(),
            changed: Vec::new(),
            tick: 0,
        }
    }
}

impl<T> Pool<T> {
    pub fn new() -> Self {
        Self::default()
    }

    fn dense_index(&self, id: Id) -> Option<usize> {
        let idx = self.sparse.get(id.slot());
        // Full-id compare = generation check against the slot's occupant.
        (idx != EMPTY && self.ids[idx as usize] == id).then_some(idx as usize)
    }

    /// Insert `value` for `id`, stamping it changed this tick. Replaces if
    /// the id (or a stale occupant of its slot) is already present — an
    /// add is an upsert, which is what lets the sync layer treat adds and
    /// changes identically.
    pub fn add(&mut self, id: Id, value: T) {
        let idx = self.sparse.get(id.slot());
        if idx != EMPTY {
            let idx = idx as usize;
            self.ids[idx] = id;
            self.values[idx] = value;
            self.changed[idx] = self.tick;
            return;
        }
        self.sparse.set(id.slot(), self.ids.len() as u32);
        self.ids.push(id);
        self.values.push(value);
        self.changed.push(self.tick);
    }

    pub fn get(&self, id: Id) -> Option<&T> {
        self.dense_index(id).map(|i| &self.values[i])
    }

    /// Mutable handle; stamps the changed-tick only if actually written
    /// through.
    pub fn get_mut(&mut self, id: Id) -> Option<Mut<'_, T>> {
        let idx = self.dense_index(id)?;
        Some(Mut {
            value: &mut self.values[idx],
            changed: &mut self.changed[idx],
            tick: self.tick,
        })
    }

    /// Kernel tick at which this entry was last inserted or mutated.
    pub fn changed_tick(&self, id: Id) -> Option<u32> {
        self.dense_index(id).map(|i| self.changed[i])
    }

    /// Entries inserted or mutated after `tick` — the sync layer's delta
    /// query ("changed since this peer's last acked tick").
    pub fn changed_since(&self, tick: u32) -> impl Iterator<Item = (Id, &T)> {
        self.ids
            .iter()
            .copied()
            .zip(self.values.iter())
            .zip(self.changed.iter())
            .filter_map(move |((id, v), &c)| (c > tick).then_some((id, v)))
    }

    pub fn contains(&self, id: Id) -> bool {
        self.dense_index(id).is_some()
    }

    /// Swap-remove. Returns the removed value, or None if absent.
    pub fn remove(&mut self, id: Id) -> Option<T> {
        let idx = self.dense_index(id)?;
        let last = self.ids.len() - 1;
        self.ids.swap_remove(idx);
        self.changed.swap_remove(idx);
        let value = self.values.swap_remove(idx);
        if idx != last {
            self.sparse.set(self.ids[idx].slot(), idx as u32);
        }
        self.sparse.set(id.slot(), EMPTY);
        Some(value)
    }

    pub fn len(&self) -> usize {
        self.ids.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

    pub fn ids(&self) -> &[Id] {
        &self.ids
    }

    pub fn values(&self) -> &[T] {
        &self.values
    }

    /// Change-tick per entry, parallel to `ids()`/`values()` (sync layer).
    pub(crate) fn changed_ticks(&self) -> &[u32] {
        &self.changed
    }

    /// Plain `&mut` access that stamps the changed-tick immediately —
    /// the `Single` handle's mutable path. Prefer `get_mut` (write-gated
    /// stamping) for entity iteration.
    pub(crate) fn get_mut_stamped(&mut self, id: Id) -> Option<&mut T> {
        let idx = self.dense_index(id)?;
        self.changed[idx] = self.tick;
        Some(&mut self.values[idx])
    }

    /// Read-only iteration over (id, value) pairs in dense order.
    pub fn iter(&self) -> impl Iterator<Item = (Id, &T)> {
        self.ids.iter().copied().zip(self.values.iter())
    }

    /// Mutable iteration in dense order. Entries are stamped changed only
    /// when actually written through the `Mut` handle.
    pub fn iter_mut(&mut self) -> impl Iterator<Item = (Id, Mut<'_, T>)> {
        let tick = self.tick;
        self.ids
            .iter()
            .copied()
            .zip(self.values.iter_mut().zip(self.changed.iter_mut()))
            .map(move |(id, (value, changed))| (id, Mut { value, changed, tick }))
    }

    pub fn clear(&mut self) {
        for id in &self.ids {
            self.sparse.set(id.slot(), EMPTY);
        }
        self.ids.clear();
        self.values.clear();
        self.changed.clear();
    }
}

/// Type-erased view used by the kernel to flush deferred id removals and
/// push the tick into every pool without knowing component types.
pub(crate) trait ErasedPool {
    fn erased_remove(&mut self, id: Id);
    fn tick_set(&mut self, tick: u32);
}

impl<T> ErasedPool for Pool<T> {
    fn erased_remove(&mut self, id: Id) {
        self.remove(id);
    }

    fn tick_set(&mut self, tick: u32) {
        self.tick = tick;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(n: u32) -> Id {
        Id::new(0, 0, n)
    }

    #[test]
    fn add_get_remove() {
        let mut pool = Pool::new();
        pool.add(id(5), 50);
        pool.add(id(9000), 90); // forces a second sparse page
        assert_eq!(pool.get(id(5)), Some(&50));
        assert_eq!(pool.get(id(9000)), Some(&90));
        assert_eq!(pool.len(), 2);
        assert_eq!(pool.remove(id(5)), Some(50));
        assert_eq!(pool.get(id(5)), None);
        assert_eq!(pool.len(), 1);
        assert_eq!(pool.remove(id(5)), None);
    }

    #[test]
    fn swap_remove_keeps_sparse_consistent() {
        let mut pool = Pool::new();
        for n in 0..100u32 {
            pool.add(id(n), n * 10);
        }
        pool.remove(id(0)); // last element swaps into slot 0
        for n in 1..100u32 {
            assert_eq!(pool.get(id(n)), Some(&(n * 10)), "id {n} broken after swap");
        }
    }

    #[test]
    fn stale_generation_misses_recycled_slot() {
        let mut pool = Pool::new();
        let old = Id::new(0, 0, 5);
        let new = Id::new(0, 1, 5); // same slot, next generation
        pool.add(old, 10);
        pool.add(new, 20); // upsert replaces the slot's occupant
        assert_eq!(pool.len(), 1);
        assert_eq!(pool.get(new), Some(&20));
        assert_eq!(pool.get(old), None, "stale id must not alias new entity");
        assert_eq!(pool.remove(old), None);
    }

    #[test]
    fn mut_guard_stamps_only_on_write() {
        let mut pool = Pool::new();
        pool.tick_set(5);
        pool.add(id(1), 10);
        assert_eq!(pool.changed_tick(id(1)), Some(5));

        pool.tick_set(6);
        assert_eq!(*pool.get_mut(id(1)).unwrap(), 10); // read through the guard: no stamp
        assert_eq!(pool.changed_tick(id(1)), Some(5));

        *pool.get_mut(id(1)).unwrap() = 11; // write: stamps
        assert_eq!(pool.changed_tick(id(1)), Some(6));

        pool.tick_set(7);
        for (_, p) in pool.iter_mut() {
            let _ = *p; // read-only pass over iter_mut: no stamps
        }
        assert_eq!(pool.changed_tick(id(1)), Some(6));
        for (_, mut p) in pool.iter_mut() {
            *p += 1;
        }
        assert_eq!(pool.changed_tick(id(1)), Some(7));
    }

    #[test]
    fn changed_since_returns_the_delta() {
        let mut pool = Pool::new();
        pool.tick_set(1);
        pool.add(id(1), 10);
        pool.add(id(2), 20);
        pool.tick_set(2);
        pool.add(id(3), 30); // add is a change
        *pool.get_mut(id(1)).unwrap() = 11;

        let delta: Vec<_> = pool.changed_since(1).collect();
        assert_eq!(delta.len(), 2);
        assert!(delta.contains(&(id(1), &11)));
        assert!(delta.contains(&(id(3), &30)));
        assert_eq!(pool.changed_since(2).count(), 0);
        assert_eq!(pool.changed_since(0).count(), 3); // new peer: everything
    }

    #[test]
    fn iter_and_views() {
        let mut pool = Pool::new();
        pool.add(id(1), 1);
        pool.add(id(2), 2);
        let sum: i32 = pool.iter().map(|(_, v)| *v).sum();
        assert_eq!(sum, 3);
        assert_eq!(pool.ids().len(), 2);
        assert_eq!(pool.values().len(), 2);
        pool.clear();
        assert!(pool.is_empty());
        assert_eq!(pool.get(id(1)), None);
    }
}
