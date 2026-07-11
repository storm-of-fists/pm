//! Shared hogs definitions: the replicated pods, THE truck step (same
//! code on server and in client prediction replay — drive's lesson), and
//! the pure geometry both sides use. Hogs are server-owned NPCs: clients
//! never step them, only interpolate — so `hog` state has no client-side
//! step function at all, just a lerp.

use bytemuck::{Pod, Zeroable};
use pm::Id;

pub const ADDR: &str = "127.0.0.1:48223";
/// Fixed simulation step on both sides (prediction replays it).
pub const FIXED_DT: f32 = 1.0 / 60.0;
/// Half-extent of the square arena (walls at +-ARENA on x and z).
/// Bigger than drive's: a horde needs room to flank.
pub const ARENA: f32 = 55.0;

/// Remote interpolation delay (seconds) — same shared-constant contract
/// as drive: the client hands it to `interp_pool` (trucks AND hogs), the
/// server subtracts it (in ticks) from a peer's acked tick to judge that
/// peer's shots against the world they were aiming at. `PM_INTERP_MS`
/// overrides for feel A/B's.
pub const INTERP_DELAY: f32 = 0.05;

/// [`INTERP_DELAY`] with the `PM_INTERP_MS` override applied.
pub fn interp_delay() -> f32 {
    std::env::var("PM_INTERP_MS")
        .ok()
        .and_then(|v| v.parse::<f32>().ok())
        .map_or(INTERP_DELAY, |ms| ms / 1000.0)
}

/// The interp delay in whole sim ticks — what the server subtracts from
/// a peer's acked tick to find the tick that peer was *seeing*.
pub fn interp_ticks() -> u32 {
    (interp_delay() / FIXED_DT).round() as u32
}

/// First-wave horde size (`PM_HOGS` overrides — the stress knob).
pub fn wave_base() -> u32 {
    std::env::var("PM_HOGS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(40)
}
/// Extra hogs per wave past the first.
pub const WAVE_GROW: u32 = 15;

// --- replicated pods -----------------------------------------------------

/// Replicated truck state — the PREDICTED substate only, same discipline
/// as drive's Car: every field is something `truck_step` evolves. Ground
/// plane: heading 0 faces +z, forward = (sin h, cos h) on (x, z).
#[derive(Clone, Copy, PartialEq, Debug, Default, Pod, Zeroable)]
#[repr(C)]
pub struct Truck {
    pub x: f32,
    pub z: f32,
    pub heading: f32,
    pub speed: f32,
    /// Filtered steering (bots lag; humans are crisp) — replicated so a
    /// truck's near future is determined, like drive.
    pub steer: f32,
}

/// A biomod feral hog: server-owned, never predicted — clients read it
/// through `interp_pool` only. Kept lean on purpose: at horde scale this
/// pod IS the bandwidth experiment (~20 B/hog + 4 B id → ~45 entities
/// per 1200 B snapshot; a 300-hog wave forces the budget to rotate).
#[derive(Clone, Copy, PartialEq, Debug, Default, Pod, Zeroable)]
#[repr(C)]
pub struct Hog {
    pub x: f32,
    pub z: f32,
    pub heading: f32,
    pub speed: f32,
    /// 0..HOG_HP; clients tint by it. Dead hogs are REMOVED, not hp==0.
    pub hp: f32,
}

/// Server-owned co-op scoreboard, replicated as a synced single (the
/// SingleRx path drive never exercised): one shared score, the live hog
/// count, and the wave number.
#[derive(Clone, Copy, PartialEq, Debug, Default, Pod, Zeroable)]
#[repr(C)]
pub struct Hunt {
    pub points: f32,
    pub alive: u32,
    pub wave: u32,
}

/// A transient replicated FACT (the contact-points pattern): the server
/// spawns one on a fresh id where something landed and `ttl_pool`
/// removes it. Clients render whatever entries exist, clean up nothing.
#[derive(Clone, Copy, PartialEq, Debug, Default, Pod, Zeroable)]
#[repr(C)]
pub struct Impact {
    pub x: f32,
    pub z: f32,
    /// What happened here — see the `IMPACT_*` constants.
    pub kind: f32,
}

pub const IMPACT_HIT: f32 = 0.0; // a shot connected
pub const IMPACT_KILL: f32 = 1.0; // a hog died here
pub const IMPACT_BITE: f32 = 2.0; // a hog rammed a truck
/// Marker lifetime — comfortably above one resend window so lossy
/// clients see every flash before it expires.
pub const IMPACT_TTL: f32 = 1.0;

// --- channels --------------------------------------------------------------

/// Command-frame input payload: driving plus the trigger. `fire` is held
/// state, not an event — the server's gun cooldown turns it into shots.
#[derive(Clone, Copy, PartialEq, Debug, Default, Pod, Zeroable)]
#[repr(C)]
pub struct Drive {
    pub thrust: f32, // -1..1
    pub turn: f32,   // -1..1 (positive = left)
    pub fire: f32,   // 0/1: trigger held
    pub bot: f32,    // 0/1: AI controller — its steering lags
}

/// Reliable client→server event: "flip me back to my spawn."
#[derive(Clone, Copy, Default, Pod, Zeroable)]
#[repr(C)]
pub struct Respawn {
    pub pad: u32,
}

// --- tuning ----------------------------------------------------------------

/// Truck top speed (forward).
pub const VMAX: f32 = 18.0;
/// Truck collision capsule: half-length along forward, radius.
pub const TRUCK_HL: f32 = 0.8;
pub const TRUCK_R: f32 = 0.9;
/// Steering control-lag time constant for bot drivers (seconds).
pub const STEER_TAU: f32 = 0.18;

/// Hog body radius (they're round; the biomod part is the attitude).
pub const HOG_R: f32 = 0.7;
/// Shots to drop a hog: HOG_HP / GUN_DMG.
pub const HOG_HP: f32 = 1.0;
/// A truck inside this range gets charged.
pub const HOG_AGGRO: f32 = 26.0;
/// Charge / wander speeds.
pub const HOG_FAST: f32 = 11.0;
pub const HOG_SLOW: f32 = 3.0;
/// Hog turn rate (rad/s) — slower than a truck can steer, so you can
/// juke a charge.
pub const HOG_TURN: f32 = 2.6;
/// After a bite the hog breaks off for this long (seconds).
pub const HOG_FLEE: f32 = 1.5;
/// Per-hog re-bite lockout (seconds) — debounces the overlap flicker.
pub const BITE_CD: f32 = 1.0;
/// Points a bite costs the team.
pub const BITE_COST: f32 = 15.0;
/// Points a kill earns the team.
pub const KILL_POINTS: f32 = 10.0;

/// Fixed forward gun: hitscan range, refire period, damage per shot.
pub const GUN_RANGE: f32 = 30.0;
pub const GUN_CD: f32 = 0.25;
pub const GUN_DMG: f32 = 0.5;

// --- THE truck step ----------------------------------------------------------

/// THE step — drive's physics minus drift: bot steering lags (first-order
/// filter, so the near future is a real prediction), humans steer crisp,
/// speed-scaled turning, drag, hard arena walls that scrub speed.
pub fn truck_step(t: &mut Truck, cmd: Drive, dt: f32) {
    if cmd.bot > 0.5 {
        let k = 1.0 - (-dt / STEER_TAU).exp();
        t.steer += (cmd.turn - t.steer) * k;
    } else {
        t.steer = cmd.turn;
    }
    t.speed = (t.speed + cmd.thrust * 14.0 * dt) * (1.0 - 1.2 * dt);
    t.speed = t.speed.clamp(-7.0, VMAX);
    let authority = (t.speed.abs() / 6.0).min(1.0);
    t.heading += t.steer * 2.2 * authority * dt * t.speed.signum();
    t.x += t.heading.sin() * t.speed * dt;
    t.z += t.heading.cos() * t.speed * dt;
    if t.x.abs() > ARENA {
        t.x = t.x.clamp(-ARENA, ARENA);
        t.speed *= 0.4;
    }
    if t.z.abs() > ARENA {
        t.z = t.z.clamp(-ARENA, ARENA);
        t.speed *= 0.4;
    }
}

/// Prediction error metric: max caring about position first.
pub fn err_metric(a: &Truck, b: &Truck) -> f32 {
    (a.x - b.x).abs()
        + (a.z - b.z).abs()
        + (a.heading - b.heading).abs()
        + (a.speed - b.speed).abs()
        + (a.steer - b.steer).abs()
}

// --- geometry ---------------------------------------------------------------

/// Wrap an angle difference to the shortest arc in [-pi, pi].
pub fn wrap_angle(dh: f32) -> f32 {
    (dh + std::f32::consts::PI).rem_euclid(std::f32::consts::TAU) - std::f32::consts::PI
}

fn lerp_heading(a: f32, b: f32, t: f32) -> f32 {
    a + wrap_angle(b - a) * t
}

/// Interpolate two truck samples (`pm::pool_interp`'s lerp).
pub fn truck_lerp(a: &Truck, b: &Truck, t: f32) -> Truck {
    let l = |x: f32, y: f32| x + (y - x) * t;
    Truck {
        x: l(a.x, b.x),
        z: l(a.z, b.z),
        heading: lerp_heading(a.heading, b.heading, t),
        speed: l(a.speed, b.speed),
        steer: l(a.steer, b.steer),
    }
}

/// Interpolate two hog samples.
pub fn hog_lerp(a: &Hog, b: &Hog, t: f32) -> Hog {
    let l = |x: f32, y: f32| x + (y - x) * t;
    Hog {
        x: l(a.x, b.x),
        z: l(a.z, b.z),
        heading: lerp_heading(a.heading, b.heading, t),
        speed: l(a.speed, b.speed),
        hp: l(a.hp, b.hp),
    }
}

/// A truck's collision capsule as its two segment endpoints (back, front).
pub fn truck_seg(t: &Truck) -> ((f32, f32), (f32, f32)) {
    let (fx, fz) = (t.heading.sin() * TRUCK_HL, t.heading.cos() * TRUCK_HL);
    ((t.x - fx, t.z - fz), (t.x + fx, t.z + fz))
}

/// Distance from point `p` to segment `a`-`b`.
pub fn seg_point_dist(a: (f32, f32), b: (f32, f32), p: (f32, f32)) -> f32 {
    let (abx, abz) = (b.0 - a.0, b.1 - a.1);
    let (apx, apz) = (p.0 - a.0, p.1 - a.1);
    let len2 = abx * abx + abz * abz;
    let t = if len2 > 1e-8 {
        ((apx * abx + apz * abz) / len2).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let (cx, cz) = (a.0 + abx * t, a.1 + abz * t);
    let (dx, dz) = (p.0 - cx, p.1 - cz);
    (dx * dx + dz * dz).sqrt()
}

/// Whether a hog (circle) touches a truck (capsule).
pub fn hog_bites_truck(h: &Hog, t: &Truck) -> bool {
    let (a, b) = truck_seg(t);
    seg_point_dist(a, b, (h.x, h.z)) < HOG_R + TRUCK_R
}

/// Hitscan from `(x, z)` along `heading` against hog circles: the
/// nearest hog whose body the ray crosses within [`GUN_RANGE`], as
/// `(index into hogs, hit x, hit z)`. Pure — the server calls it with a
/// REWOUND frame (the shooter's view), which is the whole lag-comp trick.
pub fn ray_hit_hog(x: f32, z: f32, heading: f32, hogs: &[(Id, Hog)]) -> Option<(usize, f32, f32)> {
    let (dx, dz) = (heading.sin(), heading.cos());
    let mut best: Option<(usize, f32)> = None;
    for (k, (_, h)) in hogs.iter().enumerate() {
        let (ox, oz) = (h.x - x, h.z - z);
        let t = ox * dx + oz * dz; // along-ray distance to closest approach
        if !(0.0..=GUN_RANGE).contains(&t) {
            continue;
        }
        let (cx, cz) = (ox - dx * t, oz - dz * t);
        if cx * cx + cz * cz > HOG_R * HOG_R {
            continue;
        }
        if best.is_none_or(|(_, bt)| t < bt) {
            best = Some((k, t));
        }
    }
    best.map(|(k, t)| (k, x + dx * t, z + dz * t))
}

// --- presentation helpers --------------------------------------------------

/// Per-peer truck tints (peer ids start at 1; index peer-1).
pub const PCOL: [(f32, f32, f32); 8] = [
    (0.98, 0.82, 0.16),
    (0.36, 0.55, 0.86),
    (0.85, 0.35, 0.42),
    (0.42, 0.78, 0.47),
    (0.78, 0.45, 0.85),
    (0.95, 0.55, 0.25),
    (0.35, 0.78, 0.78),
    (0.85, 0.75, 0.55),
];

pub fn peer_color(peer: u8) -> (f32, f32, f32) {
    PCOL[(peer as usize).saturating_sub(1) % PCOL.len()]
}

/// Spawn slot for a peer: spread along the south wall, facing in.
pub fn spawn_truck(peer: u8) -> Truck {
    Truck {
        x: (peer as f32 - 4.5) * 5.0,
        z: -ARENA + 6.0,
        heading: 0.0,
        speed: 0.0,
        steer: 0.0,
    }
}
