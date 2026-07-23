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
