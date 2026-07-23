//! Golden replays — the determinism boundary's tripwire (v2 item 3).
//!
//! Each test drives a vehicle through a scripted command stream and
//! folds EVERY tick's full pod bytes into one hash: any change to a
//! step's math, a shared const, a `Params` default, or pod layout
//! changes the hash. Red test = the physics changed. If that was the
//! POINT of your change: bump [`hogs_sim::SIM_VERSION`], run with
//! `cargo test -p hogs-sim -- --nocapture` and paste the printed
//! hashes over the goldens below. If it wasn't — you just caught a
//! prediction desync before it cost a soak.
//!
//! Hashes are recorded on x86_64-linux. Add/sub/mul/div/sqrt are
//! IEEE-exact everywhere, but sin/cos/exp/atan2 come from the
//! platform libm, so a different target may disagree in the last ulp —
//! record per-platform goldens the day CI runs anywhere else.

use hogs_sim::*;

/// FNV-1a over the pod's bytes, folded across ticks. Pods are
/// `bytemuck::Pod` (no padding), so the bytes are the whole truth.
fn fold<T: bytemuck::Pod>(h: u64, pod: &T) -> u64 {
    bytemuck::bytes_of(pod)
        .iter()
        .fold(h, |h, &b| (h ^ b as u64).wrapping_mul(0x100_0000_01b3))
}

const FNV_SEED: u64 = 0xcbf2_9ce4_8422_2325;

/// Deterministic command script: an LCG picks fresh axes every 13 ticks
/// (held between picks, so it drives like inputs, not noise). Exercises
/// every `Drive` axis both steps read — thrust/turn/boost/bot for the
/// truck, pitch/lift/turn for the heli, aim/aim_pitch for both.
fn script(tick: u32, rng: &mut u64) -> Drive {
    if tick % 13 == 0 {
        *rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
    }
    let mut bits = *rng;
    let mut axis = || {
        bits = bits.rotate_left(11) ^ 0x9e37_79b9_7f4a_7c15;
        ((bits >> 40) as i32 % 1000) as f32 / 500.0 - 1.0
    };
    Drive {
        thrust: axis(),
        turn: axis(),
        fire: 0.0,
        aim: axis() * 3.0,
        boost: if axis() > 0.5 { 1.0 } else { 0.0 },
        bot: if axis() > 0.0 { 1.0 } else { 0.0 },
        pitch: axis(),
        lift: axis(),
        aim_pitch: axis() * 1.5,
    }
}

/// 1800 ticks (30 s) is enough travel at truck/heli speeds to hit the
/// arena walls and the building field from the south spawns — the
/// clamp/push branches are inside the hash, not just open-field math.
const TICKS: u32 = 1800;

const GOLDEN_TRUCK: u64 = 0xc896e50f56d24d9a; // recorded at SIM_VERSION 1
const GOLDEN_HELI: u64 = 0xeaf3a043d1b078bd; // recorded at SIM_VERSION 1

#[test]
fn truck_replay_is_golden() {
    let p = Params::default();
    let mut t = spawn_truck(3);
    let mut rng = 0x7065_7263_6865_726f_u64;
    let mut h = FNV_SEED;
    for tick in 0..TICKS {
        truck_step(&mut t, script(tick, &mut rng), FIXED_DT, &p);
        h = fold(h, &t);
    }
    println!("truck golden (SIM_VERSION {SIM_VERSION}): {h:#018x}");
    assert_eq!(
        h, GOLDEN_TRUCK,
        "truck_step output changed — intentional? bump SIM_VERSION and re-record"
    );
}

#[test]
fn heli_replay_is_golden() {
    let p = Params::default();
    let mut h0 = spawn_heli(2, &Params::default());
    let mut rng = 0x686f_6773_2e73_696d_u64;
    let mut h = FNV_SEED;
    for tick in 0..TICKS {
        heli_step(&mut h0, script(tick, &mut rng), FIXED_DT, &p);
        h = fold(h, &h0);
    }
    println!("heli golden (SIM_VERSION {SIM_VERSION}): {h:#018x}");
    assert_eq!(
        h, GOLDEN_HELI,
        "heli_step output changed — intentional? bump SIM_VERSION and re-record"
    );
}

/// The replay property itself, independent of recorded goldens: the
/// same command stream from the same seed state reproduces the same
/// bytes — if this ever fails, something impure got into a step.
#[test]
fn same_script_same_bytes() {
    let run = || {
        let p = Params::default();
        let mut t = spawn_truck(1);
        let mut rng = 42;
        let mut h = FNV_SEED;
        for tick in 0..600 {
            truck_step(&mut t, script(tick, &mut rng), FIXED_DT, &p);
            h = fold(h, &t);
        }
        h
    };
    assert_eq!(run(), run());
}
