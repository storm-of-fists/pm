//! Params — declared tuning scalars: one [`pm_params!`](crate::pm_params)
//! line per param gives it a name (the field ident), a shipped default, a
//! live range, and a doc comment, and the macro generates BOTH worlds a
//! param lives in:
//!
//! * **the pod** — a plain `repr(C)` struct of `f32`s (bytemuck-derived,
//!   wire-ready for a replicated single) whose `Default` IS the declared
//!   defaults, with clamped indexed writes (`set_clamped` — the
//!   authority's clamp-of-record as generated code) and save-set text in
//!   the platform line shape (`name=value [lo..hi]`, the same format
//!   [`SaveSet`](crate::SaveSet) composes);
//! * **the knobs** — a companion struct of ranged [`PmF32`](crate::PmF32)
//!   signals (one per param, named by the same field idents, plus the
//!   `save` button), with `Register` for the telemetry tree, `seed` from
//!   a pod, and `drain_changes` back out of the dials — the whole
//!   monitor-side bridge.
//!
//! The declaration is the single source of truth: there is no spec table
//! to keep aligned with a struct, no pairing test, and nothing indexed by
//! convention — names are field idents, indices are declaration order on
//! both sides by construction.
//!
//! The generated pod references `::bytemuck` (derive + `Pod`/`Zeroable`),
//! so the declaring crate must depend on `bytemuck` with the `derive`
//! feature — the same rule as every derive that expands to foreign paths.
//! File I/O stays out (this crate is `no_std`): games wrap
//! `to_save_text`/`apply_save_text` with their `std::fs` pair.

/// One param's contract: its wire/file name (the field ident), shipped
/// default, and live range. Rows of the generated `SPECS` table —
/// monitors, codecs, and clamps all read this, never a hand-kept copy.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ParamSpec {
    pub name: &'static str,
    pub default: f32,
    pub min: f32,
    pub max: f32,
}

/// The generic face of a `pm_params!` pod — what an ENGINE needs to
/// host one without knowing the concrete type: clamped indexed writes
/// (the authority's clamp of record), save-set text both ways, and the
/// spec table. `pm_params!` implements it for every pod it generates
/// (forwarding to the inherent items, which stay — direct use reads
/// better in game code). Engine-free on purpose: the trait lives here
/// so pm (or any host) can be generic over "a params pod" while this
/// crate stays dependency-free.
pub trait Tunable: Default {
    fn specs() -> &'static [ParamSpec];
    fn get(&self, idx: usize) -> Option<f32>;
    /// Clamp `v` to the spec range and write field `idx`; true iff the
    /// stored value CHANGED (the write-gate for synced singles).
    fn set_clamped(&mut self, idx: usize, v: f32) -> bool;
    fn to_save_text(&self) -> crate::__String;
    /// Apply `name=value` lines (clamped); returns how many applied.
    fn apply_save_text(&mut self, text: &str) -> usize;
}

/// Declare a params set — see the [module docs](self) for what generates.
///
/// ```
/// use pm_control_core::*;
///
/// pm_params! {
///     /// Game tuning.
///     pub struct Tuning knobs TuningKnobs {
///         /// Top speed, u/s.
///         pub vmax: 18.0 in 5.0..40.0,
///         /// Shots per second.
///         pub fire_rate: 4.0 in 0.5..20.0,
///     }
/// }
///
/// let mut t = Tuning::default();
/// assert_eq!(t.vmax, 18.0);                    // Default = declared
/// assert!(t.set_clamped(0, 99.0));             // clamps to 40.0
/// assert_eq!(t.vmax, 40.0);
///
/// let knobs = TuningKnobs::new();
/// knobs.seed(&t);
/// let mut shadow = t;
/// knobs.fire_rate.set(8.0);
/// assert_eq!(knobs.drain_changes(&mut shadow), vec![(1, 8.0)]);
/// ```
///
/// Grammar per field: `name: default in min..max` (literals; negatives
/// allowed). Don't name a param `save` — the knobs companion adds its
/// save button under that name (the duplicate field is a compile error).
#[macro_export]
macro_rules! pm_params {
    (
        $(#[$m:meta])*
        $vis:vis struct $Pod:ident knobs $Knobs:ident {
            $(
                $(#[$fm:meta])*
                $fvis:vis $field:ident : $default:literal in $lo:literal .. $hi:literal
            ),+ $(,)?
        }
    ) => {
        $(#[$m])*
        #[repr(C)]
        #[derive(Clone, Copy, PartialEq, Debug, ::bytemuck::Pod, ::bytemuck::Zeroable)]
        $vis struct $Pod {
            $( $(#[$fm])* $fvis $field : f32 ),+
        }

        impl Default for $Pod {
            /// The DECLARED defaults — a fresh pod (and a replica single
            /// before its first snapshot) holds the shipped tuning, not
            /// zeros.
            fn default() -> Self {
                Self { $( $field: $default ),+ }
            }
        }

        impl $Pod {
            /// Field-level schema descriptor (name + every field, in
            /// order) — dependency-free here; an engine that hashes pod
            /// schemas into its connect handshake hashes this string
            /// (pm: `impl pm::PodSchema for Pod { const SCHEMA_HASH =
            /// pm::schema_hash_str(Pod::SCHEMA); }` next to the
            /// declaration). Adding/renaming/reordering a param changes
            /// it, so version-skewed ends fail loudly at connect.
            $vis const SCHEMA: &'static str =
                concat!(stringify!($Pod) $(, "|", stringify!($field), ":f32")+);

            /// One [`ParamSpec`]($crate::ParamSpec) per field, in
            /// declaration order — the index space every other surface
            /// (events, knobs, `set_clamped`) shares by construction.
            $vis const SPECS: [$crate::ParamSpec; [$(stringify!($field)),+].len()] = [
                $( $crate::ParamSpec {
                    name: stringify!($field),
                    default: $default,
                    min: $lo,
                    max: $hi,
                } ),+
            ];

            /// Values in declaration order.
            $vis fn values(&self) -> [f32; Self::SPECS.len()] {
                [ $( self.$field ),+ ]
            }

            /// Value at `idx` (`None` past the end).
            $vis fn get(&self, idx: usize) -> Option<f32> {
                self.values().get(idx).copied()
            }

            /// Spec index of `name` (`None` for a stranger).
            $vis fn index_of(name: &str) -> Option<usize> {
                Self::SPECS.iter().position(|s| s.name == name)
            }

            /// Clamp `v` to the spec range and write field `idx`. True
            /// iff the stored value CHANGED — the write-gate for synced
            /// singles; false for an unknown idx or an equal value.
            $vis fn set_clamped(&mut self, idx: usize, v: f32) -> bool {
                let Some(spec) = Self::SPECS.get(idx) else {
                    return false;
                };
                let v = v.clamp(spec.min, spec.max);
                let mut i = 0usize;
                $(
                    if i == idx {
                        if self.$field != v {
                            self.$field = v;
                            return true;
                        }
                        return false;
                    }
                    i += 1;
                )+
                let _ = i;
                false
            }

            /// The whole set as save-set text — the platform line shape
            /// (`name=value [lo..hi]`; the trailing range is a human
            /// aid, never parsed back).
            $vis fn to_save_text(&self) -> $crate::__String {
                let mut out = $crate::__String::new();
                $(
                    out.push_str(&$crate::__format!(
                        "{}={} [{}..{}]\n",
                        stringify!($field),
                        self.$field,
                        $lo,
                        $hi,
                    ));
                )+
                out
            }

            /// Apply save-set text: `name=value ...` per line, `#` and
            /// junk lines skipped, unknown names ignored, every value
            /// CLAMPED (a hand-edited file never loads raw). Returns how
            /// many params applied — compare against expectations if a
            /// silent typo matters to you.
            $vis fn apply_save_text(&mut self, text: &str) -> usize {
                let mut applied = 0;
                for line in text.lines() {
                    let line = line.trim();
                    if line.is_empty() || line.starts_with('#') {
                        continue;
                    }
                    let Some((key, rest)) = line.split_once('=') else {
                        continue;
                    };
                    let value = rest.split(' ').next().unwrap_or(rest).trim();
                    let (Some(idx), Ok(v)) = (Self::index_of(key.trim()), value.parse::<f32>())
                    else {
                        continue;
                    };
                    self.set_clamped(idx, v);
                    applied += 1;
                }
                applied
            }
        }

        /// Monitor-side dials for the params set: one ranged
        /// [`PmF32`]($crate::PmF32) per param (same names), plus the
        impl $crate::Tunable for $Pod {
            fn specs() -> &'static [$crate::ParamSpec] {
                &Self::SPECS
            }
            fn get(&self, idx: usize) -> Option<f32> {
                Self::get(self, idx)
            }
            fn set_clamped(&mut self, idx: usize, v: f32) -> bool {
                Self::set_clamped(self, idx, v)
            }
            fn to_save_text(&self) -> $crate::__String {
                Self::to_save_text(self)
            }
            fn apply_save_text(&mut self, text: &str) -> usize {
                Self::apply_save_text(self, text)
            }
        }

        /// `save` button. Generated by [`pm_params!`]($crate::pm_params).
        $vis struct $Knobs {
            $( $(#[$fm])* $fvis $field : $crate::PmF32, )+
            /// The save button: a monitor write ≥ 0.5 asks the value
            /// owner to persist (edge-detect in the bridge; write 0 to
            /// re-arm).
            $vis save: $crate::PmF32,
        }

        impl $Knobs {
            $vis fn new() -> Self {
                Self {
                    $( $field: $crate::PmF32::new().range($lo, $hi), )+
                    save: $crate::PmF32::new().range(0.0, 1.0),
                }
            }

            /// Seed every knob from the pod — call once with the loaded
            /// set so the dials open showing the truth.
            $vis fn seed(&self, p: &$Pod) {
                $( self.$field.set(p.$field); )+
            }

            /// Diff the dials against `last` (the values already sent):
            /// each moved knob updates `last` and yields `(spec index,
            /// new value)` — feed them to the wire; the value owner
            /// clamps again on arrival.
            $vis fn drain_changes(&self, last: &mut $Pod) -> $crate::__Vec<(u32, f32)> {
                let mut out = $crate::__Vec::new();
                let mut i = 0u32;
                $(
                    {
                        let v = self.$field.val();
                        if v != last.$field {
                            last.$field = v;
                            out.push((i, v));
                        }
                        i += 1;
                    }
                )+
                let _ = i;
                out
            }
        }

        impl Default for $Knobs {
            fn default() -> Self {
                Self::new()
            }
        }

        impl $crate::signal::Register for $Knobs {
            fn register(&self, r: &mut $crate::signal::Registrar) {
                $( r.child(stringify!($field), &self.$field); )+
                r.child("save", &self.save);
            }
        }
    };
}

#[cfg(test)]
mod tests {
    use crate as pm_control_core;
    use pm_control_core::*;

    pm_params! {
        /// Test tuning set.
        struct Tuning knobs TuningKnobs {
            /// Wave size.
            wave: 40.0 in 1.0..1000.0,
            /// A negative-capable knob.
            bias: -2.5 in -10.0..10.0,
            speed: 18.0 in 5.0..40.0,
        }
    }

    #[test]
    fn default_is_the_declared_defaults_and_specs_match() {
        let t = Tuning::default();
        assert_eq!(t.values(), [40.0, -2.5, 18.0]);
        assert_eq!(Tuning::SPECS.len(), 3);
        assert_eq!(Tuning::SPECS[1].name, "bias");
        assert_eq!(Tuning::SPECS[1].min, -10.0);
        assert_eq!(Tuning::index_of("speed"), Some(2));
        assert_eq!(Tuning::index_of("ghost"), None);
    }

    #[test]
    fn set_clamped_clamps_and_write_gates() {
        let mut t = Tuning::default();
        assert!(t.set_clamped(0, 5000.0)); // clamps to 1000
        assert_eq!(t.wave, 1000.0);
        assert!(!t.set_clamped(0, 1000.0)); // equal: gated
        assert!(!t.set_clamped(99, 1.0)); // unknown idx
        assert!(t.set_clamped(1, -99.0)); // clamps to -10
        assert_eq!(t.bias, -10.0);
    }

    #[test]
    fn save_text_roundtrips_clamped_and_skips_junk() {
        let mut a = Tuning::default();
        a.set_clamped(0, 200.0);
        let text = a.to_save_text();
        assert!(text.starts_with("wave=200 [1..1000]\n"));

        let mut b = Tuning::default();
        assert_eq!(b.apply_save_text(&text), 3);
        assert_eq!(b, a);

        // Hand-edited noise: junk skipped, out-of-range clamps.
        let mut c = Tuning::default();
        let n = c.apply_save_text("# hi\nwave=9999 stale\nghost=1\nnot a line\nspeed=oops\n");
        assert_eq!(n, 1);
        assert_eq!(c.wave, 1000.0);
        assert_eq!(c.speed, 18.0);
    }

    #[test]
    fn knobs_register_seed_and_drain() {
        let k = TuningKnobs::new();
        let names: Vec<String> = Registrar::collect(&k)
            .signals
            .iter()
            .map(|s| s.meta().name())
            .collect();
        assert_eq!(names, vec!["wave", "bias", "speed", "save"]);

        let mut t = Tuning::default();
        t.set_clamped(2, 25.0);
        k.seed(&t);
        assert_eq!(k.speed.val(), 25.0);

        let mut shadow = t;
        assert!(k.drain_changes(&mut shadow).is_empty());
        k.wave.set(80.0);
        k.bias.set(1.5);
        assert_eq!(k.drain_changes(&mut shadow), vec![(0, 80.0), (1, 1.5)]);
        assert!(k.drain_changes(&mut shadow).is_empty()); // shadow caught up
        assert_eq!(shadow.wave, 80.0);
    }
}
