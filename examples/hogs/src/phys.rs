//! THE SERVER'S PHYSICAL WORLD — Box3D slices 1 + 2 (2026-07-23,
//! Connor's "just send it", then "move everything into box3d"). One
//! solver world lives in the LOCAL `"phys"` single (an ordinary handle
//! any server task captures); pods in, poses out — and since slice 2
//! it is ALSO the collision authority: bullets, bites, and targeting
//! all ask THIS world (the parallel capsule+band collider pool,
//! `WorldIndex`, and the hand sweep are gone).
//!
//! BODIES:
//! - STATICS: ground, arena walls, BUILDINGS boxes, RAMPS as real
//!   wedge hulls — built once, category [`CATB_STATIC`]. Bullets stop
//!   on them by a plain world ray cast (no more `building_top` math).
//! - HOGS are dynamic capsules (full angular lock — the spike-2 idiom).
//!   The AI writes desired velocities; the solver makes them contend.
//!   The capsule is the hitbox too, cast with [`HOG_GROW`] so the
//!   gameplay reach matches the old authored hitbox (fatter and taller
//!   than the silhouette — flat truck shots must keep connecting).
//! - TRUCKS are dynamic boxes driven by the spike-3 force laws
//!   (wheels-down gate, grip force as the tire model). A FLIPPED truck
//!   hands itself back to Box3D contact friction ([`WRECK_MU`]) — the
//!   hand scrape-drag force is gone; recovery restores frictionless
//!   (the drive laws are the tire model only while wheels are down).
//! - HELIS mirror in as KINEMATIC part boxes posed from the pm-stepped
//!   pod — the cabin is the contact-active body the crowd bumps, the
//!   tail boom and rotor disc are cast-only hitboxes ([`CATB_HITBOX`]:
//!   a mask no body category matches, so they contact nothing but
//!   queries still see them). All three come from the model's authored
//!   `collide.*` boxes (models.rs stays the hitbox SSOT).
//! - FLYERS are cast-only kinematic mirrors too: no physical presence
//!   (they fly), but bullets and the debug of "what's here" see them.
//! - The DEPOT is a static box in [`CATB_UNIT`]: hogs physically bump
//!   it, bites and stray rounds route to it — zero special code.
//!
//! LAG COMP (the rewind memory, formerly the collider journal): every
//! tick records each unit body's pose into a [`RING`]-deep frame ring.
//! A shot is judged per body with `b3Body_CastShape` at the REWOUND
//! transform — same geometry that collides, posed where the shooter
//! saw it. Favor-the-shooter, one timeline per shot (the `bullets`
//! task holds that half). A dead unit's body is not destroyed but
//! RETIRED — parked kinematic, contact-inert, still castable — until
//! its history frames expire, so a round fired at a fresh corpse still
//! lands where the shooter saw meat (the ghost-eats-round contract).
//!
//! PREDICTION (spike 4, live): clients still predict with the shared
//! steps against `ground_probe` terrain — an APPROXIMATION of solver
//! truth. The reconcile dead-zone (`Params::predict_tol`) absorbs the
//! approximation's mm-scale drift; real divergence (ramps, shoves,
//! tumbles) still corrects.
//!
//! TODO(box3d-move) MASTER NOTE (Connor, 2026-07-23 — "sick of all the
//! handrolled stuff, time to just do local solving"): THE ABSOLUTE
//! NEXT STEP is the spike-4 endgame — clients own a local Box3D world
//! and predict by STEPPING IT, retiring the shared-step approximation
//! and every hand-rolled geometry survivor with it. The family is
//! greppable as TODO(box3d-move) at each site: `ground_probe`/
//! `ground_height`, `building_push(_below)`, `building_top`,
//! `in_building`, the ARENA clamps inside `truck_step`/`heli_step`,
//! `tracer_step`'s roofline gate, the player reticle marcher, the
//! bots' `line_clear` — and the heli's kinematic mirror below (a real
//! solver body once local solving lands). Order: statics into the
//! client world first (pure queries, zero determinism risk), then the
//! predicted step itself. NOT started — Connor feel-tests the
//! predict/interp reorder first.

// TODO(simplify): GENERALIZE THE BODY REGISTRY (Connor, 2026-07-23,
// mid-review of this file). Five per-kind HashMaps + five hand-rolled
// membership loops is the Collider-pool lesson un-learned — "detection
// is DATA the simulation iterates, never functions that know what a
// helicopter is". The shape to land: ONE `bodies: HashMap<Id,
// Vec<BodyId>>` registry plus a per-kind RECIPE declared once
// (dynamic-capsule / driven-box / kinematic-mirror-parts / static-box,
// category, shape source, sync direction: readback vs mirror), so a
// new entity kind is a recipe row — membership diff, mirroring,
// readback, retirement, and the frame ring all run over the one
// registry. Design questions to settle with Connor before building:
// where recipes live (models.rs beside the collide boxes?), and
// whether readback stays per-kind (truck conditioning is genuinely
// truck-shaped) or becomes a recipe hook.
use crate::common::*;
use crate::models::Models;
use box3d_sys as b3;
use pm::Id;
use std::collections::{HashMap, VecDeque};

/// Solver-side hog capsule (the physical body the crowd jostles with).
pub const PHYS_HOG_HALF_H: f32 = 0.3;
pub const PHYS_HOG_R: f32 = 0.45;
/// Hitbox forgiveness on the hog capsule: casts grow it so the
/// gameplay reach matches the authored `collide.body` cylinder
/// (r = HOG_R, taller than the silhouette) the old sweep tested.
pub const HOG_GROW: f32 = HOG_R - PHYS_HOG_R;
/// Truck box half-extents (the physical body IS the hitbox).
pub const PHYS_TRUCK_HALF: (f32, f32, f32) = (0.9, 0.7, 1.6);
/// Solver box center height above the pod origin: pod pos.y is the
/// GROUND-CONTACT height (0 on the flat — the spawn/prediction
/// convention); the box center rides half-height (+ settle slack)
/// above it, symmetric between spawn and readback. The debug cage
/// poses with the same offset.
pub const PHYS_TRUCK_LIFT: f32 = PHYS_TRUCK_HALF.1 + 0.05;
pub const PHYS_TRUCK_DENSITY: f32 = 3.0;
/// ZERO on purpose (playtest 1): our grip/drag forces are the entire
/// tire model, and Box3D combines contact friction as sqrt(mu_a*mu_b)
/// — any nonzero here taxed the solver truck's top speed below the pm
/// step's equilibrium, so prediction disagreed at cruise FOREVER and
/// corrections never stopped. Frictionless while DRIVING; a flipped
/// truck toggles to [`WRECK_MU`] instead (real contact friction is
/// what a wreck slides on — the hand scrape force it replaced is gone).
pub const PHYS_TRUCK_MU: f32 = 0.0;
/// Contact friction of a roof-landed wreck (combines with the ground's
/// 0.6 as sqrt(0.8 × 0.6) ≈ 0.7 — a heavy scraping stop).
pub const WRECK_MU: f32 = 0.8;

/// Box3D filter categories — the game's collision vocabulary inside
/// the solver. NOTE: Box3D's default category is ALL BITS, so every
/// body (statics included) gets categorized explicitly at creation.
/// Contact and query tests match in BOTH directions (`a.cat & b.mask`
/// and `b.cat & a.mask`).
const CATB_STATIC: u64 = 1;
const CATB_UNIT: u64 = 1 << 1;
/// Cast-only hitboxes (heli tail/rotor, flyers, retired corpses):
/// their mask is [`HITBOX_ONLY`] — a bit no body category carries —
/// so they contact NOTHING; queries pass because a query's category
/// is all-bits.
const CATB_HITBOX: u64 = 1 << 2;
const HITBOX_ONLY: u64 = 1 << 3;

/// Depth of the pose-history ring, ticks (1 s at 60 Hz — the same
/// bound the collider journal had; rewinds past it just miss).
const RING: usize = 60;

/// What a unit body IS — the map from solver body back to game entity,
/// kept beside the world for overlap results, targeting walks, and the
/// per-tick frame record.
#[derive(Clone, Copy)]
struct UnitInfo {
    owner: Id,
    part: u8,
    /// Game category (`CAT_VEHICLE`/`CAT_HOG`) — sweeps bring a mask.
    cat: u8,
    /// Vertical half-extent — targeting's altitude-band test.
    half_y: f32,
    /// Cast-side radius pad baked per unit (hog hitbox parity, boss).
    grow: f32,
}

/// One unit's pose in one historical frame — everything a rewound cast
/// needs.
#[derive(Clone, Copy)]
struct Snap {
    owner: Id,
    part: u8,
    cat: u8,
    body: b3::BodyId,
    pos: b3::Vec3,
    rot: b3::Quat,
    grow: f32,
}

/// A hit the shot cast found: who, which part, where, and how far
/// along the travel (`frac` orders competing hits — nearest wins).
#[derive(Clone, Copy)]
pub struct ShotHit {
    pub owner: Id,
    pub part: u8,
    pub frac: f32,
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

/// A targeting answer: who's near and where to aim/steer — `(x, y, z)`
/// is the unit's center with y clamped into its altitude extent (the
/// old closest-axis-point semantics), `dist` measured to that point.
#[derive(Clone, Copy)]
pub struct NearUnit {
    /// (Today's AI callers steer by position alone — the field is the
    /// query's identity half, exercised by tests.)
    #[allow(dead_code)]
    pub owner: Id,
    pub x: f32,
    pub y: f32,
    pub z: f32,
    pub dist: f32,
}

fn bv(v: pm::Vec3) -> b3::Vec3 {
    b3::Vec3 { x: v.x, y: v.y, z: v.z }
}

fn bq(q: pm::Quat) -> b3::Quat {
    b3::Quat { x: q.x, y: q.y, z: q.z, w: q.w }
}

pub struct Phys {
    pub world: b3::World,
    hogs: HashMap<Id, b3::BodyId>,
    /// Truck body + its wreck state (whether contact friction is on).
    trucks: HashMap<Id, (b3::BodyId, bool)>,
    /// Per-heli part bodies, aligned with `heli_boxes`.
    helis: HashMap<Id, Vec<b3::BodyId>>,
    flyers: HashMap<Id, b3::BodyId>,
    depots: HashMap<Id, b3::BodyId>,
    /// Solver body → game identity, LIVE units only (corpses leave it).
    by_body: HashMap<b3::BodyId, UnitInfo>,
    /// Retired bodies awaiting destruction: `(destroy_at_tick, body)`.
    /// A corpse outlives its entity by the ring depth so old frames
    /// stay castable, then goes for real.
    graveyard: Vec<(u32, b3::BodyId)>,
    /// The rewind memory: per-tick unit poses, newest at the back.
    hist: VecDeque<(u32, Vec<Snap>)>,
    /// Authored `collide.*` boxes (entity space) — the hitbox SSOT.
    heli_boxes: Vec<(u8, pm::Vec3, pm::Vec3)>,
    flyer_box: (pm::Vec3, pm::Vec3),
}

/// Placeholder so `Phys` can live in a pm SINGLE (`pm.single` needs
/// `Default`): an empty world, no statics, no shape protos — setup
/// immediately overwrites it with `Phys::new(&models)`, the same
/// seed-then-replace pattern the models registry uses.
impl Default for Phys {
    fn default() -> Phys {
        Phys {
            world: b3::World::new(b3::Vec3::default()),
            hogs: HashMap::new(),
            trucks: HashMap::new(),
            helis: HashMap::new(),
            flyers: HashMap::new(),
            depots: HashMap::new(),
            by_body: HashMap::new(),
            graveyard: Vec::new(),
            hist: VecDeque::new(),
            heli_boxes: Vec::new(),
            flyer_box: (pm::Vec3::ZERO, pm::Vec3::ZERO),
        }
    }
}

impl Phys {
    pub fn new(models: &Models) -> Phys {
        let mut world = b3::World::new(b3::Vec3 { x: 0.0, y: -9.81, z: 0.0 });
        let ident = b3::Quat::default();
        let mut statics = Vec::new();
        // Ground slab and four arena walls (tall enough that nothing
        // launches over them).
        statics.push(world.body_box(
            b3::STATIC,
            b3::Vec3 { x: 0.0, y: -0.5, z: 0.0 },
            ident,
            b3::Vec3 { x: ARENA + 20.0, y: 0.5, z: ARENA + 20.0 },
            1.0,
            0.6,
        ));
        for (x, z, hw, hd) in [
            (-ARENA - 1.0, 0.0, 1.0, ARENA + 2.0),
            (ARENA + 1.0, 0.0, 1.0, ARENA + 2.0),
            (0.0, -ARENA - 1.0, ARENA + 2.0, 1.0),
            (0.0, ARENA + 1.0, ARENA + 2.0, 1.0),
        ] {
            statics.push(world.body_box(
                b3::STATIC,
                b3::Vec3 { x, y: 10.0, z },
                ident,
                b3::Vec3 { x: hw, y: 10.0, z: hd },
                1.0,
                0.4,
            ));
        }
        for &(x, z, hw, hd, h) in BUILDINGS.iter() {
            statics.push(world.body_box(
                b3::STATIC,
                b3::Vec3 { x, y: h * 0.5, z },
                ident,
                b3::Vec3 { x: hw, y: h * 0.5, z: hd },
                1.0,
                0.6,
            ));
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
            statics.push(world.body_hull(b3::STATIC, b3::Vec3::default(), ident, &pts, 1.0, 0.5));
        }
        for s in statics {
            world.set_filter(s, CATB_STATIC, !0);
        }
        let heli_boxes = models.boxes("heli");
        let fb = models.boxes("flyer");
        Phys {
            world,
            hogs: HashMap::new(),
            trucks: HashMap::new(),
            helis: HashMap::new(),
            flyers: HashMap::new(),
            depots: HashMap::new(),
            by_body: HashMap::new(),
            graveyard: Vec::new(),
            hist: VecDeque::new(),
            heli_boxes,
            flyer_box: (fb[0].1, fb[0].2),
        }
    }

    /// A unit died (or its entity was swapped): park the body as a
    /// contact-inert, cast-only ghost until its history frames expire.
    /// The by_body prune is what makes corpses invisible to targeting
    /// and NEW frames while old frames keep hitting them.
    fn retire(&mut self, now: u32, body: b3::BodyId) {
        self.by_body.remove(&body);
        self.world.set_type(body, b3::KINEMATIC);
        self.world.set_velocity(body, b3::Vec3::default());
        self.world.set_angular_velocity(body, b3::Vec3::default());
        self.world.set_filter(body, CATB_HITBOX, HITBOX_ONLY);
        self.graveyard.push((now + RING as u32 + 2, body));
    }

    /// Membership + mirrors + step + readback + frame record, in that
    /// order — runs EARLY in the tick (prio 26), so this tick's
    /// AI/drive tasks read fresh poses and the velocities/forces they
    /// write are consumed by the NEXT tick's step.
    #[allow(clippy::too_many_arguments)]
    pub fn tick(
        &mut self,
        now: u32,
        hogs: &mut pm::Pool<Hog>,
        trucks: &mut pm::Pool<Truck>,
        helis: &mut pm::Pool<Heli>,
        flyers: &pm::Pool<Flyer>,
        depots: &pm::Pool<Depot>,
        boss: &pm::Pool<Boss>,
        boss_grow: f32,
    ) {
        // -- the graveyard: corpses whose frames have all expired go
        // for real.
        self.graveyard.retain(|&(at, body)| {
            if now < at {
                return true;
            }
            self.world.destroy(body);
            false
        });

        // -- membership: bodies follow pool entries; the dead retire.
        let mut dead: Vec<b3::BodyId> = Vec::new();
        self.hogs.retain(|id, body| hogs.contains(*id) || { dead.push(*body); false });
        self.trucks.retain(|id, (body, _)| trucks.contains(*id) || { dead.push(*body); false });
        self.helis
            .retain(|id, bodies| helis.contains(*id) || { dead.extend(bodies.iter().copied()); false });
        self.flyers.retain(|id, body| flyers.contains(*id) || { dead.push(*body); false });
        self.depots.retain(|id, body| depots.contains(*id) || { dead.push(*body); false });
        for body in dead {
            self.retire(now, body);
        }

        for (id, h) in hogs.iter() {
            let (world, by_body) = (&mut self.world, &mut self.by_body);
            self.hogs.entry(id).or_insert_with(|| {
                let b = world.body_capsule(
                    b3::DYNAMIC,
                    b3::Vec3 {
                        x: h.body.pos.x,
                        y: PHYS_HOG_HALF_H + PHYS_HOG_R + 0.01,
                        z: h.body.pos.z,
                    },
                    PHYS_HOG_HALF_H,
                    PHYS_HOG_R,
                    2.0,
                    0.3,
                    false,
                );
                world.lock_rotation(b);
                world.set_filter(b, CATB_UNIT, !0);
                // The boss is a hog wearing a grown hitbox — the cast
                // pad matches the spectacle (`boss_scale` on the model).
                let grow = HOG_GROW + if boss.contains(id) { boss_grow } else { 0.0 };
                by_body.insert(
                    b,
                    UnitInfo {
                        owner: id,
                        part: PART_BODY,
                        cat: CAT_HOG,
                        half_y: PHYS_HOG_HALF_H + PHYS_HOG_R,
                        grow,
                    },
                );
                b
            });
        }
        let lift = PHYS_TRUCK_LIFT;
        for (id, t) in trucks.iter() {
            let (world, by_body) = (&mut self.world, &mut self.by_body);
            let center =
                b3::Vec3 { x: t.body.pos.x, y: t.body.pos.y + lift, z: t.body.pos.z };
            let (body, _) = *self.trucks.entry(id).or_insert_with(|| {
                let b = world.body_box(
                    b3::DYNAMIC,
                    center,
                    bq(t.body.rot),
                    b3::Vec3 {
                        x: PHYS_TRUCK_HALF.0,
                        y: PHYS_TRUCK_HALF.1,
                        z: PHYS_TRUCK_HALF.2,
                    },
                    PHYS_TRUCK_DENSITY,
                    PHYS_TRUCK_MU,
                );
                world.set_filter(b, CATB_UNIT, !0);
                by_body.insert(
                    b,
                    UnitInfo {
                        owner: id,
                        part: PART_BODY,
                        cat: CAT_VEHICLE,
                        half_y: PHYS_TRUCK_HALF.1,
                        grow: 0.0,
                    },
                );
                (b, false)
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
        // TODO(box3d-move): the heli is the last pm-stepped vehicle —
        // this kinematic mirror (plus heli_step's hand clamps) becomes
        // a real dynamic solver body when local solving lands (master
        // note atop this file).
        for (id, h) in helis.iter() {
            let (world, by_body, boxes) =
                (&mut self.world, &mut self.by_body, &self.heli_boxes);
            let bodies = self.helis.entry(id).or_insert_with(|| {
                boxes
                    .iter()
                    .map(|&(part, center, half)| {
                        let b = world.body_box(
                            b3::KINEMATIC,
                            bv(h.body.pos + h.body.rot.rotate(center)),
                            bq(h.body.rot),
                            bv(half),
                            1.0,
                            0.3,
                        );
                        // The cabin is the contact-active presence the
                        // crowd bumps; tail boom and rotor disc are
                        // cast-only hitboxes.
                        if part == PART_BODY {
                            world.set_filter(b, CATB_UNIT, !0);
                        } else {
                            world.set_filter(b, CATB_HITBOX, HITBOX_ONLY);
                        }
                        by_body.insert(
                            b,
                            UnitInfo {
                                owner: id,
                                part,
                                cat: CAT_VEHICLE,
                                half_y: half.y,
                                grow: 0.0,
                            },
                        );
                        b
                    })
                    .collect()
            });
            // Kinematic mirror: the pm-stepped pod IS the pose, every
            // part riding the body frame.
            for (i, &body) in bodies.iter().enumerate() {
                let center = boxes[i].1;
                self.world.set_pose(body, bv(h.body.pos + h.body.rot.rotate(center)), bq(h.body.rot));
            }
        }
        for (id, f) in flyers.iter() {
            let (world, by_body, (center, half)) =
                (&mut self.world, &mut self.by_body, self.flyer_box);
            let body = *self.flyers.entry(id).or_insert_with(|| {
                let b = world.body_box(
                    b3::KINEMATIC,
                    bv(f.body.pos + f.body.rot.rotate(center)),
                    bq(f.body.rot),
                    bv(half),
                    1.0,
                    0.0,
                );
                world.set_filter(b, CATB_HITBOX, HITBOX_ONLY);
                by_body.insert(
                    b,
                    UnitInfo { owner: id, part: PART_BODY, cat: CAT_HOG, half_y: half.y, grow: 0.0 },
                );
                b
            });
            self.world.set_pose(body, bv(f.body.pos + f.body.rot.rotate(center)), bq(f.body.rot));
        }
        for (id, d) in depots.iter() {
            let (world, by_body) = (&mut self.world, &mut self.by_body);
            self.depots.entry(id).or_insert_with(|| {
                let b = world.body_box(
                    b3::STATIC,
                    b3::Vec3 { x: d.x, y: DEPOT_H * 0.5, z: d.z },
                    b3::Quat::default(),
                    b3::Vec3 { x: DEPOT_R, y: DEPOT_H * 0.5, z: DEPOT_R },
                    1.0,
                    0.6,
                );
                world.set_filter(b, CATB_UNIT, !0);
                by_body.insert(
                    b,
                    UnitInfo {
                        owner: id,
                        part: PART_BODY,
                        cat: CAT_VEHICLE,
                        half_y: DEPOT_H * 0.5,
                        grow: 0.0,
                    },
                );
                b
            });
        }

        // -- step.
        self.world.step(FIXED_DT, 4);

        // -- readback: solver poses become pod truth.
        for (id, body) in &self.hogs {
            if let Some(mut h) = hogs.get_mut(*id) {
                let (p, _) = self.world.pose(*body);
                h.body.pos = pm::vec3(p.x, (p.y - PHYS_HOG_HALF_H - PHYS_HOG_R).max(0.0), p.z);
                let v = self.world.velocity(*body);
                h.body.vel = pm::vec3(v.x, v.y, v.z);
                // rot stays the AI's yaw write (the capsule is
                // angular-locked, so the solver has no opinion —
                // UNTIL knockdown/flop unlocks it, and then this is
                // where the tumble quat flows to every client).
            }
        }
        for (id, (body, _)) in &self.trucks {
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

        // -- the frame record: every live unit's pose, one ring entry
        // per tick. This is the whole lag-comp memory.
        let mut frame = Vec::with_capacity(self.by_body.len());
        for (&body, u) in &self.by_body {
            let (pos, rot) = self.world.pose(body);
            frame.push(Snap { owner: u.owner, part: u.part, cat: u.cat, body, pos, rot, grow: u.grow });
        }
        self.hist.push_back((now, frame));
        while self.hist.len() > RING {
            self.hist.pop_front();
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
    /// steering as ang.y only. A FLIPPED truck is a wreck until
    /// recovered: no engine, no grip — and the moment it flips its
    /// shapes toggle to real Box3D contact friction ([`WRECK_MU`]),
    /// which is what it scrapes to a stop on. Recovery toggles back to
    /// frictionless so cruise equilibrium keeps matching the shared
    /// step.
    pub fn truck_drive(&mut self, id: Id, t: &Truck, cmd: Drive, boosting: bool, p: &Params) {
        let Some(&mut (body, ref mut wrecked)) = self.trucks.get_mut(&id) else {
            return;
        };
        let (_, q) = self.world.pose(body);
        let upness = 1.0 - 2.0 * (q.x * q.x + q.z * q.z);
        let now_wrecked = upness < 0.5;
        if now_wrecked != *wrecked {
            *wrecked = now_wrecked;
            self.world.set_friction(body, if now_wrecked { WRECK_MU } else { PHYS_TRUCK_MU });
        }
        if now_wrecked {
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
        let mass =
            PHYS_TRUCK_DENSITY * 8.0 * PHYS_TRUCK_HALF.0 * PHYS_TRUCK_HALF.1 * PHYS_TRUCK_HALF.2;
        self.world.force(
            body,
            b3::Vec3 {
                x: mass * (f_fwd * fx + f_lat * rx),
                y: 0.0,
                z: mass * (f_fwd * fz + f_lat * rz),
            },
        );
    }

    /// THE shot judgment — one tick of a bullet's travel. Statics clip
    /// in PRESENT time (they don't move); units are cast per body at
    /// their pose in the shooter's `view` frame out of the history
    /// ring (favor-the-shooter). `pad` is the shot's hit-circle
    /// forgiveness and fattens HOGS only (a pad never grows a
    /// teammate); `skip` drops the shooter's own vehicle. Returns the
    /// unit hit if one wins, else the static hit point (for the dirt/
    /// wall flash) if the travel died — `(None, None)` means the shot
    /// flies on.
    pub fn cast_shot(
        &self,
        view: u32,
        from: pm::Vec3,
        dir: pm::Vec3,
        dist: f32,
        pad: f32,
        mask: u8,
        skip: Option<Id>,
    ) -> (Option<ShotHit>, Option<pm::Vec3>) {
        let o = bv(from);
        let tv = bv(dir * dist);
        let wall = self.world.cast_ray(o, tv, CATB_STATIC);
        // The shooter's frame: exact tick if it's in the ring, the
        // newest at-or-before it otherwise (a present-time shooter's
        // view IS the newest frame). Older than the ring = no unit
        // test — the shot just flies (the old journal's contract).
        let frame = self.hist.iter().rev().find(|(t, _)| *t <= view).map(|(_, f)| f);
        let mut best: Option<ShotHit> = None;
        if let Some(frame) = frame {
            for s in frame {
                if s.cat & mask == 0 || Some(s.owner) == skip {
                    continue;
                }
                let r = s.grow + if s.cat == CAT_HOG { pad } else { 0.0 };
                if let Some((p, f)) = self.world.body_cast_sphere(s.body, (s.pos, s.rot), o, r, tv)
                    && best.as_ref().is_none_or(|b| f < b.frac)
                {
                    best =
                        Some(ShotHit { owner: s.owner, part: s.part, frac: f, x: p.x, y: p.y, z: p.z });
                }
            }
        }
        match (best, wall) {
            (Some(b), Some((_, wf))) if b.frac >= wf => {
                let (wp, _) = wall.unwrap();
                (None, Some(pm::vec3(wp.x, wp.y, wp.z)))
            }
            (Some(b), _) => (Some(b), None),
            (None, Some((wp, _))) => (None, Some(pm::vec3(wp.x, wp.y, wp.z))),
            (None, None) => (None, None),
        }
    }

    /// The bite verb: nearest unit in `mask` whose live solver shape
    /// overlaps the capsule `p1..p2` at radius `r` (a vertical segment
    /// = the old circle+band reach). Present time — bites are
    /// server-side AI, no lag to compensate. Returns who and which
    /// part (a hog flanking a heli bites the tail it's next to).
    pub fn touch_unit(&self, p1: pm::Vec3, p2: pm::Vec3, r: f32, mask: u8) -> Option<(Id, u8)> {
        let mid = (p1 + p2) * 0.5;
        let mut best: Option<(f32, Id, u8)> = None;
        for body in self.world.overlap_capsule(bv(p1), bv(p2), r, CATB_UNIT | CATB_HITBOX) {
            let Some(u) = self.by_body.get(&body) else { continue };
            if u.cat & mask == 0 {
                continue;
            }
            let (p, _) = self.world.pose(body);
            let d2 = (p.x - mid.x).powi(2) + (p.y - mid.y).powi(2) + (p.z - mid.z).powi(2);
            if best.is_none_or(|(bd, _, _)| d2 < bd) {
                best = Some((d2, u.owner, u.part));
            }
        }
        best.map(|(_, owner, part)| (owner, part))
    }

    /// The targeting verb: nearest unit in `mask` whose altitude
    /// extent overlaps `band`, within `within` of `q` — the band IS
    /// the reach criterion (a hog passes `(0, hog_leap)` and a
    /// climbing heli simply stops existing for it). Walks the live
    /// unit map — a linear scan over ~tens of vehicles, the same
    /// count the old query tree pruned to anyway.
    pub fn nearest_unit(
        &self,
        q: pm::Vec3,
        within: f32,
        band: (f32, f32),
        mask: u8,
    ) -> Option<NearUnit> {
        let mut best: Option<NearUnit> = None;
        for (&body, u) in &self.by_body {
            if u.cat & mask == 0 {
                continue;
            }
            let (p, _) = self.world.pose(body);
            if p.y - u.half_y > band.1 || p.y + u.half_y < band.0 {
                continue;
            }
            let py = q.y.clamp(p.y - u.half_y, p.y + u.half_y);
            let d = ((p.x - q.x).powi(2) + (py - q.y).powi(2) + (p.z - q.z).powi(2)).sqrt();
            if d <= within && best.is_none_or(|b| d < b.dist) {
                best = Some(NearUnit { owner: u.owner, x: p.x, y: py, z: p.z, dist: d });
            }
        }
        best
    }
}

/// The lag-comp contracts, checked against the real solver: shots are
/// judged in the shooter's frame (rewound poses hit where the shooter
/// saw the hog, not where it is), corpses eat rounds until their
/// frames expire, statics clip in present time, and the bite overlap
/// speaks the same geometry.
#[cfg(test)]
mod phys_tests {
    use super::*;

    fn hog_at(x: f32, z: f32) -> Hog {
        Hog {
            body: pm::Body { pos: pm::vec3(x, 0.0, z), ..Default::default() },
            hp: 1.0,
        }
    }

    /// A Phys with one hog, run `ticks` ticks of straight +x walking
    /// at 10 u/s; returns the world, the pools, and the hog's pod x
    /// at every tick (the readback truth the casts are judged against).
    fn walked_world(ticks: u32) -> (Phys, pm::Pool<Hog>, Id, Vec<f32>) {
        let models = Models::load();
        let mut ph = Phys::new(&models);
        let mut hogs = pm::Pool::new();
        let mut trucks = pm::Pool::new();
        let mut helis = pm::Pool::new();
        let flyers = pm::Pool::new();
        let depots = pm::Pool::new();
        let boss = pm::Pool::new();
        let id = Id::new(0, 0, 1);
        hogs.add(id, hog_at(0.0, 0.0));
        let mut xs = Vec::new();
        for t in 0..ticks {
            ph.tick(t, &mut hogs, &mut trucks, &mut helis, &flyers, &depots, &boss, 0.0);
            ph.hog_velocity(id, 10.0, 0.0);
            xs.push(hogs.get(id).unwrap().body.pos.x);
        }
        (ph, hogs, id, xs)
    }

    /// A flat +z shot crossing `x` at capsule height.
    fn shot_crossing(ph: &Phys, view: u32, x: f32) -> Option<ShotHit> {
        ph.cast_shot(view, pm::vec3(x, 0.7, -5.0), pm::vec3(0.0, 0.0, 1.0), 10.0, 0.0, CAT_HOG, None)
            .0
    }

    #[test]
    fn shots_land_in_the_shooters_frame() {
        let (ph, _hogs, id, xs) = walked_world(80);
        let now = 79;
        // Present frame: the hog is where the pod says.
        let hit = shot_crossing(&ph, now, xs[79]).expect("present-frame shot connects");
        assert_eq!(hit.owner, id);
        // Rewound 40 ticks: the hog is hit where it WAS...
        assert!(shot_crossing(&ph, now - 40, xs[39]).is_some(), "rewound shot hits the old spot");
        // ...and not where it is now (it walked ~6u since).
        assert!(shot_crossing(&ph, now - 40, xs[79]).is_none(), "rewound frame misses the present");
        // Older than the ring: no unit test at all.
        assert!(shot_crossing(&ph, 2, xs[2]).is_none(), "past the ring nothing is judged");
    }

    #[test]
    fn corpses_eat_rounds_until_their_frames_expire() {
        let (mut ph, mut hogs, id, xs) = walked_world(80);
        let mut trucks = pm::Pool::new();
        let mut helis = pm::Pool::new();
        let (flyers, depots, boss) = (pm::Pool::new(), pm::Pool::new(), pm::Pool::new());
        hogs.remove(id);
        ph.tick(80, &mut hogs, &mut trucks, &mut helis, &flyers, &depots, &boss, 0.0);
        // A round judged in a pre-death frame still lands where the
        // shooter saw meat (no contact will reach the dead owner —
        // that's the response tasks' id_alive check, not ours).
        assert!(shot_crossing(&ph, 75, xs[75]).is_some(), "the fresh corpse eats the round");
        // Present frames no longer contain it.
        assert!(shot_crossing(&ph, 80, xs[79]).is_none(), "the corpse is gone from the present");
        // Run the ring past the death: the frames expire, the body is
        // destroyed, and old-view casts just miss — no crash.
        for t in 81..145 {
            ph.tick(t, &mut hogs, &mut trucks, &mut helis, &flyers, &depots, &boss, 0.0);
        }
        assert!(shot_crossing(&ph, 75, xs[75]).is_none(), "expired frames judge nothing");
    }

    #[test]
    fn statics_clip_and_units_shield() {
        let (ph, _hogs, id, xs) = walked_world(80);
        // North up the x=0 lane (building-free, checked against the
        // BUILDINGS table): the arena wall stops the shot.
        let (hit, wall) = ph.cast_shot(
            79,
            pm::vec3(0.0, 1.0, 60.0),
            pm::vec3(0.0, 0.0, 1.0),
            100.0,
            0.0,
            CAT_HOG,
            None,
        );
        assert!(hit.is_none());
        let wp = wall.expect("the arena wall stops the shot");
        assert!((wp.z - ARENA).abs() < 0.2, "near face of the wall, got {}", wp.z);
        // The hog stands between the muzzle and the far wall: it wins
        // (the walked hog sits at x≈13, clear of every building on the
        // z lane).
        let (hit, wall) = ph.cast_shot(
            79,
            pm::vec3(xs[79], 0.7, -20.0),
            pm::vec3(0.0, 0.0, 1.0),
            2.0 * ARENA,
            0.0,
            CAT_HOG,
            None,
        );
        assert!(wall.is_none(), "the hog eats it before the north wall");
        assert_eq!(hit.expect("unit on the line").owner, id);
    }

    #[test]
    fn touch_reaches_the_live_shape() {
        let models = Models::load();
        let mut ph = Phys::new(&models);
        let mut hogs = pm::Pool::new();
        let mut trucks = pm::Pool::new();
        let mut helis = pm::Pool::new();
        let (flyers, depots, boss) = (pm::Pool::new(), pm::Pool::new(), pm::Pool::new());
        let tid = Id::new(0, 0, 9);
        trucks.add(
            tid,
            Truck {
                body: pm::Body { pos: pm::vec3(0.0, 0.0, 0.0), ..Default::default() },
                ..Default::default()
            },
        );
        for t in 0..3 {
            ph.tick(t, &mut hogs, &mut trucks, &mut helis, &flyers, &depots, &boss, 0.0);
        }
        let leap = Params::default().hog_leap;
        let touch = |x: f32| {
            ph.touch_unit(pm::vec3(x, 0.0, 0.0), pm::vec3(x, leap, 0.0), HOG_R, CAT_VEHICLE)
        };
        // The truck box's +x face sits at 0.9: HOG_R reaches from 1.5,
        // not from 2.0.
        assert_eq!(touch(1.5), Some((tid, PART_BODY)), "nose contact bites");
        assert!(touch(2.0).is_none(), "clear of the box");
        // Targeting: the same truck is the nearest vehicle, its aim
        // point y clamped into the box's extent.
        let n = ph.nearest_unit(pm::vec3(10.0, 0.0, 0.0), 100.0, (0.0, leap), CAT_VEHICLE).unwrap();
        assert_eq!(n.owner, tid);
        assert!(n.dist < 10.2 && n.y <= leap, "closest-point distance, got {} at y {}", n.dist, n.y);
    }
}
