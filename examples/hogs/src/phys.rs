//! THE SERVER'S PHYSICAL WORLD — Box3D integration slice 1
//! (2026-07-23, Connor's "just send it"). One solver world lives in a
//! server task (`Rc<RefCell<Phys>>` captured like the client's
//! renderer); pods in, poses out:
//!
//! - STATICS: ground, arena walls, BUILDINGS boxes, RAMPS as real
//!   wedge hulls — built once at construction.
//! - HOGS are dynamic capsules (full angular lock — the spike-2 idiom:
//!   yaw is collision-irrelevant and anything less never sleeps). The
//!   AI writes desired velocities; the solver makes them contend —
//!   jostling, walls, ramps, and bogging vehicles down are all
//!   emergent contact now. `building_push`/wall clamps for hogs are
//!   GONE from the AI task.
//! - TRUCKS are dynamic boxes driven by our force laws (spike-3
//!   drive doctrine: wheels-down gate, grip force as the tire model,
//!   steering writes ang.y only so tumble stays real).
//! - HELIS mirror in as KINEMATIC boxes posed from the pm-stepped pod:
//!   the crowd collides with a parked heli, the heli stays pm math.
//!
//! Membership syncs by diffing pools against the body maps each tick
//! (spawn = body appears, death/respawn = body destroyed; a pod that
//! teleported — respawn, reconnect restore — snaps its body).
//!
//! PREDICTION (the open spike-4 experiment, now live): clients still
//! predict with the hogs-sim shared steps against `ground_probe`
//! terrain — an APPROXIMATION of the solver truth. On flat ground the
//! laws match closely; on ramps and in contact they diverge and the
//! reconcile eats the difference. How that feels at lag=80/loss=3% IS
//! the spike-4 measurement; candidates (a)/(b) in the plan TODO
//! replace the approximation if it blips too hard.

use crate::common::*;
use box3d_sys as b3;
use pm::Id;
use std::collections::HashMap;

/// Solver-side hog capsule (matches the collide.body capsule scale).
pub const PHYS_HOG_HALF_H: f32 = 0.3;
pub const PHYS_HOG_R: f32 = 0.45;
/// Truck box half-extents — the models.rs collide.body box.
pub const PHYS_TRUCK_HALF: (f32, f32, f32) = (0.9, 0.7, 1.6);
pub const PHYS_TRUCK_DENSITY: f32 = 3.0;
/// ZERO on purpose (playtest 1): our grip/drag forces are the entire
/// tire model, and Box3D combines contact friction as sqrt(mu_a*mu_b)
/// — any nonzero here taxed the solver truck's top speed below the pm
/// step's equilibrium, so prediction disagreed at cruise FOREVER and
/// corrections never stopped. Frictionless vs everything; the wreck
/// slide that contact friction used to damp is a hand force in
/// `truck_drive` instead.
pub const PHYS_TRUCK_MU: f32 = 0.0;

fn bv(v: pm::Vec3) -> b3::Vec3 {
    b3::Vec3 { x: v.x, y: v.y, z: v.z }
}

fn bq(q: pm::Quat) -> b3::Quat {
    b3::Quat { x: q.x, y: q.y, z: q.z, w: q.w }
}

pub struct Phys {
    pub world: b3::World,
    hogs: HashMap<Id, b3::BodyId>,
    trucks: HashMap<Id, b3::BodyId>,
    helis: HashMap<Id, b3::BodyId>,
}

impl Phys {
    pub fn new() -> Phys {
        let mut world = b3::World::new(b3::Vec3 { x: 0.0, y: -9.81, z: 0.0 });
        let ident = b3::Quat::default();
        // Ground slab and four arena walls (tall enough that nothing
        // launches over them).
        world.body_box(
            b3::STATIC,
            b3::Vec3 { x: 0.0, y: -0.5, z: 0.0 },
            ident,
            b3::Vec3 { x: ARENA + 20.0, y: 0.5, z: ARENA + 20.0 },
            1.0,
            0.6,
        );
        for (x, z, hw, hd) in [
            (-ARENA - 1.0, 0.0, 1.0, ARENA + 2.0),
            (ARENA + 1.0, 0.0, 1.0, ARENA + 2.0),
            (0.0, -ARENA - 1.0, ARENA + 2.0, 1.0),
            (0.0, ARENA + 1.0, ARENA + 2.0, 1.0),
        ] {
            world.body_box(
                b3::STATIC,
                b3::Vec3 { x, y: 10.0, z },
                ident,
                b3::Vec3 { x: hw, y: 10.0, z: hd },
                1.0,
                0.4,
            );
        }
        for &(x, z, hw, hd, h) in BUILDINGS.iter() {
            world.body_box(
                b3::STATIC,
                b3::Vec3 { x, y: h * 0.5, z },
                ident,
                b3::Vec3 { x: hw, y: h * 0.5, z: hd },
                1.0,
                0.6,
            );
        }
        // Ramps as REAL wedge hulls — same six corners the renderer
        // draws and `ground_probe` computes, so all three agree.
        for &(rx, rz, yaw, hw, hl, rh) in RAMPS.iter() {
            let (s, c) = yaw.sin_cos();
            let corner = |lx: f32, ly: f32, lz: f32| b3::Vec3 {
                x: rx + lx * c + lz * s,
                y: ly,
                z: rz + lz * c - lx * s,
            };
            let pts = [
                corner(-hw, 0.0, -hl),
                corner(hw, 0.0, -hl),
                corner(hw, 0.0, hl),
                corner(-hw, 0.0, hl),
                corner(hw, rh, hl),
                corner(-hw, rh, hl),
            ];
            world.body_hull(b3::STATIC, b3::Vec3::default(), ident, &pts, 1.0, 0.5);
        }
        Phys { world, hogs: HashMap::new(), trucks: HashMap::new(), helis: HashMap::new() }
    }

    /// Membership + step + readback, in that order — runs EARLY in the
    /// tick (prio 26), so this tick's AI/drive tasks read fresh poses
    /// and the velocities/forces they write are consumed by the NEXT
    /// tick's step (one-tick force latency, invisible at 60 Hz).
    pub fn tick(
        &mut self,
        hogs: &mut pm::Pool<Hog>,
        trucks: &mut pm::Pool<Truck>,
        helis: &mut pm::Pool<Heli>,
    ) {
        // -- membership: bodies follow pool entries.
        self.hogs.retain(|id, body| {
            let live = hogs.contains(*id);
            if !live {
                self.world.destroy(*body);
            }
            live
        });
        for (id, h) in hogs.iter() {
            let world = &mut self.world;
            self.hogs.entry(id).or_insert_with(|| {
                let b = world.body_capsule(
                    b3::DYNAMIC,
                    b3::Vec3 { x: h.body.pos.x, y: PHYS_HOG_HALF_H + PHYS_HOG_R + 0.01, z: h.body.pos.z },
                    PHYS_HOG_HALF_H,
                    PHYS_HOG_R,
                    2.0,
                    0.3,
                    false,
                );
                world.lock_rotation(b);
                b
            });
        }
        self.trucks.retain(|id, body| {
            let live = trucks.contains(*id);
            if !live {
                self.world.destroy(*body);
            }
            live
        });
        // Pod pos.y is the GROUND-CONTACT height (0 on the flat, the
        // spawn/prediction convention); the solver box's center rides
        // half-height above it, symmetric with the readback below.
        let lift = PHYS_TRUCK_HALF.1 + 0.05;
        for (id, t) in trucks.iter() {
            let world = &mut self.world;
            let center = b3::Vec3 { x: t.body.pos.x, y: t.body.pos.y + lift, z: t.body.pos.z };
            let body = *self.trucks.entry(id).or_insert_with(|| {
                world.body_box(
                    b3::DYNAMIC,
                    center,
                    bq(t.body.rot),
                    b3::Vec3 { x: PHYS_TRUCK_HALF.0, y: PHYS_TRUCK_HALF.1, z: PHYS_TRUCK_HALF.2 },
                    PHYS_TRUCK_DENSITY,
                    PHYS_TRUCK_MU,
                )
            });
            // Respawn/reconnect teleports the POD; snap the body after
            // it (regular motion never moves 5 u in a tick).
            let (p, _) = self.world.pose(body);
            let d = (p.x - t.body.pos.x, p.z - t.body.pos.z);
            if d.0 * d.0 + d.1 * d.1 > 25.0 {
                self.world.set_pose(body, center, bq(t.body.rot));
                self.world.set_velocity(body, b3::Vec3::default());
                self.world.set_angular_velocity(body, b3::Vec3::default());
            }
        }
        self.helis.retain(|id, body| {
            let live = helis.contains(*id);
            if !live {
                self.world.destroy(*body);
            }
            live
        });
        for (id, h) in helis.iter() {
            let world = &mut self.world;
            let body = *self.helis.entry(id).or_insert_with(|| {
                world.body_box(
                    b3::KINEMATIC,
                    bv(h.body.pos),
                    bq(h.body.rot),
                    b3::Vec3 { x: 1.1, y: 0.9, z: 1.4 },
                    1.0,
                    0.3,
                )
            });
            // Kinematic mirror: the pm-stepped pod IS the pose.
            self.world.set_pose(body, bv(h.body.pos), bq(h.body.rot));
        }

        // -- step.
        self.world.step(FIXED_DT, 4);

        // -- readback: solver poses become pod truth.
        for (id, body) in &self.hogs {
            if let Some(mut h) = hogs.get_mut(*id) {
                let (p, _) = self.world.pose(*body);
                h.body.pos =
                    pm::vec3(p.x, (p.y - PHYS_HOG_HALF_H - PHYS_HOG_R).max(0.0), p.z);
                let v = self.world.velocity(*body);
                h.body.vel = pm::vec3(v.x, v.y, v.z);
                // rot stays the AI's yaw write (the capsule is
                // angular-locked, so the solver has no opinion —
                // UNTIL knockdown/flop unlocks it, and then this is
                // where the tumble quat flows to every client).
            }
        }
        for (id, body) in &self.trucks {
            if let Some(mut t) = trucks.get_mut(*id) {
                let (p, q) = self.world.pose(*body);
                let mut pos = pm::vec3(p.x, (p.y - lift).max(0.0), p.z);
                let mut rot = pm::Quat { x: q.x, y: q.y, z: q.z, w: q.w }.norm();
                let mut vel = {
                    let v = self.world.velocity(*body);
                    pm::vec3(v.x, v.y, v.z)
                };
                let mut ang = {
                    let a = self.world.angular_velocity(*body);
                    pm::vec3(a.x, a.y, a.z)
                };
                // READBACK CONDITIONING (playtest 1): a box parked on
                // flat ground carries contact-solver noise — mm of
                // penetration in y, micro pitch/roll, stray ang — that
                // the client's pm-step prediction writes as EXACT
                // zeros. Left raw, every snapshot corrected and the
                // own-pose smoother dragged ~half a second behind the
                // stick. Snap the noise to the prediction's
                // convention; real events (ramps, launches, tumbles,
                // shoves) blow past these thresholds untouched.
                let (yaw, pitch, roll) = rot.to_yaw_pitch_roll();
                if pitch.abs() < 0.05 && roll.abs() < 0.05 {
                    rot = pm::Quat::from_yaw(yaw);
                }
                if pos.y < 0.05 {
                    pos.y = 0.0;
                    if vel.y.abs() < 0.25 {
                        vel.y = 0.0;
                    }
                }
                if ang.len() < 0.4 {
                    ang = pm::Vec3::ZERO;
                }
                t.body.pos = pos;
                t.body.rot = rot;
                t.body.vel = vel;
                t.body.ang = ang;
            }
        }
    }

    /// The hog AI's motion verb: desired ground velocity, gravity's
    /// vertical preserved.
    pub fn hog_velocity(&mut self, id: Id, vx: f32, vz: f32) {
        if let Some(&body) = self.hogs.get(&id) {
            let vy = self.world.velocity(body).y;
            self.world.set_velocity(body, b3::Vec3 { x: vx, y: vy, z: vz });
        }
    }

    /// The truck drive verb — spike 3's laws verbatim: wheels-down
    /// gate, engine + rolling drag along forward, grip across it,
    /// steering as ang.y only.
    pub fn truck_drive(&mut self, id: Id, t: &Truck, cmd: Drive, boosting: bool, p: &Params) {
        let Some(&body) = self.trucks.get(&id) else {
            return;
        };
        let (_, q) = self.world.pose(body);
        let upness = 1.0 - 2.0 * (q.x * q.x + q.z * q.z);
        if upness < 0.5 {
            // A roof-landed truck is a wreck until recovered — no
            // engine, no grip; just scrape drag (stands in for the
            // contact friction the body deliberately doesn't have).
            let vel = self.world.velocity(body);
            let mass = PHYS_TRUCK_DENSITY * 8.0 * PHYS_TRUCK_HALF.0 * PHYS_TRUCK_HALF.1 * PHYS_TRUCK_HALF.2;
            self.world.force(body, b3::Vec3 { x: -mass * 2.0 * vel.x, y: 0.0, z: -mass * 2.0 * vel.z });
            return;
        }
        let heading = t.heading();
        let (fx, fz) = (heading.sin(), heading.cos());
        let (rx, rz) = (heading.cos(), -heading.sin());
        let vel = self.world.velocity(body);
        let vf = vel.x * fx + vel.z * fz;
        let vl = vel.x * rx + vel.z * rz;
        let ang = self.world.angular_velocity(body);
        let authority = (vf.abs() / 6.0).min(1.0);
        self.world.set_angular_velocity(
            body,
            b3::Vec3 { x: ang.x, y: t.steer * 2.2 * authority * vf.signum(), z: ang.z },
        );
        let (accel, vmax) = if boosting { (26.0, p.boost_vmax) } else { (14.0, p.vmax) };
        let grip = if boosting { p.truck_grip_boost } else { p.truck_grip };
        // Engine cuts once past vmax — the clamp the step had.
        let engine = if vf < vmax { cmd.thrust * accel } else { 0.0 };
        let f_fwd = engine - 1.2 * vf;
        let f_lat = -grip * vl;
        let mass = PHYS_TRUCK_DENSITY * 8.0 * PHYS_TRUCK_HALF.0 * PHYS_TRUCK_HALF.1 * PHYS_TRUCK_HALF.2;
        self.world.force(
            body,
            b3::Vec3 {
                x: mass * (f_fwd * fx + f_lat * rx),
                y: 0.0,
                z: mass * (f_fwd * fz + f_lat * rz),
            },
        );
    }
}
