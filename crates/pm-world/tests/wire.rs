//! `#[derive(pm::Wire)]` from the outside — the generated repr's layout
//! and conversion semantics. The sync path itself (WireAdapter pack/apply)
//! is covered in-crate (`src/net_tests.rs`), where the manual-impl seam
//! lives; here we prove the macro generates the same shape.

use pm::Wire;

#[derive(Clone, Copy, PartialEq, Debug, Default, bytemuck::Pod, bytemuck::Zeroable, pm::Wire)]
#[repr(C)]
struct Critter {
    #[wire(i16, scale = 64.0)]
    x: f32,
    #[wire(i16, scale = 64.0)]
    z: f32,
    /// [-pi, pi) at 1e4 fits i16 (max 3.2767).
    #[wire(i16, scale = 10000.0)]
    heading: f32,
    /// Default scale is 1.0 — plain narrowing with rounding.
    #[wire(u8)]
    hp: f32,
    /// No attribute: passes through untouched (must itself be Pod).
    flags: u32,
    tag: [u8; 4],
}

// Unhashed schema identity for the handshake bound (test pod).
impl pm::PodSchema for Critter {}

#[test]
fn repr_is_packed_and_small() {
    // i16 + i16 + i16 + u8 + u32 + [u8; 4] = 15 B, no padding.
    assert_eq!(size_of::<CritterWire>(), 15);
    assert_eq!(align_of::<CritterWire>(), 1);
    assert_eq!(size_of::<Critter>(), 24);
}

#[test]
fn quantization_roundtrip() {
    let c = Critter {
        x: 1.23456,
        z: -47.9,
        heading: -3.14159,
        hp: 3.4,
        flags: 0xdead_beef,
        tag: *b"oink",
    };
    let back = Critter::from_repr(c.to_repr());
    // Quantized fields come back at repr resolution...
    assert_eq!(back.x, (1.23456f32 * 64.0).round() / 64.0);
    assert_eq!(back.z, (-47.9f32 * 64.0).round() / 64.0);
    assert_eq!(back.heading, (-3.14159f32 * 10000.0).round() / 10000.0);
    assert_eq!(back.hp, 3.0); // round(3.4 * 1.0) = 3
    assert!((back.x - c.x).abs() <= 0.5 / 64.0);
    // ...pass-through fields exactly.
    assert_eq!(back.flags, c.flags);
    assert_eq!(back.tag, c.tag);
}

#[test]
fn out_of_range_saturates() {
    // Float→int `as` saturates: overflow clamps, NaN → 0, negatives clamp
    // to zero for unsigned targets.
    let c = Critter {
        x: 1e9,
        z: -1e9,
        heading: f32::NAN,
        hp: -5.0,
        flags: 0,
        tag: [0; 4],
    };
    let back = Critter::from_repr(c.to_repr());
    assert_eq!(back.x, i16::MAX as f32 / 64.0);
    assert_eq!(back.z, i16::MIN as f32 / 64.0);
    assert_eq!(back.heading, 0.0);
    assert_eq!(back.hp, 0.0);
}

/// `#[pm::pod]` expands to `#[repr(C)]` + the standard derive set, and
/// picks up `pm::Wire` from the `#[wire]` field attributes on its own.
#[pm::pod]
pub struct Piglet {
    #[wire(i16, scale = 64.0)]
    pub x: f32,
    pub hp: f32,
}

/// Without `#[wire]` fields it's just the seven-derive boilerplate.
#[pm::pod]
pub struct Score {
    pub points: u32,
}

#[test]
fn pod_attribute_rolls_in_the_contract() {
    // Default, PartialEq, Debug, Clone/Copy all came from the attribute.
    let p = Piglet { x: 1.5, ..Default::default() };
    assert_ne!(p, Piglet::default());
    assert_eq!(format!("{:?}", Score { points: 3 }), "Score { points: 3 }");
    // Pod came too (repr(C), no padding) — bytes_of only compiles/runs on Pod.
    assert_eq!(bytemuck::bytes_of(&Score { points: 3 }).len(), 4);
    // And Wire was auto-derived from the #[wire] field: i16 + f32 repr.
    assert_eq!(size_of::<PigletWire>(), 6);
    let back = Piglet::from_repr(p.to_repr());
    assert_eq!(back.x, 1.5); // 1.5 * 64 is exact
    assert_eq!(back.hp, p.hp);
}

#[test]
fn wire_pool_registers_and_stores_full_precision() {
    // The pool holds the game struct, not the repr — precision is only
    // lost on the wire (proven against the adapter in net_tests).
    let mut pm = pm::Pm::new();
    let pool = pm.wire_pool::<Critter>("critter");
    let id = pm.id_add();
    let c = Critter {
        x: 0.123456789,
        ..Default::default()
    };
    pool.get_mut().add(id, c);
    assert_eq!(pool.get().get(id), Some(&c));
}
