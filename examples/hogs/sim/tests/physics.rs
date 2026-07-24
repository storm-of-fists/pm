//! The force model's invariants, pinned so a tuning pass can't silently
//! break them: grip actually bleeds lateral momentum, the FBW trim
//! actually hovers, tilt actually goes places (and not past the cap).
//! (The golden replays in `golden.rs` pin the exact bytes; these pin
//! the MEANING, so they survive an intentional retune.)

use hogs_sim::*;
use pm::vec3;

const DT: f32 = 1.0 / 60.0;

/// The shipped tuning — steps read [`Params`] now, tests pin the
/// invariants at the defaults.
fn pp() -> Params {
    Params::default()
}

#[test]
fn truck_grips_out_sideways_momentum() {
    let mut t = spawn_truck(1);
    t.body.vel = vec3(10.0, 0.0, 0.0); // shoved out the doors (facing +z)
    for _ in 0..60 {
        truck_step(&mut t, Drive::default(), DT, &pp());
    }
    assert!(
        t.body.vel.x.abs() < 0.3,
        "1 s of tires should grip out a 10 u/s side shove, kept {}",
        t.body.vel.x
    );
}

#[test]
fn truck_slides_more_when_boosting() {
    let side = |boost: f32| {
        let mut t = spawn_truck(1);
        t.heat = 0.0;
        t.body.vel = vec3(8.0, 0.0, 0.0);
        let cmd = Drive {
            thrust: 1.0,
            boost,
            ..Default::default()
        };
        for _ in 0..12 {
            truck_step(&mut t, cmd, DT, &pp());
        }
        t.body.vel.x.abs()
    };
    assert!(
        side(1.0) > side(0.0) * 1.5,
        "boost should loosen grip: {} vs {}",
        side(1.0),
        side(0.0)
    );
}

#[test]
fn heli_hovers_hands_off() {
    let mut h = spawn_heli(1, &Params::default());
    h.body.pos.y = 20.0;
    for _ in 0..300 {
        heli_step(&mut h, Drive::default(), DT, &pp());
    }
    assert!(
        (h.body.pos.y - 20.0).abs() < 0.5 && h.body.vel.len() < 0.2,
        "centered stick must hover (FBW trim): y {} vel {}",
        h.body.pos.y,
        h.body.vel.len()
    );
}

/// The chin gun: azimuth and elevation are crisp clamped copies of
/// the command (the truck turret's law), and the muzzle solution
/// follows the gimbal, not the nose.
#[test]
fn truck_turret_slews_elevates_and_clamps() {
    let mut t = spawn_truck(1);
    let p = pp();
    let cmd = Drive {
        aim: 0.5,
        aim_pitch: 2.0, // past the elevation stop
        ..Default::default()
    };
    truck_step(&mut t, cmd, DT, &p);
    // One tick moves the turret at most one slew step — no snap.
    assert!(
        t.aim > 0.0 && t.aim <= p.turret_rate * DT + 1e-6,
        "azimuth slews at turret_rate, got {} after one tick",
        t.aim
    );
    // Held long enough, it converges and clamps at the stops.
    for _ in 0..120 {
        truck_step(&mut t, cmd, DT, &p);
    }
    assert!((t.aim - 0.5).abs() < 1e-4, "azimuth converges on the command");
    assert_eq!(t.aim_pitch, p.truck_aim_up, "elevation clamps at the stop");
    let (_, my, _, dir, climb) = truck_muzzle(&t);
    assert_eq!(climb, p.truck_aim_up, "the shot flies the aimed line");
    assert!(
        my > 1.45,
        "an elevated barrel's muzzle rises off the flat height, got {my}"
    );
    assert!(
        wrap_angle(dir - (t.heading() + t.aim)).abs() < 1e-5,
        "azimuth still trains off the heading"
    );
    // Depression clamps at its own (shallower) stop.
    for _ in 0..120 {
        truck_step(&mut t, Drive { aim_pitch: -2.0, ..Default::default() }, DT, &p);
    }
    assert_eq!(t.aim_pitch, -p.truck_aim_down, "depression stop is asymmetric");
}

#[test]
fn heli_chin_gun_gimbals_and_clamps() {
    let mut h = spawn_heli(1, &Params::default());
    h.body.pos.y = 10.0;
    let cmd = Drive {
        aim: 0.8,
        aim_pitch: -2.0, // past the gimbal stop
        ..Default::default()
    };
    heli_step(&mut h, cmd, DT, &pp());
    assert_eq!(h.aim, 0.8, "azimuth is a crisp copy");
    assert_eq!(h.aim_pitch, -pp().heli_aim_pitch, "elevation clamps at the stop");
    let (_, _, _, dir, climb) = heli_muzzle(&h);
    assert!(
        wrap_angle(dir - 0.8).abs() < 0.05,
        "the shot trains off the nose with the gimbal, got {dir}"
    );
    assert!(
        climb < -0.9,
        "a level airframe fires down the depressed gun, got {climb}"
    );
}

/// The tail thruster: full stick settles near the commanded yaw rate
/// (a rate loop against rotational drag — a little sag is physics, not
/// a bug) and the yaw comes with the commanded bank, not flat.
#[test]
fn heli_tail_thruster_yaws_at_commanded_rate() {
    let p = pp();
    let mut h = spawn_heli(1, &p);
    h.body.pos.y = 20.0;
    let cmd = Drive {
        turn: 1.0,
        ..Default::default()
    };
    let y0 = h.body.rot.to_yaw_pitch_roll().0;
    for _ in 0..120 {
        heli_step(&mut h, cmd, DT, &p);
    }
    let mid = h.body.rot.to_yaw_pitch_roll().0;
    for _ in 0..60 {
        heli_step(&mut h, cmd, DT, &p);
    }
    let rate = wrap_angle(h.body.rot.to_yaw_pitch_roll().0 - mid) / 1.0;
    assert!(
        rate > p.heli_yaw * 0.7 && rate < p.heli_yaw * 1.1,
        "settled yaw rate should track the command (~{}), got {rate}",
        p.heli_yaw
    );
    assert!(
        wrap_angle(mid - y0).abs() > 0.1,
        "the nose actually comes around during spin-up"
    );
    let (_, _, roll) = h.body.rot.to_yaw_pitch_roll();
    assert!(
        (roll - -p.heli_roll_max).abs() < 0.15,
        "yaw input banks the turn, roll {roll}"
    );
}

/// The combined body earning its keep: a bullet impulse on the tail
/// boom (the server's `impulse_at` seam) swings the nose, then the
/// fly-by-wire fights it back — level and rate-stable again hands-off.
#[test]
fn heli_tail_hit_swings_then_fbw_recovers() {
    let p = pp();
    let mut h = spawn_heli(1, &p);
    h.body.pos.y = 20.0;
    for _ in 0..60 {
        heli_step(&mut h, Drive::default(), DT, &p);
    }
    let yaw0 = h.body.rot.to_yaw_pitch_roll().0;
    // A square side-on hit at the tail mount, the server's exact call.
    let j = vec3(p.heli_tail_kick, 0.0, 0.0);
    h.body.impulse_at(j, h.body.rot.rotate(HELI_TAIL), HELI_MASS, HELI_INERTIA);
    assert!(
        h.body.ang.y.abs() > 0.5,
        "a tail hit is a real yaw swing, got {} rad/s",
        h.body.ang.y
    );
    let mut peak = 0.0f32;
    for _ in 0..180 {
        heli_step(&mut h, Drive::default(), DT, &p);
        peak = peak.max(wrap_angle(h.body.rot.to_yaw_pitch_roll().0 - yaw0).abs());
    }
    let (_, pitch, roll) = h.body.rot.to_yaw_pitch_roll();
    assert!(peak > 0.25, "the nose visibly comes around first, peak {peak}");
    assert!(
        h.body.ang.len() < 0.1 && pitch.abs() < 0.05 && roll.abs() < 0.05,
        "3 s hands-off and the FBW has it level and still: ang {} pitch {pitch} roll {roll}",
        h.body.ang.len()
    );
}

/// The terrain query under the trucks: flat floor, wedge surfaces,
/// tallest-wins, normals leaning downhill.
#[test]
fn ground_probe_reads_the_wedge() {
    // RAMPS[3] = (66, -55, yaw 0, hw 4, hl 7, h 2.8): lip at z = -62,
    // top edge at z = -48.
    assert_eq!(ground_height(0.0, 0.0), 0.0, "open arena is flat");
    assert_eq!(ground_height(66.0, -63.0), 0.0, "just short of the lip");
    assert!((ground_height(66.0, -55.0) - 1.4).abs() < 1e-4, "mid-slope = half height");
    assert!((ground_height(66.0, -48.1) - 2.78).abs() < 0.02, "near the top edge");
    assert_eq!(ground_height(71.0, -55.0), 0.0, "off the side");
    let (_, n) = ground_probe(66.0, -55.0);
    assert!(n.y > 0.95 && n.z < -0.1, "normal leans back down the slope, got {n:?}");
}

/// The whole arc: BOOST the ramp (cruise speed only pops ~0.2 u — big
/// air is boost's reward), launch off the top edge with the climb rate
/// the slope imparted, fly ballistic (heading held, no tire
/// authority), land, keep rolling. All inside the shared step, so the
/// jump predicts byte-exact.
#[test]
fn truck_runs_the_ramp_and_flies() {
    let p = pp();
    let mut t = spawn_truck(1);
    t.body.pos = vec3(66.0, 0.0, -80.0); // lined up south of RAMPS[3]
    let cmd = Drive {
        thrust: 1.0,
        boost: 1.0,
        ..Default::default()
    };
    let (mut apex, mut airborne_ticks) = (0.0f32, 0);
    for _ in 0..360 {
        truck_step(&mut t, cmd, DT, &p);
        apex = apex.max(t.body.pos.y);
        if t.body.pos.y > ground_height(t.body.pos.x, t.body.pos.z) + 0.2 {
            airborne_ticks += 1;
            // Stabilized turret: the muzzle rides the flying chassis.
            let (_, my, ..) = truck_muzzle(&t);
            assert!((my - (t.body.pos.y + 1.45)).abs() < 1e-4);
        }
    }
    assert!(
        apex > 3.2,
        "a boosted run off a 2.8 u ramp should clear 3.2 u, apex {apex}"
    );
    assert!(airborne_ticks > 20, "real air time, got {airborne_ticks} ticks");
    assert_eq!(t.body.pos.y, 0.0, "back on the floor");
    assert!(t.body.pos.z > -40.0, "landed past the ramp and kept rolling");
    assert!(t.heading().abs() < 1e-3, "no tire authority in the air — heading held");
}

/// Parked on the slope with no input: gravity's in-plane component
/// rolls the truck back down — ramps are terrain, not scripted pads.
#[test]
fn idle_truck_rolls_back_down_the_ramp() {
    let p = pp();
    let mut t = spawn_truck(1);
    t.body.pos = vec3(66.0, ground_height(66.0, -55.0), -55.0);
    for _ in 0..240 {
        truck_step(&mut t, Drive::default(), DT, &p);
    }
    assert!(
        t.body.pos.z < -58.0,
        "4 s of downslope pull should roll it toward the lip, z {}",
        t.body.pos.z
    );
}

/// The tall back face is a wall, not a 2.8 u instant climb.
#[test]
fn ramp_back_face_blocks_like_a_wall() {
    let p = pp();
    let mut t = spawn_truck(1);
    t.body.pos = vec3(66.0, 0.0, -40.0);
    t.body.rot = pm::Quat::from_yaw(std::f32::consts::PI); // facing -z, at the back face
    let cmd = Drive {
        thrust: 1.0,
        ..Default::default()
    };
    for _ in 0..240 {
        truck_step(&mut t, cmd, DT, &p);
    }
    assert!(
        t.body.pos.z > -48.5 && t.body.pos.y < 0.2,
        "blocked at the face, not teleported up it: z {} y {}",
        t.body.pos.z,
        t.body.pos.y
    );
}

/// Playtest-1 fix pinned: ramps are terrain for the skids — a heli
/// descending over a wedge settles ON its surface, not inside it.
#[test]
fn heli_skids_ride_the_ramp_surface() {
    let p = pp();
    let mut h = spawn_heli(1, &p);
    // Over the middle of RAMPS[3] (surface height ~1.4), descending.
    h.body.pos = vec3(66.0, 4.0, -55.0);
    let cmd = Drive { lift: -0.6, ..Default::default() };
    let mut worst = f32::MAX;
    for _ in 0..300 {
        heli_step(&mut h, cmd, DT, &p);
        let floor = ground_height(h.body.pos.x, h.body.pos.z) + p.heli_ground;
        worst = worst.min(h.body.pos.y - floor);
    }
    // The skids may SLIDE down the slope (no grip on a steel wedge —
    // that's the fun), but they must never sink through it.
    assert!(worst > -0.05, "never inside the surface, worst {worst}");
    assert!(
        h.body.pos.y <= p.heli_ground + 0.05,
        "slid off the frictionless wedge to the flat, y {}",
        h.body.pos.y
    );
}

#[test]
fn heli_full_tilt_cruises_fast_but_capped() {
    let mut h = spawn_heli(1, &Params::default());
    h.body.pos.y = 20.0;
    let cmd = Drive {
        pitch: 1.0,
        ..Default::default()
    };
    for _ in 0..600 {
        heli_step(&mut h, cmd, DT, &pp());
    }
    let hs = (h.body.vel.x * h.body.vel.x + h.body.vel.z * h.body.vel.z).sqrt();
    let vcap = pp().heli_vcap;
    assert!(
        hs > 20.0 && hs <= vcap + 0.1,
        "full nose-down should cruise 20..{vcap} u/s, got {hs}"
    );
    assert!(
        (h.body.pos.y - 20.0).abs() < 2.0,
        "FBW trim should hold altitude through a full-tilt dash, y {}",
        h.body.pos.y
    );
}
