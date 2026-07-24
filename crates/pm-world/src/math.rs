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

// --- orientation & kinematics ------------------------------------------------

/// Unit quaternion — THE orientation representation for anything that
/// leaves the ground plane. A 2D vehicle's pose is honestly (x, z, yaw);
/// a 3D one stores a `Quat` (no gimbal lock at nose-straight-up, which
/// is exactly where a jet lives). Conventions match [`Mat4`]: right-
/// handed, +Z forward, +Y up; `from_yaw(a)` agrees with `Mat4::rot_y(a)`
/// and positive pitch tips the nose DOWN (`Mat4::rot_x`), asserted in
/// tests.
///
/// Constrained-attitude vehicles (helis: pitch/roll limits) extract
/// euler with [`to_yaw_pitch_roll`](Quat::to_yaw_pitch_roll), clamp, and
/// rebuild with [`from_yaw_pitch_roll`](Quat::from_yaw_pitch_roll) —
/// stable for |pitch| < 90°. Free-attitude vehicles (jets) skip the
/// extraction and compose axis rotations directly.
#[derive(Clone, Copy, PartialEq, Debug, Pod, Zeroable)]
#[repr(C)]
pub struct Quat {
    pub x: f32,
    pub y: f32,
    pub z: f32,
    pub w: f32,
}

impl Default for Quat {
    /// Identity (no rotation) — NOT zeroed; an all-zero quat is not a
    /// rotation, which is why this impl is manual.
    fn default() -> Self {
        Quat::IDENTITY
    }
}

impl Quat {
    pub const IDENTITY: Quat = Quat {
        x: 0.0,
        y: 0.0,
        z: 0.0,
        w: 1.0,
    };

    /// Rotation of `angle` radians about a UNIT `axis` (right-handed).
    pub fn from_axis_angle(axis: Vec3, angle: f32) -> Quat {
        let (s, c) = (angle * 0.5).sin_cos();
        Quat {
            x: axis.x * s,
            y: axis.y * s,
            z: axis.z * s,
            w: c,
        }
    }

    /// Yaw about +Y — the 2D heading, as a quat.
    pub fn from_yaw(yaw: f32) -> Quat {
        Quat::from_axis_angle(Vec3::UP, yaw)
    }

    /// Compose yaw (about Y), then pitch (about X, + = nose down), then
    /// roll (about Z) — the constrained-vehicle attitude constructor.
    pub fn from_yaw_pitch_roll(yaw: f32, pitch: f32, roll: f32) -> Quat {
        Quat::from_yaw(yaw)
            * Quat::from_axis_angle(vec3(1.0, 0.0, 0.0), pitch)
            * Quat::from_axis_angle(vec3(0.0, 0.0, 1.0), roll)
    }

    /// The inverse extraction of [`from_yaw_pitch_roll`](Self::from_yaw_pitch_roll)
    /// (round-trips for |pitch| < 90°).
    pub fn to_yaw_pitch_roll(self) -> (f32, f32, f32) {
        let (x, y, z, w) = (self.x, self.y, self.z, self.w);
        // Matrix elements of the YXZ composition (see to_mat4).
        let m9 = 2.0 * (y * z - w * x);
        let m8 = 2.0 * (x * z + w * y);
        let m10 = 1.0 - 2.0 * (x * x + y * y);
        let m1 = 2.0 * (x * y + w * z);
        let m5 = 1.0 - 2.0 * (x * x + z * z);
        (
            m8.atan2(m10),
            (-m9).clamp(-1.0, 1.0).asin(),
            m1.atan2(m5),
        )
    }

    pub fn dot(self, o: Quat) -> f32 {
        self.x * o.x + self.y * o.y + self.z * o.z + self.w * o.w
    }

    /// Renormalize — call after long chains of composition/interpolation
    /// so drift never accumulates (steps that write `rot` every tick do).
    pub fn norm(self) -> Quat {
        let l = self.dot(self).sqrt();
        if l > 1e-6 {
            Quat {
                x: self.x / l,
                y: self.y / l,
                z: self.z / l,
                w: self.w / l,
            }
        } else {
            Quat::IDENTITY
        }
    }

    /// Rotate a vector: `q * v * q⁻¹`, in the cheap two-cross form.
    pub fn rotate(self, v: Vec3) -> Vec3 {
        let u = vec3(self.x, self.y, self.z);
        let t = u.cross(v) * 2.0;
        v + t * self.w + u.cross(t)
    }

    /// Conjugate — the inverse rotation for a unit quat. `conj().rotate`
    /// carries a world-frame vector into the body frame (what diagonal
    /// body-frame inertia in [`Forces::apply`] needs).
    pub fn conj(self) -> Quat {
        Quat {
            x: -self.x,
            y: -self.y,
            z: -self.z,
            w: self.w,
        }
    }

    /// Normalized lerp along the SHORT arc — the pool-interp / attitude-
    /// easing workhorse (slerp's constant angular velocity isn't worth
    /// its cost at snapshot-interval deltas; if a use case ever shows
    /// visible nlerp speed-warp, add slerp then).
    pub fn nlerp(a: Quat, b: Quat, t: f32) -> Quat {
        let s = if a.dot(b) < 0.0 { -1.0 } else { 1.0 };
        Quat {
            x: a.x + (b.x * s - a.x) * t,
            y: a.y + (b.y * s - a.y) * t,
            z: a.z + (b.z * s - a.z) * t,
            w: a.w + (b.w * s - a.w) * t,
        }
        .norm()
    }

    /// The equivalent rotation matrix (column-major, matches `Mat4`).
    pub fn to_mat4(self) -> Mat4 {
        let (x, y, z, w) = (self.x, self.y, self.z, self.w);
        Mat4([
            1.0 - 2.0 * (y * y + z * z),
            2.0 * (x * y + w * z),
            2.0 * (x * z - w * y),
            0.0,
            2.0 * (x * y - w * z),
            1.0 - 2.0 * (x * x + z * z),
            2.0 * (y * z + w * x),
            0.0,
            2.0 * (x * z + w * y),
            2.0 * (y * z - w * x),
            1.0 - 2.0 * (x * x + y * y),
            0.0,
            0.0,
            0.0,
            0.0,
            1.0,
        ])
    }
}

impl Mul for Quat {
    type Output = Quat;
    /// Hamilton product: `a * b` applies `b` first, then `a` — same
    /// composition order as `Mat4`.
    fn mul(self, o: Quat) -> Quat {
        Quat {
            x: self.w * o.x + self.x * o.w + self.y * o.z - self.z * o.y,
            y: self.w * o.y - self.x * o.z + self.y * o.w + self.z * o.x,
            z: self.w * o.z + self.x * o.y - self.y * o.x + self.z * o.w,
            w: self.w * o.w - self.x * o.x - self.y * o.y - self.z * o.z,
        }
    }
}

/// Rigid-body kinematic state: position, velocity, orientation, angular
/// velocity — the shared chunk every vehicle pod EMBEDS (composition,
/// deliberately not a separate pool: the predicted-pod contract needs
/// pose and velocity inside the pod its step evolves). Step functions
/// map input to forces and rates — through [`Forces`] when the forces
/// act on PARTS of a combined body (a rotor above the roof, a tail
/// thruster on a boom) — then [`integrate`](Body::integrate); rendering
/// takes [`model`](Body::model). `ang` joined 2026-07-23 (the heli
/// integrates it now); a constrained vehicle that scripts its own
/// rotation (the truck's pure-yaw heading) projects `ang` back to zero
/// each step, the same way it projects `pos.y`.
///
/// The physics stance: physics is **library functions, not a system**
/// — there is no rigid-body world to register into, just `Body`,
/// [`Quat`], and step functions games call from their own tasks, in
/// three tiers (STYLE(source): independently identical to Source's
/// QPhysics / VPhysics / client-ragdoll split, which validates it):
/// 1. **Predicted-kinematic** — player vehicles: deterministic pure
///    steps, shared byte-identical by server and prediction replay.
/// 2. **Server-dynamic** — NPC impulses (knockback, shoves): the
///    server writes velocities, replication carries the result,
///    nobody predicts it. A real constraint solver (Box3D-style)
///    becomes this tier's backend *if* stacking/joints get demanded;
///    `Body` is the seam it slots behind.
/// 3. **Client-cosmetic** — corpse tumbles, tracer flight: never on
///    the wire. GPU physics, if ever, lives ONLY here — the
///    authoritative loop must replay deterministically on headless
///    servers.
#[derive(Clone, Copy, PartialEq, Debug, Default, Pod, Zeroable)]
#[repr(C)]
pub struct Body {
    pub pos: Vec3,
    pub vel: Vec3,
    pub rot: Quat,
    /// Angular velocity, WORLD frame, rad/s (axis = spin axis, length =
    /// rate). Zero for vehicles that script their rotation directly.
    pub ang: Vec3,
}

impl Body {
    /// Forward (+Z through `rot`) — a 2D `heading`'s (sin, 0, cos)
    /// when `rot` is pure yaw.
    pub fn fwd(self) -> Vec3 {
        self.rot.rotate(vec3(0.0, 0.0, 1.0))
    }

    pub fn up(self) -> Vec3 {
        self.rot.rotate(Vec3::UP)
    }

    /// The 2D heading most gameplay wants (AI bearings, spawn facing).
    pub fn yaw(self) -> f32 {
        self.rot.to_yaw_pitch_roll().0
    }

    /// Advance position by velocity and orientation by angular velocity
    /// (`q̇ = ½ ω q`, then renormalize). The rotation half is skipped at
    /// exactly `ang == 0` so a yaw-scripted vehicle's `rot` bytes never
    /// move through a needless renorm (prediction is byte-exact).
    pub fn integrate(&mut self, dt: f32) {
        self.pos = self.pos + self.vel * dt;
        if self.ang != Vec3::ZERO {
            let h = self.ang * (0.5 * dt);
            let w = Quat { x: h.x, y: h.y, z: h.z, w: 0.0 } * self.rot;
            self.rot = Quat {
                x: self.rot.x + w.x,
                y: self.rot.y + w.y,
                z: self.rot.z + w.z,
                w: self.rot.w + w.w,
            }
            .norm();
        }
    }

    /// Apply an instantaneous impulse `j` (mass·u/s, world frame) at
    /// `at`, world-frame offset from the center of mass — the one-shot
    /// form of [`Forces::at`] for hits: `vel` takes `j/mass`, `ang`
    /// takes `I⁻¹ (at × j)`. A shot clipping the far end of a boom
    /// spins the machine; the same shot square into the hub just shoves
    /// it — obliquity falls out of the cross product, nobody hand-codes
    /// a `sin()`.
    pub fn impulse_at(&mut self, j: Vec3, at: Vec3, mass: f32, inertia: Vec3) {
        self.vel = self.vel + j * (1.0 / mass);
        let l = self.rot.conj().rotate(at.cross(j)); // body frame
        let spin = vec3(l.x / inertia.x, l.y / inertia.y, l.z / inertia.z);
        self.ang = self.ang + self.rot.rotate(spin);
    }

    /// Model matrix: rotate, then place.
    pub fn model(self) -> Mat4 {
        Mat4::translate(self.pos) * self.rot.to_mat4()
    }
}

/// One tick's worth of forces on a COMBINED body — parts bolted to one
/// rigid frame (rotor above the cabin, thruster on the tail boom). Feed
/// it forces at part offsets, then [`apply`](Forces::apply) once: each
/// force lands twice, as linear acceleration through the center of mass
/// and as torque `r × F` about it — push the tail, the nose comes
/// around. This is the library-not-a-system stance for tier-1 physics:
/// no rigid-body world to register into, just an accumulator a step
/// function fills and applies inside its own math.
///
/// Mass properties live at the call site (a const per vehicle), not in
/// [`Body`] — pods carry state, not configuration. `inertia` is the
/// diagonal of the body-frame inertia tensor (pitch `x`, yaw `y`, roll
/// `z` for a +Z-forward vehicle); the gyroscopic `ω × Iω` term is
/// deliberately dropped — arcade vehicles want controllability, and
/// nothing here spins fast enough to precess visibly.
#[derive(Clone, Copy, Debug, Default)]
pub struct Forces {
    pub force: Vec3,
    pub torque: Vec3,
}

impl Forces {
    /// A force through the center of mass (gravity, drag): no torque.
    pub fn central(&mut self, f: Vec3) {
        self.force = self.force + f;
    }

    /// A force on a part: `at` is the part's offset from the center of
    /// mass, WORLD frame (rotate the model-space mount point through
    /// `body.rot` first). Adds the full force linearly plus `at × f` as
    /// torque — the propagation that makes a combined body one body.
    pub fn at(&mut self, f: Vec3, at: Vec3) {
        self.force = self.force + f;
        self.torque = self.torque + at.cross(f);
    }

    /// A pure torque (world frame) with no linear component — for
    /// control moments modeled without a mount point.
    pub fn torque(&mut self, t: Vec3) {
        self.torque = self.torque + t;
    }

    /// Integrate the accumulated forces into the body's velocities
    /// (`vel += F/m·dt`, `ang += R I⁻¹ Rᵀ τ·dt`). Position and attitude
    /// advance in [`Body::integrate`], which the step calls after its
    /// drags and caps.
    pub fn apply(self, b: &mut Body, mass: f32, inertia: Vec3, dt: f32) {
        b.vel = b.vel + self.force * (dt / mass);
        let l = b.rot.conj().rotate(self.torque); // body frame
        let spin = vec3(l.x / inertia.x, l.y / inertia.y, l.z / inertia.z);
        b.ang = b.ang + b.rot.rotate(spin) * dt;
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
    fn quat_matches_mat4_conventions() {
        // from_yaw ≡ rot_y, pitch ≡ rot_x, roll ≡ rot_axis(Z) — the whole
        // point: quats and matrices must agree on every convention.
        for a in [-2.1f32, -0.4, 0.0, 0.7, 3.0] {
            for (q, m) in [
                (Quat::from_yaw(a), Mat4::rot_y(a)),
                (
                    Quat::from_axis_angle(vec3(1.0, 0.0, 0.0), a),
                    Mat4::rot_x(a),
                ),
                (
                    Quat::from_axis_angle(vec3(0.0, 0.0, 1.0), a),
                    Mat4::rot_axis(vec3(0.0, 0.0, 1.0), a),
                ),
            ] {
                for i in 0..16 {
                    assert!((q.to_mat4().0[i] - m.0[i]).abs() < 1e-5);
                }
            }
        }
        // rotate() agrees with the matrix path.
        let q = Quat::from_yaw_pitch_roll(1.1, 0.4, -0.3);
        let v = vec3(0.3, -1.2, 2.0);
        assert!(q.rotate(v).dist(q.to_mat4().transform_dir(v)) < 1e-5);
        // Positive pitch tips the nose down, like rot_x.
        let f = Quat::from_yaw_pitch_roll(0.0, 0.5, 0.0).rotate(vec3(0.0, 0.0, 1.0));
        assert!(f.y < 0.0);
    }

    #[test]
    fn quat_ypr_round_trips() {
        for &(y, p, r) in &[
            (0.0f32, 0.0f32, 0.0f32),
            (1.2, 0.4, -0.3),
            (-2.8, -0.44, 0.34),
            (3.0, 0.0, 0.0),
        ] {
            let (y2, p2, r2) = Quat::from_yaw_pitch_roll(y, p, r).to_yaw_pitch_roll();
            assert!((y - y2).abs() < 1e-4, "yaw {y} -> {y2}");
            assert!((p - p2).abs() < 1e-4, "pitch {p} -> {p2}");
            assert!((r - r2).abs() < 1e-4, "roll {r} -> {r2}");
        }
    }

    #[test]
    fn quat_nlerp_short_arc() {
        // ±q is the same rotation; nlerp must interpolate the short way
        // even when the signs disagree (the pool-interp wrap case).
        let a = Quat::from_yaw(3.0);
        let b = Quat::from_yaw(-3.0); // ~0.28 rad away through ±pi
        let mid = Quat::nlerp(a, Quat { x: -b.x, y: -b.y, z: -b.z, w: -b.w }, 0.5);
        let (yaw, _, _) = mid.to_yaw_pitch_roll();
        assert!(yaw.abs() > 3.0 || yaw.abs() > 3.0 - 1e-3); // near ±pi, not 0
        assert!((mid.dot(mid) - 1.0).abs() < 1e-5); // unit after lerp
    }

    #[test]
    fn body_basics() {
        let mut b = Body {
            vel: vec3(0.0, 0.0, 10.0),
            ..Body::default()
        };
        assert_eq!(b.rot, Quat::IDENTITY); // Default = identity, not zero
        assert!(b.fwd().dist(vec3(0.0, 0.0, 1.0)) < 1e-6);
        b.integrate(0.5);
        assert!(b.pos.dist(vec3(0.0, 0.0, 5.0)) < 1e-6);
        b.rot = Quat::from_yaw(std::f32::consts::FRAC_PI_2);
        assert!((b.yaw() - std::f32::consts::FRAC_PI_2).abs() < 1e-5);
        assert!(b.fwd().dist(vec3(1.0, 0.0, 0.0)) < 1e-5);
        // model() places after rotating.
        let p = b.model().transform_point(vec3(0.0, 0.0, 1.0));
        assert!(p.dist(vec3(1.0, 0.0, 5.0)) < 1e-5);
    }

    #[test]
    fn ang_integrates_rotation() {
        let mut b = Body {
            ang: vec3(0.0, std::f32::consts::FRAC_PI_2, 0.0),
            ..Body::default()
        };
        for _ in 0..60 {
            b.integrate(1.0 / 60.0);
        }
        // π/2 rad/s about +y for 1 s ≈ a quarter turn of yaw (small
        // first-order integration error is fine, byte-exactness is the
        // golden tests' job).
        assert!(
            (b.yaw() - std::f32::consts::FRAC_PI_2).abs() < 0.01,
            "yaw after 1 s at π/2 rad/s: {}",
            b.yaw()
        );
    }

    #[test]
    fn force_on_a_part_shoves_and_spins() {
        // A sideways force on a tail boom 3 u behind the CoM: the whole
        // body drifts sideways AND yaws — one force, both motions.
        let mut b = Body::default();
        let mut f = Forces::default();
        f.at(vec3(2.0, 0.0, 0.0), vec3(0.0, 0.0, -3.0));
        f.apply(&mut b, 2.0, vec3(1.0, 1.5, 1.0), 0.5);
        assert!((b.vel.x - 0.5).abs() < 1e-6, "F/m·dt = 2/2·0.5");
        // τ = r × F = (0,0,-3)×(2,0,0) = (0,-6,0); ang = τ/I_y·dt = -2.
        assert!((b.ang.y + 2.0).abs() < 1e-6, "got {}", b.ang.y);
        // The same force through the CoM: shove only, no spin.
        let mut b2 = Body::default();
        let mut f2 = Forces::default();
        f2.central(vec3(2.0, 0.0, 0.0));
        f2.apply(&mut b2, 2.0, vec3(1.0, 1.5, 1.0), 0.5);
        assert_eq!(b2.ang, Vec3::ZERO);
        assert_eq!(b2.vel, b.vel);
    }

    #[test]
    fn torque_maps_through_body_frame_inertia() {
        // Yaw the body 90°: its boom (body −z) now lies along world +x,
        // so a world-z torque works against the BODY's pitch inertia
        // (x axis), not roll. The frame round-trip is what this pins.
        let mut b = Body {
            rot: Quat::from_yaw(std::f32::consts::FRAC_PI_2),
            ..Body::default()
        };
        let mut f = Forces::default();
        f.torque(vec3(0.0, 0.0, 4.0));
        f.apply(&mut b, 1.0, vec3(2.0, 1.0, 0.5), 1.0);
        // World z maps to body x (pitch, I=2): |ang| = 4/2 = 2, still
        // about world z after mapping back.
        assert!(b.ang.dist(vec3(0.0, 0.0, 2.0)) < 1e-5, "got {:?}", b.ang);
    }

    #[test]
    fn impulse_obliquity_falls_out_of_the_cross() {
        let (mass, inertia) = (1.0, vec3(1.0, 2.0, 1.0));
        let tail = vec3(0.0, 0.0, -3.0);
        // Square hit across the boom: full yaw kick.
        let mut b = Body::default();
        b.impulse_at(vec3(1.0, 0.0, 0.0), tail, mass, inertia);
        assert!((b.ang.y + 1.5).abs() < 1e-6, "r×j/I = 3/2, got {}", b.ang.y);
        // Down the boom's own axis: pure shove, zero spin.
        let mut b2 = Body::default();
        b2.impulse_at(vec3(0.0, 0.0, -1.0), tail, mass, inertia);
        assert_eq!(b2.ang, Vec3::ZERO);
        assert!((b2.vel.z + 1.0).abs() < 1e-6);
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
