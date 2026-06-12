//! Drop-in profiling probes for timing arbitrary scopes inside tasks:
//!
//! ```ignore
//! let _p = pm::probe::scope("collision.broadphase");
//! // ... work ...
//! // recorded when _p drops
//! ```
//!
//! The registry is thread-local — the kernel is single-threaded, so each
//! thread (each world) profiles itself. Read with `stats()`, clear with
//! `reset()`. For whole-task timings, use `Pm::task_stats` instead; probes
//! are for the hot spots *inside* a task.

use std::cell::RefCell;
use std::collections::HashMap;
use std::time::Instant;

#[derive(Default, Clone, Debug)]
pub struct ProbeStat {
    pub calls: u64,
    pub ns_total: u64,
    pub ns_max: u64,
}

thread_local! {
    static PROBES: RefCell<HashMap<&'static str, ProbeStat>> = RefCell::new(HashMap::new());
}

pub struct Scope {
    name: &'static str,
    start: Instant,
}

/// Start timing; records into the thread's registry when dropped.
pub fn scope(name: &'static str) -> Scope {
    Scope { name, start: Instant::now() }
}

impl Drop for Scope {
    fn drop(&mut self) {
        let ns = self.start.elapsed().as_nanos() as u64;
        PROBES.with(|p| {
            let mut p = p.borrow_mut();
            let s = p.entry(self.name).or_default();
            s.calls += 1;
            s.ns_total += ns;
            s.ns_max = s.ns_max.max(ns);
        });
    }
}

/// All probes recorded on this thread, heaviest first.
pub fn stats() -> Vec<(&'static str, ProbeStat)> {
    PROBES.with(|p| {
        let mut v: Vec<_> = p.borrow().iter().map(|(&n, s)| (n, s.clone())).collect();
        v.sort_by_key(|(_, s)| std::cmp::Reverse(s.ns_total));
        v
    })
}

pub fn reset() {
    PROBES.with(|p| p.borrow_mut().clear());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scopes_accumulate_and_reset() {
        reset();
        for _ in 0..3 {
            let _p = scope("test.span");
            std::hint::black_box((0..100).sum::<u64>());
        }
        let stats = stats();
        let (name, s) = &stats[0];
        assert_eq!(*name, "test.span");
        assert_eq!(s.calls, 3);
        assert!(s.ns_total >= s.ns_max);
        reset();
        assert!(super::stats().is_empty());
    }
}
