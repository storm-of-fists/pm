//! PmFault — a PmBool whose app-facing `set()` runs the raw condition
//! through an on/off debounce PmTimer, with optional latch-on. Exact ST
//! semantics (PmFault.val SET):
//!
//! ```text
//! debounce(in := val);
//! IF locked THEN
//!     IF latch THEN IF debounce.out THEN _value := TRUE; END_IF
//!     ELSE          _value := debounce.out; END_IF
//! END_IF
//! edge(in := _value);
//! ```
//!
//! Note the two ST quirks kept on purpose (they're load-bearing for parity):
//! the debounce timer advances even while unlocked, and the edge tracker
//! follows the *stored* value — so a remote override produces edges too.

use crate::signal::{AnySignal, PmBool, Register, Registrar};
use crate::timers::{PmEdge, PmTimer};
use alloc::rc::Rc;
use core::cell::{Cell, RefCell};

pub(crate) struct PmFaultInner {
    pub sig: PmBool,
    pub debounce: RefCell<PmTimer>,
    pub latch: Cell<bool>,
    pub edge: RefCell<PmEdge>,
    /// Human text for the fault table (ST left this as a display-side
    /// 'TODO'); io channels feed it from their `PhysInfo`.
    pub description: RefCell<alloc::string::String>,
}

#[derive(Clone)]
pub struct PmFault(pub(crate) Rc<PmFaultInner>);

impl PmFault {
    pub fn new() -> Self {
        let f = PmFault(Rc::new(PmFaultInner {
            sig: PmBool::new(),
            debounce: RefCell::new(PmTimer::new()), // 0ms: safest default, opt-in delays
            latch: Cell::new(false),
            edge: RefCell::new(PmEdge::default()),
            description: RefCell::new(alloc::string::String::new()),
        }));
        // Advertised as WireType::Fault so segment tools (Monitor) can build
        // fault tables from the schema alone.
        f.0.sig.meta().fault.set(true);
        f
    }
    pub fn describe(self, d: &str) -> Self {
        use alloc::string::ToString;
        *self.0.description.borrow_mut() = d.to_string();
        self
    }
    pub fn on_delay(self, ms: u64) -> Self {
        self.0.debounce.borrow_mut().on_ms = ms;
        self
    }
    pub fn off_delay(self, ms: u64) -> Self {
        self.0.debounce.borrow_mut().off_ms = ms;
        self
    }
    pub fn latch(self) -> Self {
        self.0.latch.set(true);
        self
    }

    /// Feed the raw condition. Call once per scan, like the ST setter.
    pub fn set(&self, cond: bool) {
        let out = self.0.debounce.borrow_mut().update(cond);
        if self.0.sig.meta().locked.get() {
            if self.0.latch.get() {
                if out {
                    self.0.sig.set_raw(true);
                }
            } else {
                self.0.sig.set_raw(out);
            }
        }
        self.0.edge.borrow_mut().update(self.0.sig.val());
    }

    pub fn val(&self) -> bool {
        self.0.sig.val()
    }
    pub fn rise(&self) -> bool {
        self.0.edge.borrow().rise.val()
    }
    pub fn fall(&self) -> bool {
        self.0.edge.borrow().fall.val()
    }
    /// Clear a latched fault (local path; the network clear protocol is
    /// NetworkManager's job in chunk 2).
    pub fn clear(&self) {
        if self.0.sig.meta().locked.get() {
            self.0.sig.set_raw(false);
            self.0.edge.borrow_mut().update(false);
        }
    }
    pub fn meta(&self) -> &crate::signal::Meta {
        self.0.sig.meta()
    }
    pub fn description(&self) -> alloc::string::String {
        self.0.description.borrow().clone()
    }
}

impl Default for PmFault {
    fn default() -> Self {
        Self::new()
    }
}

impl Register for PmFault {
    fn register(&self, r: &mut Registrar) {
        // On the wire and in files a PmFault is just its PmBool…
        r.leaf(self.0.sig.0.clone() as Rc<dyn AnySignal>);
        // …and additionally lands in the fault table (SignalManager, chunk 3).
        r.fault(self);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock;

    #[test]
    fn debounce_on_delay() {
        clock::set(0);
        let f = PmFault::new().on_delay(500);
        f.set(true);
        assert!(!f.val());
        clock::set(499);
        f.set(true);
        assert!(!f.val());
        clock::set(500);
        f.set(true);
        assert!(f.val());
        assert!(f.rise());
        f.set(false);
        assert!(!f.val()); // off_ms = 0 → drops immediately
        assert!(f.fall());
    }

    #[test]
    fn latch_holds_until_cleared() {
        clock::set(0);
        let f = PmFault::new().latch();
        f.set(true);
        assert!(f.val());
        f.set(false);
        assert!(f.val()); // latched
        f.clear();
        assert!(!f.val());
    }

    #[test]
    fn unlocked_blocks_local_eval_but_timer_runs() {
        clock::set(0);
        let f = PmFault::new().on_delay(100);
        f.meta().locked.set(false);
        f.set(true);
        clock::set(100);
        f.set(true);
        assert!(!f.val()); // remote owns the value; local eval gated
        f.meta().locked.set(true);
        f.set(true); // timer already expired while unlocked → applies now
        assert!(f.val());
    }
}
