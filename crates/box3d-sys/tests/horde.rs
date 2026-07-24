//! Spike 2 of the Box3D adoption plan (the TODO(roadmap) above
//! BUILDINGS in hogs-sim): THE HORDE AT SCALE. Two questions:
//!
//! 1. Budget — 300 wandering, mutually-colliding hog capsules: what
//!    does a step cost awake, and does island sleeping actually zero
//!    the bill when the horde settles? Run `cargo test -p box3d-sys
//!    --release --test horde -- --nocapture` for the real numbers
//!    (assertions here are generous debug-safe ceilings; the printed
//!    µs/step is the datum the plan wants).
//! 2. The Connor qualifier "mass you can feel" — is a packed crowd
//!    physically load-bearing? A force-driven truck must measurably
//!    LOSE SPEED plowing through 200 packed hogs, and the hogs must
//!    visibly jostle aside. If that falls out of the solver with zero
//!    gameplay code, qualifier #2 is physics, not scripting.

use box3d_sys::*;
use std::time::Instant;

fn v(x: f32, y: f32, z: f32) -> Vec3 {
    Vec3 { x, y, z }
}

const DT: f32 = 1.0 / 60.0;
const SUBSTEPS: i32 = 4;

/// Hog-ish capsule: ~1.4 tall, 0.4 wide, upright-locked.
const HOG_HALF_H: f32 = 0.3;
const HOG_R: f32 = 0.4;

fn ground(w: &mut World) {
    w.body_box(STATIC, v(0.0, -0.5, 0.0), Quat::default(), v(120.0, 0.5, 120.0), 1.0, 0.6);
}

/// THE HOG IDIOM (spike-2 finding): full angular lock, not just
/// upright. An upright capsule is rotationally symmetric about y, so
/// yaw is collision-IRRELEVANT — heading stays game-side pod data
/// (exactly like today). And it's load-bearing for sleep: Box3D's
/// sleep velocity includes the angular arc, capsule caps have no
/// torsional friction, so a yaw-free hog nudged into a spin spins
/// FOREVER and its whole island never sleeps (found the hard way:
/// 6/300 asleep at zero linear velocity). Density 2.0 ≈ 150 kg boar
/// against the ~24-unit truck — a crowd that weighs something.
fn spawn_hog(w: &mut World, x: f32, z: f32) -> BodyId {
    let h = w.body_capsule(DYNAMIC, v(x, HOG_HALF_H + HOG_R + 0.01, z), HOG_HALF_H, HOG_R, 2.0, 0.3, false);
    w.lock_rotation(h);
    h
}

/// Deterministic wander velocities — a stand-in for the hog AI task
/// writing intents (the real integration drives the same verb).
fn wander(i: usize, tick: u32) -> Vec3 {
    let seed = (i as u32).wrapping_mul(2654435761).wrapping_add(tick / 30);
    let a = (seed % 6283) as f32 / 1000.0;
    v(a.sin() * 4.5, 0.0, a.cos() * 4.5)
}

#[test]
fn horde_300_budget_and_sleep() {
    let mut w = World::new(v(0.0, -9.81, 0.0));
    ground(&mut w);
    // 300 hogs on a 20×15 grid, 2 u spacing — dense enough that
    // wander paths cross constantly.
    let hogs: Vec<BodyId> = (0..300)
        .map(|i| spawn_hog(&mut w, (i % 20) as f32 * 2.0 - 19.0, (i / 20) as f32 * 2.0 - 14.0))
        .collect();

    // Phase A: 10 s of full-crowd wandering, everyone awake.
    let t0 = Instant::now();
    for tick in 0..600u32 {
        if tick % 30 == 0 {
            for (i, &h) in hogs.iter().enumerate() {
                let mut vel = wander(i, tick);
                // Keep whatever vertical the solver gave (gravity).
                vel.y = w.velocity(h).y;
                w.set_velocity(h, vel);
            }
        }
        w.step(DT, SUBSTEPS);
    }
    let awake_us = t0.elapsed().as_micros() as f32 / 600.0;

    for &h in &hogs {
        let (p, _) = w.pose(h);
        assert!(p.y > 0.0 && p.y < 5.0, "hog stayed on the floor, y {}", p.y);
        assert!(p.x.is_finite() && p.z.is_finite());
    }

    // Phase B: brains go quiet; the pile must SLEEP and the bill must
    // collapse — a sleeping horde is free CPU and (in pm) free wire.
    for &h in &hogs {
        let mut vel = w.velocity(h);
        vel.x = 0.0;
        vel.z = 0.0;
        w.set_velocity(h, vel);
    }
    for _ in 0..300 {
        w.step(DT, SUBSTEPS);
    }
    let t1 = Instant::now();
    for _ in 0..300 {
        w.step(DT, SUBSTEPS);
    }
    let asleep_us = t1.elapsed().as_micros() as f32 / 300.0;
    let asleep = hogs.iter().filter(|&&h| !w.awake(h)).count();

    println!("horde 300: awake {awake_us:.0} µs/step, settled {asleep_us:.0} µs/step, asleep {asleep}/300");
    assert!(asleep >= 240, "a settled horde must sleep, asleep {asleep}/300");
    assert!(
        asleep_us < awake_us * 0.5,
        "sleeping must collapse the bill: awake {awake_us:.0} vs settled {asleep_us:.0} µs/step"
    );
    // Generous ceiling so debug runs pass; release is the real datum.
    assert!(awake_us < 16_000.0, "a 300-hog step must fit a frame even in debug, {awake_us:.0} µs");
}

#[test]
fn truck_bogs_down_in_the_crowd() {
    // The truck: a heavy box driven by constant forward force with
    // linear damping, so open ground has a steady top speed — any
    // speed lost in the crowd is hog mass, not tuning.
    let drive = |crowd: bool| -> (f32, f32, Vec<f32>) {
        let mut w = World::new(v(0.0, -9.81, 0.0));
        ground(&mut w);
        let truck = w.body_box(DYNAMIC, v(0.0, 0.8, -40.0), Quat::default(), v(0.9, 0.7, 1.6), 3.0, 0.4);
        w.lock_rotation(truck);
        w.set_damping(truck, 1.2);
        let mut hogs = Vec::new();
        if crowd {
            // 200 hogs packed at 1.1 u spacing straight across the
            // truck's path — shoulder-to-shoulder, ~15 ranks deep.
            for i in 0..200 {
                hogs.push(spawn_hog(&mut w, (i % 13) as f32 * 1.1 - 6.6, (i / 13) as f32 * 0.9));
            }
        }
        // F = m·damping·14 — ground friction (μ 0.4 on a sliding box)
        // taxes that to a steady ~10 m/s in the open, identically in
        // both runs, so the crowd delta is pure hog mass.
        let (mut in_crowd_speed, mut samples) = (0.0, 0);
        for _ in 0..900 {
            let mass = 3.0 * (1.8 * 1.4 * 3.2); // density × box volume
            w.force(truck, v(0.0, 0.0, mass * 1.2 * 14.0));
            w.step(DT, SUBSTEPS);
            let (p, _) = w.pose(truck);
            if (0.0..14.0).contains(&p.z) {
                in_crowd_speed += w.velocity(truck).z;
                samples += 1;
            }
        }
        // Ground-plane displacement from the spawn slot — a plow both
        // bulldozes forward and parts sideways; either counts as
        // "the crowd physically moved".
        let displaced = hogs
            .iter()
            .enumerate()
            .map(|(i, &h)| {
                let (p, _) = w.pose(h);
                let (dx, dz) = (p.x - ((i % 13) as f32 * 1.1 - 6.6), p.z - (i / 13) as f32 * 0.9);
                (dx * dx + dz * dz).sqrt()
            })
            .collect();
        let open_speed = w.velocity(truck).z;
        (in_crowd_speed / samples.max(1) as f32, open_speed, displaced)
    };

    let (open_zone_speed, open_final, _) = drive(false);
    let (crowd_zone_speed, _, displaced) = drive(true);
    println!(
        "bog-down: open {open_zone_speed:.1} m/s through the band (steady {open_final:.1}), crowd {crowd_zone_speed:.1} m/s"
    );
    assert!(open_zone_speed > 9.0, "open-ground control run reaches speed, {open_zone_speed:.1}");
    assert!(
        crowd_zone_speed < open_zone_speed * 0.75,
        "200 packed hogs must cost real speed: {crowd_zone_speed:.1} vs {open_zone_speed:.1} m/s"
    );
    // And the crowd itself got shoved: hogs near the path jostled aside.
    let moved = displaced.iter().filter(|d| **d > 0.5).count();
    println!("bog-down: {moved}/200 hogs displaced > 0.5 u");
    assert!(moved > 30, "the plow must visibly part the crowd, moved {moved}");
}
