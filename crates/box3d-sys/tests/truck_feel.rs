//! Spike 3 of the Box3D adoption plan: FEEL PARITY. The integration
//! doctrine is "our steps become force generators" — so this file
//! drives a dynamic Box3D box with literally the truck_step laws
//! (steer turns the chassis, engine force + rolling drag along
//! forward, grip force bleeds lateral momentum, boost loosens grip)
//! and asserts the invariants hogs-sim pins for the pm step. If those
//! hold, the feel Connor tuned survives the solver — and everything
//! the solver adds (tumble, contact, multi-level) is pure upside.
//!
//! Also here: the FULL TUMBLE gate (qualifier 3 — launch off a ramp
//! with rotation free, pick up real spin, land, come to rest in
//! whatever orientation gravity chose), a multi-level sanity pass
//! (qualifier 4 — drive UNDER a slab something is parked ON), and the
//! b3WheelJoint audition (real suspension, spin motors).
//!
//! Solver-vs-step differences to expect and own: ground contact
//! friction exists here (μ deliberately low — OUR grip forces are the
//! tire model; solver friction is for collisions), and contact acts at
//! the patch, not the CoM, so hard maneuvers pitch the chassis — the
//! weight-transfer MotorStorm wants, for free.

use box3d_sys::*;
use std::f32::consts::FRAC_PI_2;

fn v(x: f32, y: f32, z: f32) -> Vec3 {
    Vec3 { x, y, z }
}

const DT: f32 = 1.0 / 60.0;
const SUBSTEPS: i32 = 4;

/// pm's truck laws (hogs-sim Params defaults, the played-in numbers).
const ACCEL: f32 = 14.0;
const BOOST_ACCEL: f32 = 26.0;
const DRAG: f32 = 1.2;
const GRIP: f32 = 8.0;
const GRIP_BOOST: f32 = 3.2;
const TRUCK_HALF: (f32, f32, f32) = (0.9, 0.7, 1.6);
const TRUCK_DENSITY: f32 = 3.0;
/// Low contact friction ON PURPOSE: grip forces are the tire model,
/// contact friction is for collisions. Box3D combines pair friction
/// as √(μa·μb) (measured: 0.15 vs 0.6 ground taxed exactly √0.09 g),
/// so keep the truck's own μ tiny for a near-pm rolling tax.
const TRUCK_MU: f32 = 0.05;

fn ground(w: &mut World) {
    w.body_box(STATIC, v(0.0, -0.5, 0.0), Quat::default(), v(150.0, 0.5, 150.0), 1.0, 0.6);
}

fn quat_yaw(yaw: f32) -> Quat {
    Quat { x: 0.0, y: (yaw * 0.5).sin(), z: 0.0, w: (yaw * 0.5).cos() }
}

fn quat_pitch(a: f32) -> Quat {
    Quat { x: (a * 0.5).sin(), y: 0.0, z: 0.0, w: (a * 0.5).cos() }
}

fn spawn_truck(w: &mut World, pos: Vec3) -> (BodyId, f32) {
    let t = w.body_box(
        DYNAMIC,
        pos,
        Quat::default(),
        v(TRUCK_HALF.0, TRUCK_HALF.1, TRUCK_HALF.2),
        TRUCK_DENSITY,
        TRUCK_MU,
    );
    let mass = TRUCK_DENSITY * 8.0 * TRUCK_HALF.0 * TRUCK_HALF.1 * TRUCK_HALF.2;
    (t, mass)
}

/// Yaw from the body quat — what heading() reads off pm::Body.
fn yaw_of(q: Quat) -> f32 {
    let fwd_x = 2.0 * (q.x * q.z + q.w * q.y);
    let fwd_z = 1.0 - 2.0 * (q.x * q.x + q.y * q.y);
    fwd_x.atan2(fwd_z)
}

/// One tick of truck_step's laws expressed as forces on the solver
/// body: OUR feel, Box3D's contacts. Returns current forward speed.
///
/// THE WHEELS-DOWN GATE (spike-3 finding): engine, grip, and steering
/// only exist while the truck is on its wheels. The first tumble run
/// omitted this — the truck flipped onto its roof at the lip (real
/// tumble, first try!), the inverted heading read as reversed, and
/// the still-flooring throttle rocketed the wreck 100 u backwards on
/// its roof. A roof-landed truck is a WRECK until recovered — which
/// is exactly the qualifier-3 recovery moment, surfacing straight
/// from the force model.
fn drive_tick(w: &mut World, truck: BodyId, mass: f32, thrust: f32, steer: f32, boost: bool) -> f32 {
    let (_, q) = w.pose(truck);
    let upness = 1.0 - 2.0 * (q.x * q.x + q.z * q.z); // body-up · world-up
    if upness < 0.5 {
        w.step(DT, SUBSTEPS);
        return 0.0;
    }
    let heading = yaw_of(q);
    let (fx, fz) = (heading.sin(), heading.cos());
    let (rx, rz) = (heading.cos(), -heading.sin());
    let vel = w.velocity(truck);
    let vf = vel.x * fx + vel.z * fz;
    let vl = vel.x * rx + vel.z * rz;
    // Steering: chassis yaw rate, authority scaling straight from the
    // pm step. Written as angular velocity Y ONLY — x/z stay the
    // solver's, which is what keeps tumble real.
    let authority = (vf.abs() / 6.0).min(1.0);
    let ang = w.angular_velocity(truck);
    w.set_angular_velocity(truck, v(ang.x, steer * 2.2 * authority * vf.signum(), ang.z));
    // Engine + rolling drag along forward, grip across it: F = m·a.
    let accel = if boost { BOOST_ACCEL } else { ACCEL };
    let grip = if boost { GRIP_BOOST } else { GRIP };
    let f_fwd = thrust * accel - DRAG * vf;
    let f_lat = -grip * vl;
    w.force(truck, v(mass * (f_fwd * fx + f_lat * rx), 0.0, mass * (f_fwd * fz + f_lat * rz)));
    w.step(DT, SUBSTEPS);
    vf
}

#[test]
fn grips_out_sideways_momentum_like_the_pm_step() {
    let mut w = World::new(v(0.0, -9.81, 0.0));
    ground(&mut w);
    let (truck, mass) = spawn_truck(&mut w, v(0.0, 0.71, 0.0));
    w.set_velocity(truck, v(10.0, 0.0, 0.0)); // shoved out the doors
    for _ in 0..60 {
        drive_tick(&mut w, truck, mass, 0.0, 0.0, false);
    }
    let side = w.velocity(truck).x.abs();
    assert!(side < 0.5, "1 s of grip forces bleeds a 10 m/s side shove, kept {side}");
}

#[test]
fn top_speed_matches_the_accel_drag_equilibrium() {
    let mut w = World::new(v(0.0, -9.81, 0.0));
    ground(&mut w);
    let (truck, mass) = spawn_truck(&mut w, v(0.0, 0.71, -60.0));
    let mut vf = 0.0;
    for _ in 0..600 {
        vf = drive_tick(&mut w, truck, mass, 1.0, 0.0, false);
    }
    // pm's equilibrium is ACCEL/DRAG ≈ 11.7; solver contact friction
    // (μ 0.15) taxes a little off the top.
    assert!(
        (9.5..12.5).contains(&vf),
        "solver truck cruises at the pm step's top speed, got {vf}"
    );
}

#[test]
fn boost_loosens_grip_into_a_powerslide() {
    let side_kept = |boost: bool| {
        let mut w = World::new(v(0.0, -9.81, 0.0));
        ground(&mut w);
        let (truck, mass) = spawn_truck(&mut w, v(0.0, 0.71, 0.0));
        w.set_velocity(truck, v(8.0, 0.0, 6.0));
        for _ in 0..12 {
            drive_tick(&mut w, truck, mass, 1.0, 0.0, boost);
        }
        w.velocity(truck).x.abs()
    };
    let (loose, tight) = (side_kept(true), side_kept(false));
    assert!(
        loose > tight * 1.5,
        "boost powerslides on the solver too: {loose} vs {tight}"
    );
}

/// Qualifier 3, FULL TUMBLE: boost off a steep ramp with rotation
/// free. The lip imparts real spin, the truck flies, crashes, and
/// comes to rest in whatever orientation physics chose — a state, not
/// a scripted wreck. (In-game, recovery/reset becomes gameplay.)
#[test]
fn ramp_launch_tumbles_and_comes_to_rest() {
    let mut w = World::new(v(0.0, -9.81, 0.0));
    ground(&mut w);
    // A 15° kicker slab, entry lip buried just below grade so the
    // truck rolls on, top edge ~3.7 u up.
    w.body_box(STATIC, v(0.0, 2.1, 0.0), quat_pitch(-15.0f32.to_radians()), v(4.0, 0.5, 8.0), 1.0, 0.5);
    let (truck, mass) = spawn_truck(&mut w, v(0.0, 0.71, -50.0));
    let (mut apex, mut max_spin) = (0.0f32, 0.0f32);
    for _ in 0..900 {
        // Boost at the ramp; coast once past it (or the wreck would
        // boost itself off the edge of the test world).
        let (p, _) = w.pose(truck);
        let thrust = if p.z < 20.0 { 1.0 } else { 0.0 };
        drive_tick(&mut w, truck, mass, thrust, 0.0, true);
        let (p, _) = w.pose(truck);
        apex = apex.max(p.y);
        let a = w.angular_velocity(truck);
        max_spin = max_spin.max((a.x * a.x + a.z * a.z).sqrt());
        assert!(p.y > -1.0 && p.y.is_finite(), "no tunneling, no NaN");
    }
    // Let it die down with no inputs at all.
    for _ in 0..600 {
        w.step(DT, SUBSTEPS);
    }
    let (p, q) = w.pose(truck);
    let vel = w.velocity(truck);
    let speed = (vel.x * vel.x + vel.y * vel.y + vel.z * vel.z).sqrt();
    println!(
        "tumble: apex {apex:.1} u, peak tumble rate {max_spin:.1} rad/s, rest quat ({:.2},{:.2},{:.2},{:.2})",
        q.x, q.y, q.z, q.w
    );
    assert!(apex > 2.5, "the kicker launches a boosting truck, apex {apex}");
    assert!(max_spin > 1.0, "the lip imparts REAL pitch/roll spin, peak {max_spin} rad/s");
    assert!(speed < 0.5 && p.y < 2.0, "the wreck comes to rest on the floor, v {speed} y {}", p.y);
}

/// Qualifier 4 sanity: things above drivable things. A crate sleeps ON
/// a bridge slab while the truck drives UNDER it — trivially correct
/// in a real 3D world, structurally impossible in capsule+band.
#[test]
fn drives_under_an_occupied_bridge() {
    let mut w = World::new(v(0.0, -9.81, 0.0));
    ground(&mut w);
    w.body_box(STATIC, v(0.0, 4.0, 0.0), Quat::default(), v(3.0, 0.3, 8.0), 1.0, 0.6); // the deck
    let crate_on = w.body_box(DYNAMIC, v(0.0, 5.0, 0.0), Quat::default(), v(0.5, 0.5, 0.5), 1.0, 0.6);
    let (truck, mass) = spawn_truck(&mut w, v(0.0, 0.71, -40.0));
    for _ in 0..600 {
        drive_tick(&mut w, truck, mass, 1.0, 0.0, false);
    }
    let (tp, _) = w.pose(truck);
    let (cp, _) = w.pose(crate_on);
    assert!(tp.z > 10.0, "drove clean under the deck, z {}", tp.z);
    assert!(tp.y < 2.0, "…on the ground floor, y {}", tp.y);
    assert!((cp.y - 4.8).abs() < 0.5, "the crate stayed parked on the deck, y {}", cp.y);
    assert!(!w.awake(crate_on), "…asleep the whole time (free bandwidth)");
}

/// The b3WheelJoint audition: chassis + four sphere wheels on sprung,
/// motor-driven joints. Not the integration candidate yet — the
/// force-generator truck above is — but this is the machinery
/// MotorStorm-grade suspension chatter would come from, so prove the
/// rig drives and survives a curb hit.
#[test]
fn wheel_joint_rig_drives_and_absorbs_a_bump() {
    let mut w = World::new(v(0.0, -9.81, 0.0));
    ground(&mut w);
    // A curb strip across the path.
    w.body_box(STATIC, v(0.0, 0.06, 10.0), Quat::default(), v(6.0, 0.06, 0.35), 1.0, 0.5);
    let chassis = w.body_box(DYNAMIC, v(0.0, 1.05, -20.0), Quat::default(), v(0.9, 0.35, 1.6), 2.0, 0.3);
    let mounts = [
        v(-0.95, -0.25, 1.15),
        v(0.95, -0.25, 1.15),
        v(-0.95, -0.25, -1.15),
        v(0.95, -0.25, -1.15),
    ];
    let mut joints = Vec::new();
    for m in mounts {
        let wheel = w.body_sphere(DYNAMIC, v(m.x, 1.05 + m.y - 0.25, -20.0 + m.z), 0.42, 1.5, 1.2);
        joints.push(w.wheel_joint(chassis, wheel, m, 4.0, 0.7, 400.0));
    }
    for j in &joints {
        // Spin is about the +x axle: positive spin rolls −z (ω × r at
        // the contact), so FORWARD (+z) wants negative spin.
        w.wheel_spin(*j, -24.0); // ~10 m/s ground speed at r 0.42
    }
    let mut min_y = f32::MAX;
    let mut max_y = f32::MIN;
    for _ in 0..600 {
        w.step(DT, SUBSTEPS);
        let (p, _) = w.pose(chassis);
        min_y = min_y.min(p.y);
        max_y = max_y.max(p.y);
    }
    let (p, q) = w.pose(chassis);
    println!("wheel rig: z {:.1}, chassis y range {min_y:.2}..{max_y:.2}", p.z);
    assert!(p.z > -5.0, "the motor rig drives itself forward (15+ u from spawn), z {}", p.z);
    assert!(min_y > 0.4, "suspension never bottomed the chassis into the floor, min y {min_y}");
    let upright = 1.0 - 2.0 * (q.x * q.x + q.z * q.z); // body-up · world-up
    assert!(upright > 0.8, "still on its wheels after the curb, up-ness {upright}");
}
