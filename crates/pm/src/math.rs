//! Lightweight math primitives: Vec2/Vec3/Mat4 and a fast xorshift32
//! Rng.
//!
//! Rust note: operator overloading is done by implementing the
//! `std::ops` traits — `Add` gives you `a + b`, `Mul<f32>` gives
//! `v * s`. The vector types are `Pod` so they can sit directly inside
//! replicated components.
//!
//! ```
//! use pm::{vec2, Vec2};
//!
//! let a = vec2(3.0, 4.0);
//! assert_eq!(a.len(), 5.0);
//! assert_eq!(a.norm().len(), 1.0);
//!
//! // Operator overloads (commutative scalar mul, component add/sub):
//! let b = a + vec2(1.0, 0.0) - vec2(0.0, 4.0);
//! assert_eq!(b, vec2(4.0, 0.0));
//! assert_eq!(2.0 * vec2(1.0, 1.0), vec2(2.0, 2.0));
//! ```

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
        if l > 1e-4 {
            self * (1.0 / l)
        } else {
            Vec2::ZERO
        }
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
        Self {
            state: if seed == 0 { 1 } else { seed },
        }
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

// --- 3d ------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Default, Debug, Pod, Zeroable)]
#[repr(C)]
pub struct Vec3 {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

pub const fn vec3(x: f32, y: f32, z: f32) -> Vec3 {
    Vec3 { x, y, z }
}

impl Vec3 {
    pub const ZERO: Vec3 = vec3(0.0, 0.0, 0.0);
    pub const UP: Vec3 = vec3(0.0, 1.0, 0.0);

    pub fn dot(self, o: Vec3) -> f32 {
        self.x * o.x + self.y * o.y + self.z * o.z
    }

    pub fn cross(self, o: Vec3) -> Vec3 {
        vec3(
            self.y * o.z - self.z * o.y,
            self.z * o.x - self.x * o.z,
            self.x * o.y - self.y * o.x,
        )
    }

    pub fn len(self) -> f32 {
        self.dot(self).sqrt()
    }

    pub fn dist(self, o: Vec3) -> f32 {
        (self - o).len()
    }

    /// Unit vector, or zero when too short to normalize safely.
    pub fn norm(self) -> Vec3 {
        let l = self.len();
        if l > 1e-6 {
            self * (1.0 / l)
        } else {
            Vec3::ZERO
        }
    }
}

impl Add for Vec3 {
    type Output = Vec3;
    fn add(self, o: Vec3) -> Vec3 {
        vec3(self.x + o.x, self.y + o.y, self.z + o.z)
    }
}

impl Sub for Vec3 {
    type Output = Vec3;
    fn sub(self, o: Vec3) -> Vec3 {
        vec3(self.x - o.x, self.y - o.y, self.z - o.z)
    }
}

impl Mul<f32> for Vec3 {
    type Output = Vec3;
    fn mul(self, s: f32) -> Vec3 {
        vec3(self.x * s, self.y * s, self.z * s)
    }
}

impl Mul<Vec3> for f32 {
    type Output = Vec3;
    fn mul(self, v: Vec3) -> Vec3 {
        v * self
    }
}

impl Neg for Vec3 {
    type Output = Vec3;
    fn neg(self) -> Vec3 {
        vec3(-self.x, -self.y, -self.z)
    }
}

impl AddAssign for Vec3 {
    fn add_assign(&mut self, o: Vec3) {
        *self = *self + o;
    }
}

impl SubAssign for Vec3 {
    fn sub_assign(&mut self, o: Vec3) {
        *self = *self - o;
    }
}

impl MulAssign<f32> for Vec3 {
    fn mul_assign(&mut self, s: f32) {
        *self = *self * s;
    }
}

/// Wrap an angle difference to the shortest arc, in [-pi, pi]. The
/// building block for steering ("how far do I turn?") and for
/// interpolating headings without the long-way-around spin.
///
/// ```
/// use pm::wrap_angle;
/// use std::f32::consts::PI;
///
/// assert!((wrap_angle(1.5 * PI) + 0.5 * PI).abs() < 1e-6); // 270deg -> -90deg
/// assert!((wrap_angle(0.1 - 2.0 * PI) - 0.1).abs() < 1e-6);
/// ```
pub fn wrap_angle(dh: f32) -> f32 {
    (dh + std::f32::consts::PI).rem_euclid(std::f32::consts::TAU) - std::f32::consts::PI
}

/// Interpolate between two angles along the SHORTEST arc. Use this in a
/// pool lerp for every angular field (heading, aim, ...) — a plain lerp
/// spins entities the long way round when the angle crosses the wrap,
/// and that's a silent visual bug.
///
/// ```
/// use pm::lerp_angle;
/// use std::f32::consts::PI;
///
/// // Crossing the pi/-pi wrap: halfway is just past the wrap, not 0.
/// let h = lerp_angle(PI - 0.1, -PI + 0.1, 0.5);
/// assert!((h.abs() - PI).abs() < 1e-6);
/// ```
pub fn lerp_angle(a: f32, b: f32, t: f32) -> f32 {
    a + wrap_angle(b - a) * t
}

/// Column-major 4x4 — the layout WGSL/SPIR-V uniforms expect, so `.0`
/// can be memcpy'd into a shader uniform directly. Compose with `*`
/// (or `.mul(..)`, same thing): `proj * view * model` applies model
/// first.
#[derive(Clone, Copy, PartialEq, Debug, Pod, Zeroable)]
#[repr(C)]
pub struct Mat4(pub [f32; 16]);

impl Default for Mat4 {
    fn default() -> Self {
        Mat4::IDENTITY
    }
}

impl Mul for Mat4 {
    type Output = Mat4;
    fn mul(self, o: Mat4) -> Mat4 {
        let (a, b) = (self.0, o.0);
        let mut m = [0.0f32; 16];
        for c in 0..4 {
            for r in 0..4 {
                m[c * 4 + r] = (0..4).map(|k| a[k * 4 + r] * b[c * 4 + k]).sum();
            }
        }
        Mat4(m)
    }
}

impl Mat4 {
    pub const IDENTITY: Mat4 = Mat4([
        1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0,
    ]);

    /// Transform a point (w = 1, includes translation).
    pub fn transform_point(self, v: Vec3) -> Vec3 {
        let m = self.0;
        vec3(
            m[0] * v.x + m[4] * v.y + m[8] * v.z + m[12],
            m[1] * v.x + m[5] * v.y + m[9] * v.z + m[13],
            m[2] * v.x + m[6] * v.y + m[10] * v.z + m[14],
        )
    }

    /// Rotate a direction (w = 0, ignores translation).
    pub fn transform_dir(self, v: Vec3) -> Vec3 {
        let m = self.0;
        vec3(
            m[0] * v.x + m[4] * v.y + m[8] * v.z,
            m[1] * v.x + m[5] * v.y + m[9] * v.z,
            m[2] * v.x + m[6] * v.y + m[10] * v.z,
        )
    }

    pub fn translate(t: Vec3) -> Mat4 {
        let mut m = Mat4::IDENTITY;
        m.0[12] = t.x;
        m.0[13] = t.y;
        m.0[14] = t.z;
        m
    }

    pub fn scale(s: f32) -> Mat4 {
        let mut m = Mat4::IDENTITY;
        m.0[0] = s;
        m.0[5] = s;
        m.0[10] = s;
        m
    }

    /// Per-axis scale — stretch one authored mesh into many shapes
    /// (a unit cube into every wall and building) instead of baking a
    /// mesh per shape. Note for lit meshes: shaders that transform
    /// normals by the model matrix (pm_sdl's does, then normalizes)
    /// skew normals under non-uniform scale — exact for axis-aligned
    /// boxes, increasingly wrong for angled faces at extreme ratios.
    pub fn scale_xyz(sx: f32, sy: f32, sz: f32) -> Mat4 {
        let mut m = Mat4::IDENTITY;
        m.0[0] = sx;
        m.0[5] = sy;
        m.0[10] = sz;
        m
    }

    pub fn rot_x(a: f32) -> Mat4 {
        let (s, c) = a.sin_cos();
        let mut m = Mat4::IDENTITY;
        m.0[5] = c;
        m.0[6] = s;
        m.0[9] = -s;
        m.0[10] = c;
        m
    }

    pub fn rot_y(a: f32) -> Mat4 {
        let (s, c) = a.sin_cos();
        let mut m = Mat4::IDENTITY;
        m.0[0] = c;
        m.0[2] = -s;
        m.0[8] = s;
        m.0[10] = c;
        m
    }

    /// Rotation about a unit axis (Rodrigues, in matrix form).
    pub fn rot_axis(axis: Vec3, a: f32) -> Mat4 {
        let (s, c) = a.sin_cos();
        let t = 1.0 - c;
        let (x, y, z) = (axis.x, axis.y, axis.z);
        Mat4([
            t * x * x + c,
            t * x * y + s * z,
            t * x * z - s * y,
            0.0,
            t * x * y - s * z,
            t * y * y + c,
            t * y * z + s * x,
            0.0,
            t * x * z + s * y,
            t * y * z - s * x,
            t * z * z + c,
            0.0,
            0.0,
            0.0,
            0.0,
            1.0,
        ])
    }

    /// View matrix looking from `eye` toward `target` (+z forward).
    pub fn look_at(eye: Vec3, target: Vec3, up: Vec3) -> Mat4 {
        let f = (target - eye).norm();
        let r = up.cross(f).norm();
        let u = f.cross(r);
        Mat4([
            r.x,
            u.x,
            f.x,
            0.0,
            r.y,
            u.y,
            f.y,
            0.0,
            r.z,
            u.z,
            f.z,
            0.0,
            -r.dot(eye),
            -u.dot(eye),
            -f.dot(eye),
            1.0,
        ])
    }
}

#[cfg(test)]
mod tests3d {
    use super::*;

    #[test]
    fn mat4_identity_and_transform() {
        let p = vec3(1.0, 2.0, 3.0);
        assert_eq!(Mat4::IDENTITY.transform_point(p), p);
        assert_eq!(
            Mat4::translate(vec3(1.0, 0.0, 0.0)).transform_point(p),
            vec3(2.0, 2.0, 3.0)
        );
        assert_eq!(Mat4::translate(vec3(1.0, 0.0, 0.0)).transform_dir(p), p);
        let r = Mat4::rot_y(std::f32::consts::FRAC_PI_2).transform_dir(vec3(0.0, 0.0, 1.0));
        assert!(r.dist(vec3(1.0, 0.0, 0.0)) < 1e-6);
    }

    #[test]
    fn mat4_mul_composes_left_to_right() {
        let m = Mat4::translate(vec3(5.0, 0.0, 0.0)).mul(Mat4::scale(2.0));
        // Scale first, then translate.
        assert_eq!(m.transform_point(vec3(1.0, 0.0, 0.0)), vec3(7.0, 0.0, 0.0));
    }

    #[test]
    fn look_at_view_space() {
        let v = Mat4::look_at(vec3(0.0, 0.0, -10.0), Vec3::ZERO, Vec3::UP);
        // The target sits straight ahead, 10 units down +z.
        assert!(v.transform_point(Vec3::ZERO).dist(vec3(0.0, 0.0, 10.0)) < 1e-5);
        // World up is still up in view space.
        assert!(v.transform_dir(Vec3::UP).dist(Vec3::UP) < 1e-6);
    }

    #[test]
    fn rot_axis_matches_axis_rotations() {
        let a = 0.7;
        for v in [vec3(1.0, 2.0, 3.0), vec3(-1.0, 0.5, 2.0)] {
            let m1 = Mat4::rot_y(a).transform_dir(v);
            let m2 = Mat4::rot_axis(Vec3::UP, a).transform_dir(v);
            assert!(m1.dist(m2) < 1e-5);
        }
    }
}
