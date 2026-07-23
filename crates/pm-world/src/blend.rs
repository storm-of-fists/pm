//! The pod compiler's runtime half (engine-v2 item 1): `#[pm::pod]`
//! GENERATES each pod's interpolation ([`PodLerp`]), prediction-error
//! metric ([`PodErr`]), and schema hash over these traits — so a synced
//! pod's blend semantics live ON its declaration (a `#[lerp(angle)]`
//! field tag), not in a hand-written lerp a new field can silently
//! miss. The generated methods are plain fn-pointer-compatible:
//! `pm.interp_pool(&truck, Truck::pod_lerp, …)`,
//! `pm.predict_pool(…, Truck::pod_err, …)`.
//!
//! Per-type meaning, chosen to match the hand code they replaced:
//! - floats lerp linearly; err is the absolute difference.
//! - integers and [`Id`]s are IDENTITY, not quantities: lerp takes the
//!   newer sample whole, err is 0 when equal and 1 when not (any
//!   mismatch on a discrete field should trip a prediction correction).
//! - [`Vec2`]/[`Vec3`] go componentwise; err is the component abs-sum.
//! - [`Quat`] lerps short-arc ([`Quat::nlerp`]); err is
//!   `(1 − |dot|) × 8` — zero when aligned, ±q counts as aligned.
//! - [`Body`] is the shared kinematic chunk: fieldwise over the above.
//! - `#[lerp(angle)]` on an f32 field switches that field to
//!   [`lerp_angle`](crate::lerp_angle) / wrapped-difference err — tag
//!   EVERY angular field (the pool-lerp rule that used to be a comment).
//!
//! Arrays lerp elementwise so a pod can carry small fixed tables.

use crate::id::Id;
use crate::math::{Body, Quat, Vec2, Vec3};

/// Field-meaning-aware interpolation between two samples of a pod.
/// Derived by `#[pm::pod]`; implement by hand only for types the derive
/// can't see inside.
pub trait PodLerp: Sized {
    /// The sample a fraction `t` (0..1) of the way from `self` to `b`.
    fn pod_lerp(&self, b: &Self, t: f32) -> Self;
}

/// Field-meaning-aware prediction-error metric between two samples —
/// what reconciliation compares against its tolerance.
pub trait PodErr {
    /// Accumulated per-field divergence; 0 = byte-agreement in spirit.
    fn pod_err(&self, b: &Self) -> f32;
}

impl PodLerp for f32 {
    fn pod_lerp(&self, b: &Self, t: f32) -> Self {
        self + (b - self) * t
    }
}
impl PodErr for f32 {
    fn pod_err(&self, b: &Self) -> f32 {
        (self - b).abs()
    }
}

impl PodLerp for f64 {
    fn pod_lerp(&self, b: &Self, t: f32) -> Self {
        self + (b - self) * t as f64
    }
}
impl PodErr for f64 {
    fn pod_err(&self, b: &Self) -> f32 {
        (self - b).abs() as f32
    }
}

/// Integers are identity, not quantities — never blend them.
macro_rules! identity_impls {
    ($($ty:ty),*) => {$(
        impl PodLerp for $ty {
            fn pod_lerp(&self, b: &Self, _t: f32) -> Self {
                *b
            }
        }
        impl PodErr for $ty {
            fn pod_err(&self, b: &Self) -> f32 {
                if self == b { 0.0 } else { 1.0 }
            }
        }
    )*};
}
identity_impls!(u8, u16, u32, u64, i8, i16, i32, i64, Id);

impl PodLerp for Vec2 {
    fn pod_lerp(&self, b: &Self, t: f32) -> Self {
        Vec2 {
            x: self.x.pod_lerp(&b.x, t),
            y: self.y.pod_lerp(&b.y, t),
        }
    }
}
impl PodErr for Vec2 {
    fn pod_err(&self, b: &Self) -> f32 {
        (self.x - b.x).abs() + (self.y - b.y).abs()
    }
}

impl PodLerp for Vec3 {
    fn pod_lerp(&self, b: &Self, t: f32) -> Self {
        Vec3 {
            x: self.x.pod_lerp(&b.x, t),
            y: self.y.pod_lerp(&b.y, t),
            z: self.z.pod_lerp(&b.z, t),
        }
    }
}
impl PodErr for Vec3 {
    fn pod_err(&self, b: &Self) -> f32 {
        (self.x - b.x).abs() + (self.y - b.y).abs() + (self.z - b.z).abs()
    }
}

impl PodLerp for Quat {
    fn pod_lerp(&self, b: &Self, t: f32) -> Self {
        Quat::nlerp(*self, *b, t)
    }
}
impl PodErr for Quat {
    /// `(1 − |dot|) × 8`: 0 when aligned (±q counts as aligned), and the
    /// ×8 keeps a small attitude error commensurate with position error
    /// in a summed metric — the weight the hand-written `body_err` shipped.
    fn pod_err(&self, b: &Self) -> f32 {
        (1.0 - self.dot(*b).abs()) * 8.0
    }
}

impl PodLerp for Body {
    fn pod_lerp(&self, b: &Self, t: f32) -> Self {
        Body {
            pos: self.pos.pod_lerp(&b.pos, t),
            vel: self.vel.pod_lerp(&b.vel, t),
            rot: self.rot.pod_lerp(&b.rot, t),
        }
    }
}
impl PodErr for Body {
    fn pod_err(&self, b: &Self) -> f32 {
        self.pos.pod_err(&b.pos) + self.vel.pod_err(&b.vel) + self.rot.pod_err(&b.rot)
    }
}

impl<T: PodLerp + Copy, const N: usize> PodLerp for [T; N] {
    fn pod_lerp(&self, b: &Self, t: f32) -> Self {
        std::array::from_fn(|i| self[i].pod_lerp(&b[i], t))
    }
}
impl<T: PodErr, const N: usize> PodErr for [T; N] {
    fn pod_err(&self, b: &Self) -> f32 {
        self.iter().zip(b).map(|(a, b)| a.pod_err(b)).sum()
    }
}

/// FNV-1a over a schema-descriptor string — `#[pm::pod]` feeds it the
/// pod's name + per-field (name, type, wire, lerp-tag) descriptors to
/// mint the pod's `SCHEMA_HASH`. Const so the hash is baked at compile
/// time: two builds agree on a pod's hash iff they agree on everything
/// that gives its bytes meaning. (Wiring these into the connect
/// handshake is the queued half of v2 item 1 — see lib.rs.)
pub const fn schema_hash_str(s: &str) -> u64 {
    let bytes = s.as_bytes();
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    let mut i = 0;
    while i < bytes.len() {
        h ^= bytes[i] as u64;
        h = h.wrapping_mul(0x100_0000_01b3);
        i += 1;
    }
    h
}

/// A pod's schema identity for the connect handshake (v2 item 1, the
/// hash's stage 2): every type registered on the wire — synced pools,
/// singles, the input pod, event pods — carries a `SCHEMA_HASH`, and
/// the handshake compares it per channel alongside name and size.
/// Same-size-different-meaning drift (a reordered field, a changed
/// quantization scale, a retagged lerp) fails the connect loudly
/// instead of silently misparsing.
///
/// `#[pm::pod]` implements this for you from the full field descriptor
/// string — that is the normal path. The default of 0 means "unhashed:
/// name + size only", the pre-hash contract — implement the trait
/// empty (`impl pm::PodSchema for X {}`) only for pods built outside
/// the macro (e.g. `pm_params!` pods hash their generated `SCHEMA`
/// string instead).
pub trait PodSchema {
    const SCHEMA_HASH: u64 = 0;
}

// A bare primitive synced directly (a counter pool, a tag) hashes its
// type name — same-size cross-type drift (u32 vs f32) still fails the
// handshake.
macro_rules! prim_schema {
    ($($t:ty),+) => {$(
        impl PodSchema for $t {
            const SCHEMA_HASH: u64 = schema_hash_str(stringify!($t));
        }
    )+};
}
prim_schema!(u8, u16, u32, u64, i8, i16, i32, i64, f32, f64);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::math::vec3;

    #[test]
    fn body_matches_the_hand_formulas_it_replaced() {
        let a = Body {
            pos: vec3(1.0, 2.0, 3.0),
            vel: vec3(-1.0, 0.5, 0.0),
            rot: Quat::from_yaw(0.4),
        };
        let b = Body {
            pos: vec3(2.0, 0.0, 3.0),
            vel: vec3(0.0, 0.5, 2.0),
            rot: Quat::from_yaw(-0.6),
        };
        // The old body_lerp: linear pos/vel, short-arc nlerp attitude.
        let l = a.pod_lerp(&b, 0.25);
        assert_eq!(l.pos, vec3(1.25, 1.5, 3.0));
        assert_eq!(l.vel, vec3(-0.75, 0.5, 0.5));
        let n = Quat::nlerp(a.rot, b.rot, 0.25);
        assert_eq!(l.rot, n);
        // The old body_err: abs-sums plus the ×8 quat term.
        let e = a.pod_err(&b);
        let expect = 1.0 + 2.0 + 0.0 // pos
            + 1.0 + 0.0 + 2.0 // vel
            + (1.0 - a.rot.dot(b.rot).abs()) * 8.0;
        assert!((e - expect).abs() < 1e-6);
    }

    #[test]
    fn discrete_types_are_identity() {
        let (a, b) = (Id::new(0, 0, 7), Id::new(0, 0, 9));
        assert_eq!(a.pod_lerp(&b, 0.3), b, "ids never blend — newest wins");
        assert_eq!(a.pod_err(&b), 1.0);
        assert_eq!(a.pod_err(&a), 0.0);
        assert_eq!(3u32.pod_lerp(&5, 0.5), 5);
    }

    #[test]
    fn schema_hash_is_stable_and_discriminating() {
        const A: u64 = schema_hash_str("Truck|body:Body|steer:f32");
        const B: u64 = schema_hash_str("Truck|body:Body|steer:f32|heat:f32");
        assert_ne!(A, B, "a new field changes the schema identity");
        assert_eq!(A, schema_hash_str("Truck|body:Body|steer:f32"));
    }
}
