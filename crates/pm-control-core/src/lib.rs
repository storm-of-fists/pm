//! pm-control-core — Rust port of the sweeper (cicada.xml) signals/network core.
//!
//! `no_std` + `alloc`: the framework needs a heap (Rc, String, Vec) but no OS.
//! Anything that touches an OS service (files, sockets, RTC) stays out of the
//! core and comes in through host-implemented traits.
//!
//! The core is what a micro needs: signals, groups, timers, faults, clock,
//! registration, and the network protocol — plus the tool-side pieces that
//! ride it with no OS deps (`monitor`, `fault_table`, `record`). The
//! machine-side save-file/blackbox and login are host concerns and live in
//! the `pm-control-host` crate as they get ported.
//! The port tracks ST *semantics*, not bytes — simplify where Rust allows.
//!
//! Done: signal core, `pm_group!` (`@skip`/`@flat`/`@arr`), timers, faults,
//! clock (scan ms + host-installed fine µs), registration, NetworkManager
//! (`net`, incl. `NetHealth` self-monitoring), the I/O layer (`io`), the
//! fault table (`fault_table`), recording & playback over any signals —
//! local or a Monitor's shadows (`record`), the save set (`save`), the
//! drop-in profiler (`prof`), loopback demo
//! (`pm-control-host/examples/loopback.rs`).
//! What's next lives in ROADMAP.md.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

pub mod clock;
pub mod fault;
pub mod fault_table;
pub mod io;
pub mod monitor;
pub mod net;
pub mod params;
pub mod prof;
pub mod record;
pub mod save;
pub mod signal;
pub mod timers;

pub use fault::PmFault;
pub use fault_table::{FaultRecord, FaultTable, Notifier};
pub use io::{
    Diag, InDig, InDigChannel, InDigCfg, InRes, InResChannel, InResCfg, InVolt, InVoltChannel,
    InVoltCfg, Joystick, OutCur, OutCurChannel, OutCurCfg, OutDiag, OutDiagIn, OutDig,
    OutDigChannel, OutDigCfg, PdmRelay, PhysInfo, PowerSupply, ScalingGroup, Slew,
};
pub use monitor::{MonFault, MonNode, MonUnlock, Monitor};
pub use net::{Message, NetHealth, NetStatus, NetworkManager, SegmentPort};
pub use prof::{PmProf, ProfSpan};
pub use record::{Playback, Recording, SNAPSHOT_DELAY_MS, SnapshotTrigger};
pub use params::{ParamSpec, Tunable};
pub use save::SaveSet;
pub use signal::{
    AnySignal, PmBool, PmF32, PmI32, PmI64, PmSignal, PmString, PmU64, RCursor, Register,
    Registrar, Stamp, Value, WCursor, WireType, wire_value_from_text, wire_value_to_text,
};
pub use timers::{PmBlinker, PmEdge, PmPulse, PmTimer};

/// Implementation detail of `pm_group!`'s `@arr` marker.
#[doc(hidden)]
pub use alloc::format as __format;
#[doc(hidden)]
pub use alloc::string::String as __String;
#[doc(hidden)]
pub use alloc::vec::Vec as __Vec;

/// Declare a struct of signals (or nested groups) that knows how to register
/// itself. Field names become the dotted-path segments — the Rust replacement
/// for `{attribute 'instance-path'}`. Nesting composes: any field type that
/// implements `Register` works, including other `pm_group!` structs.
///
/// The `= expr` initializer is optional — fields without configuration fall
/// back to `Default::default()`. Field markers:
/// * `@skip` — not a signal (plain config, scratch state); stays out of
///   registration.
/// * `@flat` — register the field's signals without a path segment (how
///   the timer helpers surface `rise`/`fall` beside their `out`).
/// * `@arr` — a `[T; N]` of registrables, named `field0..fieldN-1` (the
///   `ScalingGroup` calibration points).
///
/// The framework's own composites (timers, faults-carrying io blocks,
/// `PmProf`, `NetHealth`) are all declared with this macro; the only
/// hand-written `Register` impls left are the signal leaves themselves —
/// and dynamically-sized sets (a `Vec` of signals), where a manual impl
/// over `Registrar::child` is the documented escape hatch.
///
/// ```
/// use pm_control_core::*;
///
/// pm_group! {
///     pub struct RockerSw {
///         pub sw: PmI32 = PmI32::new().text_list("CanRockerSwitch"),
///         pub mismatch_flt: PmFault = PmFault::new().on_delay(500),
///         pub invalid_flt: PmFault = PmFault::new().on_delay(500),
///         @skip pub detect_ms: u32 = 100,
///     }
/// }
///
/// pm_group! {
///     pub struct Consoles {
///         pub sb_l_raise_pres_sw: RockerSw,
///         pub sb_r_raise_pres_sw: RockerSw,
///     }
/// }
///
/// let root = Consoles::new();
/// let reg = Registrar::collect(&root);
/// assert_eq!(reg.signals[0].meta().name(), "sb_l_raise_pres_sw.sw");
/// ```
#[macro_export]
macro_rules! pm_group {
    (
        $(#[$m:meta])*
        $vis:vis struct $Name:ident {
            $( $(#[$fm:meta])* $(@$skip:ident)? $fvis:vis $field:ident : $Ty:ty $(= $init:expr)? ),+ $(,)?
        }
    ) => {
        $(#[$m])*
        $vis struct $Name {
            $( $(#[$fm])* $fvis $field : $Ty ),+
        }

        impl $Name {
            $vis fn new() -> Self {
                Self { $( $field: $crate::pm_group!(@init $($init)?) ),+ }
            }
        }

        impl Default for $Name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl $crate::signal::Register for $Name {
            fn register(&self, r: &mut $crate::signal::Registrar) {
                $( $crate::pm_group!(@reg r, &self.$field, stringify!($field) $(, @$skip)?); )+
            }
        }
    };
    (@init $init:expr) => { $init };
    (@init) => { ::core::default::Default::default() };
    (@reg $r:ident, $f:expr, $name:expr) => {
        $r.child($name, $f);
    };
    (@reg $r:ident, $f:expr, $name:expr, @skip) => {};
    (@reg $r:ident, $f:expr, $name:expr, @flat) => {
        $crate::signal::Register::register($f, $r);
    };
    (@reg $r:ident, $f:expr, $name:expr, @arr) => {
        for (i, s) in $f.iter().enumerate() {
            $r.child(&$crate::__format!("{}{}", $name, i), s);
        }
    };
}

#[cfg(test)]
mod tests {
    use crate as pm_control_core;
    use pm_control_core::*;

    pm_group! {
        struct RockerSw {
            sw: PmI32 = PmI32::new().text_list("CanRockerSwitch"),
            mismatch_flt: PmFault = PmFault::new().on_delay(500),
        }
    }

    pm_group! {
        struct App {
            sb_l_rot_sw: RockerSw,
            sweep_rot_spd_frac: PmF32 = PmF32::new().range(0.0, 1.0).save(),
            machine_serial: PmString = PmString::new().save(),
        }
    }

    #[test]
    fn names_derive_from_field_paths() {
        let app = App::new();
        let reg = Registrar::collect(&app);
        let names: Vec<String> = reg.signals.iter().map(|s| s.meta().name()).collect();
        assert_eq!(
            names,
            vec![
                "sb_l_rot_sw.sw",
                "sb_l_rot_sw.mismatch_flt",
                "sweep_rot_spd_frac",
                "machine_serial",
            ]
        );
        assert_eq!(reg.faults.len(), 1);
        assert_eq!(reg.faults[0].meta().name(), "sb_l_rot_sw.mismatch_flt");
    }

    #[test]
    fn handles_share_state_with_registry() {
        let app = App::new();
        let reg = Registrar::collect(&app);
        app.sweep_rot_spd_frac.set(0.7);
        let mut s = String::new();
        reg.signals[2].value_to_text(&mut s);
        assert_eq!(s, "0.700"); // csv_write_lreal(_, 3) parity
    }

    #[test]
    fn clamp_and_lock_semantics() {
        let app = App::new();
        app.sweep_rot_spd_frac.set(7.0);
        assert_eq!(app.sweep_rot_spd_frac.val(), 1.0); // clamped hi
        app.sweep_rot_spd_frac.meta().locked.set(false);
        app.sweep_rot_spd_frac.set(0.2);
        assert_eq!(app.sweep_rot_spd_frac.val(), 1.0); // unlocked → app write blocked
    }

    #[test]
    fn bytes_roundtrip_le() {
        let a = App::new();
        let reg = Registrar::collect(&a);
        a.sb_l_rot_sw.sw.set(2);
        a.sweep_rot_spd_frac.set(0.5);
        a.machine_serial.set("SN-04512");

        let mut buf = [0u8; 256];
        let mut w = WCursor::new(&mut buf);
        for s in &reg.signals {
            s.value_to_bytes(&mut w);
        }
        assert!(!w.ovf);
        let len = w.off;
        assert_eq!(len, 4 + 1 + 4 + 31);

        let b = App::new();
        let reg_b = Registrar::collect(&b);
        let mut r = RCursor::new(&buf[..len]);
        for s in &reg_b.signals {
            assert!(s.value_from_bytes(&mut r));
        }
        assert_eq!(b.sb_l_rot_sw.sw.val(), 2);
        assert_eq!(b.sweep_rot_spd_frac.val(), 0.5);
        assert_eq!(b.machine_serial.val(), "SN-04512");
    }

    pm_group! {
        struct Mixed {
            req: PmBool,
            debounce: PmTimer = PmTimer::new().on_delay(100),
            flash: PmBlinker = PmBlinker::new().period(700),
        }
    }

    #[test]
    fn helper_state_registers_for_recording() {
        let mut m = Mixed::new();
        let reg = Registrar::collect(&m);
        let names: Vec<String> = reg.signals.iter().map(|s| s.meta().name()).collect();
        assert_eq!(
            names,
            vec![
                "req",
                "debounce.out",
                "debounce.rise",
                "debounce.fall",
                "debounce.dur",
                "flash.out",
                "flash.rise",
                "flash.fall",
            ]
        );
        crate::clock::set(0);
        m.debounce.update(true);
        crate::clock::set(100);
        assert!(m.debounce.update(true)); // still driven via &mut as usual
        let mut s = String::new();
        reg.signals[1].value_to_text(&mut s);
        assert_eq!(s, "1"); // and the registry sees the timer's state
    }

    #[test]
    fn wire_value_text_roundtrip() {
        let v = wire_value_from_text(WireType::F32, "1.5").unwrap();
        assert_eq!(v.len(), WireType::F32.byte_size());
        let mut s = String::new();
        wire_value_to_text(WireType::F32, &v, &mut s);
        assert_eq!(s, "1.500");
        assert_eq!(wire_value_from_text(WireType::Bool, "1").unwrap(), vec![1]);
        assert_eq!(wire_value_from_text(WireType::Bool, "0").unwrap(), vec![0]);
        assert!(wire_value_from_text(WireType::I32, "fast").is_none()); // no parse
        let v = wire_value_from_text(WireType::Str, "SN-04512").unwrap();
        assert_eq!(v.len(), WireType::Str.byte_size()); // full field, NUL-padded
        let mut s = String::new();
        wire_value_to_text(WireType::Str, &v, &mut s);
        assert_eq!(s, "SN-04512");
    }

    #[test]
    fn every_write_path_stamps_last_write_ms() {
        clock::set(100);
        let app = App::new();
        let sig = &app.sweep_rot_spd_frac;
        assert_eq!(sig.meta().last_write_ms.get(), 0); // never written
        sig.set(0.5);
        assert_eq!(sig.meta().last_write_ms.get(), 100); // app set
        clock::set(200);
        sig.set_raw(0.6);
        assert_eq!(sig.meta().last_write_ms.get(), 200); // raw write
        let reg = Registrar::collect(&app);
        clock::set(300);
        reg.signals[2].value_from_text("0.25");
        assert_eq!(sig.meta().last_write_ms.get(), 300); // save-file load
        clock::set(400);
        let v = wire_value_from_text(WireType::F32, "0.75").unwrap();
        reg.signals[2].value_from_bytes(&mut RCursor::new(&v));
        assert_eq!(sig.meta().last_write_ms.get(), 400); // net apply
        clock::set(500);
        sig.meta().locked.set(false);
        sig.set(0.1); // blocked while overridden — must NOT stamp
        assert_eq!(sig.meta().last_write_ms.get(), 400);
        clock::set(600);
        app.machine_serial.set("X"); // PmString paths stamp too
        assert_eq!(app.machine_serial.meta().last_write_ms.get(), 600);
    }

    #[test]
    fn string_wire_field_is_nul_terminated() {
        let s = PmString::new();
        s.set("0123456789012345678901234567890123456789"); // 40 chars
        let mut buf = [0u8; 31];
        let mut w = WCursor::new(&mut buf);
        s.0.value_to_bytes(&mut w);
        assert_eq!(buf[30], 0); // terminator always present
        let t = PmString::new();
        let mut r = RCursor::new(&buf);
        t.0.value_from_bytes(&mut r);
        assert_eq!(t.val().len(), 30); // capped, no stale tail possible
    }

    #[test]
    fn short_datagram_does_not_apply() {
        let d = PmI32::new().init(42);
        let buf = [1u8, 2]; // 2 bytes where 4 are needed
        let mut r = RCursor::new(&buf);
        assert!(!d.0.value_from_bytes(&mut r));
        assert_eq!(d.val(), 42); // untouched, like the ST guard
    }
}
