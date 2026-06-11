//! Lightweight math primitives (port of pm_math.hpp): Vec2 and a fast
//! xorshift32 Rng.
//!
//! Rust note: C++ operator overloads map to the `std::ops` traits —
//! implementing `Add` gives you `a + b`, `Mul<f32>` gives `v * s`. Vec2
//! is `Pod` so it can sit directly inside replicated components.

use std::ops::{Add, AddAssign, Div, Mul, MulAssign, Neg, Sub, SubAssign};

use bytemuck::{Pod, Zeroable};

#[derive(Clone, Copy, PartialEq, Default, Debug, Pod, Zeroable)]
#[repr(C)]
pub struct Vec2 {
    pub x: f32,
    pub y: f32,
}

pub const fn vec2(x: f32, y: f32) -> Vec2 {
    Vec2 { x, y }
}

impl Vec2 {
    pub const ZERO: Vec2 = vec2(0.0, 0.0);

    pub fn len(self) -> f32 {
        (self.x * self.x + self.y * self.y).sqrt()
    }

    pub fn dist(self, other: Vec2) -> f32 {
        (self - other).len()
    }

    /// Unit vector, or zero when too short to normalize safely.
    pub fn norm(self) -> Vec2 {
        let l = self.len();
        if l > 1e-4 { self * (1.0 / l) } else { Vec2::ZERO }
    }
}

impl Add for Vec2 {
    type Output = Vec2;
    fn add(self, o: Vec2) -> Vec2 {
        vec2(self.x + o.x, self.y + o.y)
    }
}

impl Sub for Vec2 {
    type Output = Vec2;
    fn sub(self, o: Vec2) -> Vec2 {
        vec2(self.x - o.x, self.y - o.y)
    }
}

impl Mul<f32> for Vec2 {
    type Output = Vec2;
    fn mul(self, s: f32) -> Vec2 {
        vec2(self.x * s, self.y * s)
    }
}

impl Mul<Vec2> for f32 {
    type Output = Vec2;
    fn mul(self, v: Vec2) -> Vec2 {
        v * self
    }
}

impl Div<f32> for Vec2 {
    type Output = Vec2;
    fn div(self, s: f32) -> Vec2 {
        vec2(self.x / s, self.y / s)
    }
}

impl Neg for Vec2 {
    type Output = Vec2;
    fn neg(self) -> Vec2 {
        vec2(-self.x, -self.y)
    }
}

impl AddAssign for Vec2 {
    fn add_assign(&mut self, o: Vec2) {
        *self = *self + o;
    }
}

impl SubAssign for Vec2 {
    fn sub_assign(&mut self, o: Vec2) {
        *self = *self - o;
    }
}

impl MulAssign<f32> for Vec2 {
    fn mul_assign(&mut self, s: f32) {
        *self = *self * s;
    }
}

/// xorshift32 PRNG — fast, deterministic, good enough for gameplay.
#[derive(Clone, Debug)]
pub struct Rng {
    state: u32,
}

impl Default for Rng {
    fn default() -> Self {
        Self::new(42)
    }
}

impl Rng {
    pub fn new(seed: u32) -> Self {
        Self { state: if seed == 0 { 1 } else { seed } }
    }

    pub fn next_u32(&mut self) -> u32 {
        self.state ^= self.state << 13;
        self.state ^= self.state >> 17;
        self.state ^= self.state << 5;
        self.state
    }

    /// Uniform float in [0, 1).
    pub fn rf(&mut self) -> f32 {
        (self.next_u32() & 0xFF_FFFF) as f32 / 0x100_0000 as f32
    }

    /// Uniform float in [lo, hi].
    pub fn rfr(&mut self, lo: f32, hi: f32) -> f32 {
        lo + self.rf() * (hi - lo)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vec_ops() {
        let v = vec2(3.0, 4.0);
        assert_eq!(v.len(), 5.0);
        assert_eq!((vec2(1.0, 2.0) + vec2(3.0, 4.0)), vec2(4.0, 6.0));
        assert_eq!(2.0 * vec2(1.0, 2.0), vec2(2.0, 4.0));
        assert!((v.norm().len() - 1.0).abs() < 1e-6);
        assert_eq!(Vec2::ZERO.norm(), Vec2::ZERO);
        let mut a = vec2(1.0, 1.0);
        a += vec2(1.0, 0.0);
        a *= 2.0;
        assert_eq!(a, vec2(4.0, 2.0));
    }

    #[test]
    fn rng_is_deterministic_and_in_range() {
        let mut a = Rng::new(7);
        let mut b = Rng::new(7);
        for _ in 0..1000 {
            let f = a.rf();
            assert!((0.0..1.0).contains(&f));
            assert_eq!(f, b.rf()); // same seed, same stream
            let r = a.rfr(-2.0, 3.0);
            assert!((-2.0..=3.0).contains(&r));
            assert_eq!(r, b.rfr(-2.0, 3.0));
        }
        assert_eq!(Rng::new(0).next_u32(), Rng::new(1).next_u32()); // zero-seed guard
    }
}
