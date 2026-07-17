//! PmProf — a drop-in section profiler whose readout is signals.
//!
//! Declare one as a `pm_group!` field, wrap the code you care about, and
//! the timing publishes/records/plays back like any other signal — pin
//! `scan.avg_us` in pm-mon and watch how fast things run, live:
//!
//! ```
//! use pm_control_core::*;
//!
//! pm_group! {
//!     struct App {
//!         scan: PmProf,
//!     }
//! }
//!
//! let app = App::new();
//! {
//!     let _t = app.scan.measure(); // records on drop
//!     // ... the scan body ...
//! }
//! assert_eq!(app.scan.last_us.val(), 0); // no fine clock installed here
//! ```
//!
//! Timebase: [`clock::now_us`] — the host-installed free-running fine
//! clock, NOT the per-scan ms clock (which is frozen inside a scan and
//! couldn't time anything within one). Until a host installs it
//! (`pm_control_host::install_us_clock()`, or a cycle-counter fn on a
//! micro), every measurement reads 0 — profs are inert, never wrong.
//!
//! `avg_us` is an EWMA (1/32 per sample, seeded by the first sample), so
//! it tracks drift within a couple of seconds at typical scan rates.
//! `max_us` is the high-water mark since boot; a tool holding an unlock
//! can write it back to 0 to re-arm it (it's just a signal).

use crate::clock;
use crate::pm_group;
use crate::signal::{PmF32, PmU64};

pm_group! {
    /// Section timing as signals: last sample, EWMA average, and the
    /// high-water mark, all in microseconds.
    #[derive(Clone)]
    pub struct PmProf {
        pub last_us: PmU64,
        pub avg_us: PmF32,
        pub max_us: PmU64,
    }
}

impl PmProf {
    /// Record one sample directly (when the caller already timed the
    /// section — a host thread, an interrupt, a foreign timestamp).
    pub fn record_us(&self, us: u64) {
        self.last_us.set(us);
        if us > self.max_us.val() {
            self.max_us.set(us);
        }
        // Seed the EWMA with the first sample ("never written" per the
        // freshness field) so the average doesn't crawl up from zero.
        let avg = if self.avg_us.meta().last_write_ms.get() == 0 {
            us as f32
        } else {
            self.avg_us.val() + (us as f32 - self.avg_us.val()) / 32.0
        };
        self.avg_us.set(avg);
    }

    /// Time a section: the guard records on drop. Shared handle semantics
    /// mean this needs no `&mut` — drop a prof anywhere and measure.
    #[must_use = "the span records when dropped — bind it to a variable"]
    pub fn measure(&self) -> ProfSpan<'_> {
        ProfSpan { prof: self, t0_us: clock::now_us() }
    }
}

/// A running measurement from [`PmProf::measure`]; records on drop.
pub struct ProfSpan<'a> {
    prof: &'a PmProf,
    t0_us: u64,
}

impl Drop for ProfSpan<'_> {
    fn drop(&mut self) {
        self.prof.record_us(clock::now_us().saturating_sub(self.t0_us));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signal::Registrar;
    use std::cell::Cell;

    thread_local! {
        static FAKE_US: Cell<u64> = const { Cell::new(0) };
    }

    fn fake_us() -> u64 {
        FAKE_US.with(|c| c.get())
    }

    #[test]
    fn records_last_max_and_seeded_ewma() {
        crate::clock::set(1_000); // t=0 writes read as "never written"
        let p = PmProf::new();
        p.record_us(100);
        assert_eq!(p.last_us.val(), 100);
        assert_eq!(p.max_us.val(), 100);
        assert_eq!(p.avg_us.val(), 100.0); // first sample seeds the average

        p.record_us(36);
        assert_eq!(p.last_us.val(), 36);
        assert_eq!(p.max_us.val(), 100); // high-water mark holds
        assert_eq!(p.avg_us.val(), 98.0); // 100 + (36-100)/32
    }

    #[test]
    fn measure_spans_the_installed_fine_clock() {
        crate::clock::set(1_000);
        crate::clock::install_us(fake_us);
        let p = PmProf::new();
        FAKE_US.with(|c| c.set(1_000));
        let span = p.measure();
        FAKE_US.with(|c| c.set(1_750));
        drop(span);
        assert_eq!(p.last_us.val(), 750);
        assert_eq!(p.avg_us.val(), 750.0);
    }

    #[test]
    fn registers_three_signals_under_the_field_path() {
        pm_group! {
            struct App {
                scan: PmProf,
            }
        }
        let reg = Registrar::collect(&App::new());
        let names: Vec<String> = reg.signals.iter().map(|s| s.meta().name()).collect();
        assert_eq!(names, vec!["scan.last_us", "scan.avg_us", "scan.max_us"]);
    }
}
