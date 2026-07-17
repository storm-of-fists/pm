//! Ambient cycle clock — the Rust stand-in for the PLC runtime's TIME().
//!
//! One piece of global state, on purpose: signals (PmFault debounce) need time
//! without threading a clock through every `set()` call, exactly like ST.
//! The runtime advances it once per scan; tests set it directly.
//!
//! Beside it lives the ambient **fine clock**, `now_us()` — free-running
//! microseconds for profiling ([`PmProf`](crate::PmProf)). The ms clock is
//! deliberately frozen for the duration of a scan, so it can't time anything
//! *inside* one; the fine clock is host-installed (`install_us`; std hosts
//! wrap `Instant`, micros wrap a cycle counter) and reads 0 until then —
//! profs on a host without one just read zero.
//!
//! In the shipped (`no_std`) build these are process-wide `Cell`s that
//! claim `Sync` — sound only under the single-threaded scan contract below.
//! (An `AtomicU64` would dodge the unsafe but doesn't exist on Cortex-M4
//! class targets; `thread_local!` doesn't exist in core at all.) The test
//! build keeps real thread-locals so `cargo test`'s parallel test threads
//! can each own their own clocks without stomping one another.

#[cfg(not(test))]
mod imp {
    use core::cell::Cell;

    struct ScanClock<T>(Cell<T>);

    // SAFETY: the framework's scan model is single-threaded, like the PLC
    // runtime it ports — every signal is an un-Sync `Rc` already, so no
    // framework object can cross threads. Hosts that spawn threads must
    // keep clock access on the scan thread.
    unsafe impl<T> Sync for ScanClock<T> {}

    static NOW_MS: ScanClock<u64> = ScanClock(Cell::new(0));
    static NOW_US: ScanClock<Option<fn() -> u64>> = ScanClock(Cell::new(None));

    pub fn now_ms() -> u64 {
        NOW_MS.0.get()
    }
    pub fn advance(dt_ms: u64) {
        NOW_MS.0.set(NOW_MS.0.get() + dt_ms);
    }
    pub fn set(t_ms: u64) {
        NOW_MS.0.set(t_ms);
    }
    pub fn now_us() -> u64 {
        NOW_US.0.get().map_or(0, |f| f())
    }
    pub fn install_us(f: fn() -> u64) {
        NOW_US.0.set(Some(f));
    }
}

/// Current cycle time in milliseconds. Frozen for the duration of a scan.
pub use imp::now_ms;
/// Advance the clock by `dt_ms`. Call exactly once per scan, at the top.
pub use imp::advance;
/// Set the clock absolutely (tests, playback).
pub use imp::set;
/// Free-running microseconds from the host-installed fine clock (0 until
/// [`install_us`] is called) — the profiling timebase, NOT scan time.
pub use imp::now_us;
/// Install the fine clock: std hosts wrap `Instant`, micros a cycle counter.
pub use imp::install_us;

#[cfg(test)]
mod imp {
    use core::cell::Cell;

    thread_local! {
        static NOW_MS: Cell<u64> = const { Cell::new(0) };
        static NOW_US: Cell<Option<fn() -> u64>> = const { Cell::new(None) };
    }

    pub fn now_ms() -> u64 {
        NOW_MS.with(|c| c.get())
    }
    pub fn advance(dt_ms: u64) {
        NOW_MS.with(|c| c.set(c.get() + dt_ms));
    }
    pub fn set(t_ms: u64) {
        NOW_MS.with(|c| c.set(t_ms));
    }
    pub fn now_us() -> u64 {
        NOW_US.with(|c| c.get()).map_or(0, |f| f())
    }
    pub fn install_us(f: fn() -> u64) {
        NOW_US.with(|c| c.set(Some(f)));
    }
}
