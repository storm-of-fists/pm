//! box3d-sys — vendored [Box3D](https://github.com/erincatto/box3d)
//! (pin + policy in `vendor/box3d/VENDOR.md`) behind the `pmb3` C shim.
//!
//! THE SEAM RULE: pm code never sees a `b3*` type. The shim
//! (`src/pmb3.c`, the only file that includes Box3D headers) exposes
//! primitive-typed calls; this crate wraps them in a minimal safe API
//! (`World`, `BodyId`). Box3D is alpha — upstream churn lands in the
//! shim, not in game code. Spike 1 surface (2026-07-23): worlds, box
//! bodies, step, poses, sleep — enough to prove the toolchain and the
//! solver's determinism. Capsules/contacts/joints/recording arrive
//! with spikes 2-4 (the plan lives on the BUILDINGS TODO in hogs-sim).

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Vec3 {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Quat {
    pub x: f32,
    pub y: f32,
    pub z: f32,
    pub w: f32,
}

impl Default for Quat {
    fn default() -> Self {
        Quat { x: 0.0, y: 0.0, z: 0.0, w: 1.0 }
    }
}

unsafe extern "C" {
    fn pmb3_world_create(gx: f32, gy: f32, gz: f32) -> u32;
    fn pmb3_world_destroy(w: u32);
    fn pmb3_world_step(w: u32, dt: f32, substeps: i32);
    fn pmb3_body_box(
        w: u32,
        kind: i32,
        pos: Vec3,
        rot: Quat,
        half: Vec3,
        density: f32,
        friction: f32,
    ) -> u64;
    fn pmb3_body_capsule(
        w: u32,
        kind: i32,
        pos: Vec3,
        half_h: f32,
        radius: f32,
        density: f32,
        friction: f32,
        lock_upright: i32,
    ) -> u64;
    fn pmb3_body_hull(
        w: u32,
        kind: i32,
        pos: Vec3,
        rot: Quat,
        pts: *const Vec3,
        n: i32,
        density: f32,
        friction: f32,
    ) -> u64;
    fn pmb3_body_destroy(body: u64);
    fn pmb3_body_set_pose(body: u64, pos: Vec3, rot: Quat);
    fn pmb3_body_sphere(w: u32, kind: i32, pos: Vec3, radius: f32, density: f32, friction: f32) -> u64;
    fn pmb3_body_set_velocity(body: u64, v: Vec3);
    fn pmb3_body_force(body: u64, f: Vec3);
    fn pmb3_body_set_damping(body: u64, linear: f32);
    fn pmb3_body_lock_rotation(body: u64);
    fn pmb3_body_set_angular_velocity(body: u64, v: Vec3);
    fn pmb3_body_angular_velocity(body: u64, v: *mut Vec3);
    fn pmb3_wheel_joint(
        w: u32,
        chassis: u64,
        wheel: u64,
        mount: Vec3,
        hertz: f32,
        damping: f32,
        max_torque: f32,
    ) -> u64;
    fn pmb3_wheel_spin(joint: u64, speed: f32);
    fn pmb3_body_pose(body: u64, pos: *mut Vec3, rot: *mut Quat);
    fn pmb3_body_velocity(body: u64, vel: *mut Vec3);
    fn pmb3_body_awake(body: u64) -> i32;
    fn pmb3_body_set_friction(body: u64, mu: f32);
    fn pmb3_body_set_filter(body: u64, category: u64, mask: u64);
    fn pmb3_body_set_type(body: u64, kind: i32);
    fn pmb3_world_cast_ray(
        w: u32,
        origin: Vec3,
        translation: Vec3,
        mask: u64,
        point: *mut Vec3,
        frac: *mut f32,
    ) -> i32;
    fn pmb3_body_cast_sphere(
        body: u64,
        tpos: Vec3,
        trot: Quat,
        origin: Vec3,
        radius: f32,
        translation: Vec3,
        point: *mut Vec3,
        frac: *mut f32,
    ) -> i32;
    fn pmb3_world_overlap_capsule(
        w: u32,
        p1: Vec3,
        p2: Vec3,
        radius: f32,
        mask: u64,
        out: *mut u64,
        cap: i32,
    ) -> i32;
}

/// Opaque Box3D body handle (packed id — Copy, pod-friendly, 8 bytes).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct BodyId(u64);

/// Opaque Box3D joint handle, packed like [`BodyId`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct JointId(u64);

/// One Box3D world. Owns its handle; drop destroys it. The intended
/// pm shape is exactly one of these inside a server task (pods in,
/// poses out) — nothing here is thread-aware because pm tasks aren't.
pub struct World(u32);

/// Box3D's world table is a process-global array scanned WITHOUT locks
/// (`b3_worlds` in b3CreateWorld) — two threads creating worlds at
/// once can claim the same slot. Serialize create/destroy here so
/// multi-threaded TESTS can't race it; game code runs one world on one
/// thread and never contends. (Stepping/querying different worlds
/// concurrently remains unsupported — pm tasks aren't threaded.)
static WORLD_GATE: std::sync::Mutex<()> = std::sync::Mutex::new(());

impl World {
    pub fn new(gravity: Vec3) -> World {
        let _gate = WORLD_GATE.lock().unwrap();
        World(unsafe { pmb3_world_create(gravity.x, gravity.y, gravity.z) })
    }

    /// Advance the world. Box3D wants a FIXED dt (its docs and our
    /// determinism story agree); substeps 4 is upstream's default.
    pub fn step(&mut self, dt: f32, substeps: i32) {
        unsafe { pmb3_world_step(self.0, dt, substeps) }
    }

    /// A box body. `kind`: [`STATIC`], [`KINEMATIC`], or [`DYNAMIC`].
    pub fn body_box(
        &mut self,
        kind: i32,
        pos: Vec3,
        rot: Quat,
        half: Vec3,
        density: f32,
        friction: f32,
    ) -> BodyId {
        BodyId(unsafe { pmb3_body_box(self.0, kind, pos, rot, half, density, friction) })
    }

    /// An upright capsule (hemisphere centers at local ±half_h on y) —
    /// the character/critter shape. `lock_upright` freezes angular x/z
    /// so it jostles and yaws but never tips over.
    #[allow(clippy::too_many_arguments)]
    pub fn body_capsule(
        &mut self,
        kind: i32,
        pos: Vec3,
        half_h: f32,
        radius: f32,
        density: f32,
        friction: f32,
        lock_upright: bool,
    ) -> BodyId {
        BodyId(unsafe {
            pmb3_body_capsule(self.0, kind, pos, half_h, radius, density, friction, lock_upright as i32)
        })
    }

    /// Hard-set the linear velocity (the AI-drive verb for spikes: a
    /// wander brain writes velocities; the solver makes them contend).
    pub fn set_velocity(&mut self, body: BodyId, v: Vec3) {
        unsafe { pmb3_body_set_velocity(body.0, v) }
    }

    /// Accumulate a force through the center of mass for this step.
    pub fn force(&mut self, body: BodyId, f: Vec3) {
        unsafe { pmb3_body_force(body.0, f) }
    }

    /// Linear damping — Box3D worlds have no air, so anything driven
    /// by a constant force needs damping to reach a steady speed.
    pub fn set_damping(&mut self, body: BodyId, linear: f32) {
        unsafe { pmb3_body_set_damping(body.0, linear) }
    }

    /// Freeze all rotation (controlled-measurement bodies, kinematic
    /// stand-ins for pm-posed vehicles).
    pub fn lock_rotation(&mut self, body: BodyId) {
        unsafe { pmb3_body_lock_rotation(body.0) }
    }

    /// A convex hull body from up to 64 points in the body's local
    /// space (statics usually sit at the origin and pass world-space
    /// points directly). Ramps, chunks, anything authored-convex.
    pub fn body_hull(
        &mut self,
        kind: i32,
        pos: Vec3,
        rot: Quat,
        pts: &[Vec3],
        density: f32,
        friction: f32,
    ) -> BodyId {
        BodyId(unsafe {
            pmb3_body_hull(self.0, kind, pos, rot, pts.as_ptr(), pts.len() as i32, density, friction)
        })
    }

    /// Remove a body (and its shapes) from the world.
    pub fn destroy(&mut self, body: BodyId) {
        unsafe { pmb3_body_destroy(body.0) }
    }

    /// Teleport — kinematic mirrors and respawn resets only; regular
    /// motion should be velocities and forces.
    pub fn set_pose(&mut self, body: BodyId, pos: Vec3, rot: Quat) {
        unsafe { pmb3_body_set_pose(body.0, pos, rot) }
    }

    pub fn body_sphere(
        &mut self,
        kind: i32,
        pos: Vec3,
        radius: f32,
        density: f32,
        friction: f32,
    ) -> BodyId {
        BodyId(unsafe { pmb3_body_sphere(self.0, kind, pos, radius, density, friction) })
    }

    pub fn set_angular_velocity(&mut self, body: BodyId, v: Vec3) {
        unsafe { pmb3_body_set_angular_velocity(body.0, v) }
    }

    pub fn angular_velocity(&self, body: BodyId) -> Vec3 {
        let mut v = Vec3::default();
        unsafe { pmb3_body_angular_velocity(body.0, &mut v) };
        v
    }

    /// A car wheel for a y-up, +z-forward vehicle (axle on x):
    /// suspension spring toward `mount` on the chassis, spin motor
    /// capped at `max_torque`. Frame conventions live in the shim.
    pub fn wheel_joint(
        &mut self,
        chassis: BodyId,
        wheel: BodyId,
        mount: Vec3,
        hertz: f32,
        damping: f32,
        max_torque: f32,
    ) -> JointId {
        JointId(unsafe { pmb3_wheel_joint(self.0, chassis.0, wheel.0, mount, hertz, damping, max_torque) })
    }

    /// Command the wheel's spin motor speed, rad/s (the throttle verb).
    pub fn wheel_spin(&mut self, joint: JointId, speed: f32) {
        unsafe { pmb3_wheel_spin(joint.0, speed) }
    }

    pub fn pose(&self, body: BodyId) -> (Vec3, Quat) {
        let (mut p, mut q) = (Vec3::default(), Quat::default());
        unsafe { pmb3_body_pose(body.0, &mut p, &mut q) };
        (p, q)
    }

    pub fn velocity(&self, body: BodyId) -> Vec3 {
        let mut v = Vec3::default();
        unsafe { pmb3_body_velocity(body.0, &mut v) };
        v
    }

    /// Asleep bodies are the free-bandwidth seam: an entry that stops
    /// changing stops replicating (pm's change-tick doctrine).
    pub fn awake(&self, body: BodyId) -> bool {
        unsafe { pmb3_body_awake(body.0) != 0 }
    }

    /// Set contact friction on every shape of a body, live (wakes it —
    /// only contacts formed after the change feel the new value).
    pub fn set_friction(&mut self, body: BodyId, mu: f32) {
        unsafe { pmb3_body_set_friction(body.0, mu) }
    }

    /// Category/mask bits on every shape of a body. Contact and query
    /// tests both require the match in BOTH directions: `a.category &
    /// b.mask` and `b.category & a.mask`.
    pub fn set_filter(&mut self, body: BodyId, category: u64, mask: u64) {
        unsafe { pmb3_body_set_filter(body.0, category, mask) }
    }

    /// Change a body's kind (`STATIC`/`KINEMATIC`/`DYNAMIC`) in place —
    /// the corpse-ghost verb (a dead unit parks as a castable kinematic
    /// until its history frames expire).
    pub fn set_type(&mut self, body: BodyId, kind: i32) {
        unsafe { pmb3_body_set_type(body.0, kind) }
    }

    /// Closest ray hit in the live world against shapes whose category
    /// is in `mask` — `(hit point, fraction of the translation)`.
    pub fn cast_ray(&self, origin: Vec3, translation: Vec3, mask: u64) -> Option<(Vec3, f32)> {
        let (mut p, mut f) = (Vec3::default(), 0.0f32);
        (unsafe { pmb3_world_cast_ray(self.0, origin, translation, mask, &mut p, &mut f) } != 0)
            .then_some((p, f))
    }

    /// Cast a sphere (`radius` 0 = a ray) at ONE body posed at an
    /// arbitrary transform — the lag-comp verb: the caller supplies a
    /// rewound pose, Box3D judges the same geometry that collides.
    pub fn body_cast_sphere(
        &self,
        body: BodyId,
        pose: (Vec3, Quat),
        origin: Vec3,
        radius: f32,
        translation: Vec3,
    ) -> Option<(Vec3, f32)> {
        let (mut p, mut f) = (Vec3::default(), 0.0f32);
        (unsafe {
            pmb3_body_cast_sphere(body.0, pose.0, pose.1, origin, radius, translation, &mut p, &mut f)
        } != 0)
            .then_some((p, f))
    }

    /// Every body (category in `mask`) overlapping the capsule
    /// `p1..p2` at `radius` — the touch/bite verb, live world.
    pub fn overlap_capsule(&self, p1: Vec3, p2: Vec3, radius: f32, mask: u64) -> Vec<BodyId> {
        let mut out = [0u64; 64];
        let n = unsafe {
            pmb3_world_overlap_capsule(self.0, p1, p2, radius, mask, out.as_mut_ptr(), 64)
        };
        out[..n as usize].iter().map(|&b| BodyId(b)).collect()
    }
}

impl Drop for World {
    fn drop(&mut self) {
        let _gate = WORLD_GATE.lock().unwrap();
        unsafe { pmb3_world_destroy(self.0) }
    }
}

pub const STATIC: i32 = 0;
pub const KINEMATIC: i32 = 1;
pub const DYNAMIC: i32 = 2;

#[cfg(test)]
mod tests {
    use super::*;

    fn v(x: f32, y: f32, z: f32) -> Vec3 {
        Vec3 { x, y, z }
    }

    /// Build the spike-1 scene: a ground slab and a rain of boxes.
    fn drop_boxes(n: usize) -> (World, Vec<BodyId>) {
        let mut w = World::new(v(0.0, -9.81, 0.0));
        w.body_box(STATIC, v(0.0, -0.5, 0.0), Quat::default(), v(50.0, 0.5, 50.0), 1.0, 0.6);
        let bodies: Vec<BodyId> = (0..n)
            .map(|i| {
                // A loose grid with a deterministic jitter so stacks
                // topple instead of balancing perfectly.
                let (ix, iz) = ((i % 8) as f32, (i / 8) as f32);
                w.body_box(
                    DYNAMIC,
                    v(
                        ix * 1.1 - 4.0 + (i as f32 * 0.37).sin() * 0.05,
                        2.0 + (i / 8) as f32 * 1.5,
                        iz * 1.1 - 4.0 + (i as f32 * 0.61).cos() * 0.05,
                    ),
                    Quat::default(),
                    v(0.4, 0.4, 0.4),
                    1.0,
                    0.6,
                )
            })
            .collect();
        (w, bodies)
    }

    #[test]
    fn boxes_fall_settle_and_sleep() {
        let (mut w, bodies) = drop_boxes(50);
        for _ in 0..600 {
            w.step(1.0 / 60.0, 4);
        }
        let mut asleep = 0;
        for &b in &bodies {
            let (p, _) = w.pose(b);
            assert!(p.y > -0.01, "no box fell through the ground, y {}", p.y);
            assert!(p.y < 10.0, "no box exploded upward, y {}", p.y);
            assert!(w.velocity(b).y.abs() < 0.05, "settled, vy {}", w.velocity(b).y);
            asleep += (!w.awake(b)) as u32;
        }
        // Island sleeping is the free-bandwidth win — most of a settled
        // pile must actually be ASLEEP, not merely slow.
        assert!(asleep >= 40, "sleeping engaged on a settled pile, asleep {asleep}/50");
    }

    /// The query surface the collisions slice leans on: world ray
    /// casts respect the category mask, per-body sphere casts judge
    /// the body at a SUPPLIED transform (the lag-comp rewind), radius
    /// grows the query, and the capsule overlap finds live bodies.
    #[test]
    fn casts_filter_rewind_and_pad() {
        let mut w = World::new(v(0.0, -9.81, 0.0));
        // Ground as category 1, a box "unit" at x=5 as category 2.
        // NOTE the default category is ALL BITS (B3_DEFAULT_CATEGORY_BITS
        // = u64::MAX) — anyone using masked queries must categorize
        // every body explicitly, statics included.
        let ground =
            w.body_box(STATIC, v(0.0, -0.5, 0.0), Quat::default(), v(50.0, 0.5, 50.0), 1.0, 0.6);
        w.set_filter(ground, 1, !0);
        let unit = w.body_box(STATIC, v(5.0, 1.0, 0.0), Quat::default(), v(1.0, 1.0, 1.0), 1.0, 0.6);
        w.set_filter(unit, 2, !0);

        // A flat ray down +x at y=1: masked to category 1 it passes the
        // unit and flies off (no wall in range); masked to everything
        // it stops on the unit's near face.
        let (o, t) = (v(0.0, 1.0, 0.0), v(20.0, 0.0, 0.0));
        assert!(w.cast_ray(o, t, 1).is_none(), "category mask must exclude the unit");
        let (p, f) = w.cast_ray(o, t, !0).expect("unmasked ray hits the unit");
        assert!((p.x - 4.0).abs() < 1e-3 && (f - 0.2).abs() < 1e-3, "near face at x=4, got {p:?}");

        // The lag-comp verb: cast at the body as if it were at x=-5
        // (a rewound frame) — the live pose must not matter.
        let rewound = (v(-5.0, 1.0, 0.0), Quat::default());
        let hit = w.body_cast_sphere(unit, rewound, v(-10.0, 1.0, 0.0), 0.0, v(20.0, 0.0, 0.0));
        let (p, _) = hit.expect("ray hits the body at its REWOUND pose");
        assert!((p.x + 6.0).abs() < 1e-3, "rewound near face at x=-6, got {p:?}");

        // Radius is the shot's forgiveness: a ray 2.5 over the box top
        // misses, a 0.8-sphere along the same line connects.
        let graze = (v(5.0, 1.0, 0.0), Quat::default());
        let (o, t) = (v(0.0, 2.5, 0.0), v(20.0, 0.0, 0.0));
        assert!(w.body_cast_sphere(unit, graze, o, 0.0, t).is_none(), "0.5 over the top misses");
        assert!(w.body_cast_sphere(unit, graze, o, 0.8, t).is_some(), "the pad connects it");

        // Touch verb: a vertical capsule beside the unit's face
        // overlaps it; the same capsule masked to category 1 does not.
        let (p1, p2) = (v(3.5, 0.5, 0.0), v(3.5, 1.5, 0.0));
        assert_eq!(w.overlap_capsule(p1, p2, 0.7, 2), vec![unit], "capsule reaches the face");
        assert!(!w.overlap_capsule(p1, p2, 0.7, 1).contains(&unit), "mask excludes the unit");
        assert!(w.overlap_capsule(p1, p2, 0.3, 2).is_empty(), "short reach misses");
    }

    /// The property every future spike leans on: two identical runs
    /// produce IDENTICAL bytes. This is the determinism Box3D
    /// advertises, checked from OUR side of the FFI on our workload.
    #[test]
    fn same_scene_same_bytes() {
        let run = || {
            let (mut w, bodies) = drop_boxes(50);
            let mut out = Vec::with_capacity(bodies.len() * 7);
            for _ in 0..300 {
                w.step(1.0 / 60.0, 4);
            }
            for &b in &bodies {
                let (p, q) = w.pose(b);
                out.extend_from_slice(&[p.x, p.y, p.z, q.x, q.y, q.z, q.w]);
            }
            out
        };
        let (a, b) = (run(), run());
        assert!(
            a.iter().zip(&b).all(|(x, y)| x.to_bits() == y.to_bits()),
            "two identical runs must be bit-identical"
        );
    }
}
