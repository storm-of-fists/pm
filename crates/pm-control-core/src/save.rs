//! Save set — persistence text for the `.save()` (configuration) signals.
//!
//! The ST SignalManager save file, split along the usual OS boundary: this
//! module is the text codec over the registered save set (compose the full
//! file, apply a loaded one); the actual file — open/read/rewrite, the
//! ST `File` FB + SysFile plumbing — is `pm_control_host::SaveFile`.
//!
//! File shape is the ST end_cycle save buffer, one line per signal:
//!
//! ```text
//! name=value metadata\n
//! ```
//!
//! Loading is the ST `load_save`: `name` up to the first `=`, value up to
//! the first space (the metadata trailing it is a human aid, never parsed
//! back), unknown names ignored, and every value lands through the
//! clamped/locked `value_from_text` path — a hand-edited out-of-range
//! value clamps instead of loading raw (the P1-13 fix, same as ever).
//!
//! Inherited ST constraint, flagged: a *string* value stops at the first
//! space too, so saved strings must not contain spaces (ST parsed the
//! same way).

use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write as _;

use crate::signal::{AnySignal, Register, Registrar};

/// The `.save()` signals of one registration pass, in registration order —
/// the ST `saves[]` array built at boot from the `save` flags.
pub struct SaveSet {
    pub signals: Vec<Rc<dyn AnySignal>>,
}

impl SaveSet {
    /// Collect everything under a `Register` root (a `pm_group!` struct)
    /// whose meta carries `.save()`.
    pub fn collect(root: &impl Register) -> SaveSet {
        SaveSet::from_registered(&Registrar::collect(root))
    }

    /// Filter an existing registrar pass down to the save set.
    pub fn from_registered(reg: &Registrar) -> SaveSet {
        SaveSet {
            signals: reg.signals.iter().filter(|s| s.meta().save.get()).cloned().collect(),
        }
    }

    /// Compose the whole save file — the ST end_cycle `sav` buffer:
    /// `name=value metadata` per line, rebuilt from scratch every time.
    pub fn to_text(&self, out: &mut String) {
        for s in &self.signals {
            let _ = write!(out, "{}=", s.meta().name());
            s.value_to_text(out);
            out.push(' ');
            out.push_str(&s.metadata_text());
            out.push('\n');
        }
    }

    /// Apply a save file's text — the ST `load_save`. Returns how many
    /// signals were hydrated (unknown names and unparseable lines are
    /// skipped; short files are normal — new signals just keep defaults).
    pub fn from_text(&self, text: &str) -> usize {
        let mut applied = 0;
        for line in text.lines() {
            let Some((key, rest)) = line.split_once('=') else {
                continue;
            };
            let value = rest.split(' ').next().unwrap_or(rest);
            let Some(s) = self.signals.iter().find(|s| s.meta().name() == key) else {
                continue;
            };
            s.value_from_text(value);
            applied += 1;
        }
        applied
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{PmBool, PmF32, PmI32, PmString, pm_group};

    pm_group! {
        struct App {
            spd_frac: PmF32 = PmF32::new().range(0.0, 1.0).save(),
            mode: PmI32 = PmI32::new().text_list("ModeList").save(),
            serial: PmString = PmString::new().save(),
            lamp_on: PmBool, // not saved: stays out of the set
        }
    }

    #[test]
    fn composes_st_shaped_lines_for_the_save_set_only() {
        let app = App::new();
        app.spd_frac.set(0.5);
        app.mode.set(2);
        app.serial.set("SN-04512");
        let set = SaveSet::collect(&app);
        assert_eq!(set.signals.len(), 3);
        let mut text = String::new();
        set.to_text(&mut text);
        assert_eq!(
            text,
            "spd_frac=0.500 [0.000..1.000]\n\
             mode=2 [ModeList]\n\
             serial=SN-04512 \n"
        );
    }

    #[test]
    fn loads_through_the_clamped_setter_and_skips_junk() {
        let app = App::new();
        let set = SaveSet::collect(&app);
        let applied = set.from_text(
            "spd_frac=7.0 \n\
             mode=3 ModeList\n\
             serial=SN-99 stale metadata here\n\
             ghost=1 \n\
             not a line\n",
        );
        assert_eq!(applied, 3); // ghost + junk skipped
        assert_eq!(app.spd_frac.val(), 1.0); // 7.0 clamped by the range
        assert_eq!(app.mode.val(), 3);
        assert_eq!(app.serial.val(), "SN-99"); // value stops at the space
    }

    #[test]
    fn round_trips_into_a_fresh_group() {
        let a = App::new();
        a.spd_frac.set(0.25);
        a.mode.set(1);
        a.serial.set("X1");
        let mut text = String::new();
        SaveSet::collect(&a).to_text(&mut text);

        let b = App::new();
        assert_eq!(SaveSet::collect(&b).from_text(&text), 3);
        assert_eq!(b.spd_frac.val(), 0.25);
        assert_eq!(b.mode.val(), 1);
        assert_eq!(b.serial.val(), "X1");
    }
}
