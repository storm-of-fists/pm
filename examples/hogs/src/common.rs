//! Shared hogs definitions: the replicated pods, THE truck step (same
//! code on server and in client prediction replay — drive's lesson), and
//! the pure geometry both sides use. Hogs are server-owned NPCs: clients
//! never step them, only interpolate — so `hog` state has no client-side
//! step function at all, just a lerp.

use pm::Id;

pub const ADDR: &str = "127.0.0.1:48223";
/// Fixed simulation step on both sides (prediction replays it).
pub const FIXED_DT: f32 = 1.0 / 60.0;
/// Half-extent of the square arena (walls at +-ARENA on x and z).
/// Big: the horde needs room to flank and the trucks need room to run,
/// with buildings breaking up the sightlines.
pub const ARENA: f32 = 100.0;

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
#[pm::pod]
pub struct Truck {
    pub x: f32,
    pub z: f32,
    pub heading: f32,
    pub speed: f32,
    /// Filtered steering (bots lag; humans are crisp) — replicated so a
    /// truck's near future is determined, like drive.
    pub steer: f32,
    /// Turret angle relative to `heading` (the mouse-aim seam). Evolved
    /// by `truck_step` from the command frame like everything else, so
    /// it predicts and replicates for free — remote players see your
    /// turret swing.
    pub aim: f32,
    /// Boost heat, 0..1 — rises while boosting, cools otherwise, all in
    /// `truck_step`, so the meter predicts smoothly. Hitting 1.0 is the
    /// SERVER's cue to explode the truck (consequences aren't predicted;
    /// see `Health` for why).
    pub heat: f32,
}

/// Server-owned truck vitals, deliberately NOT in the predicted pod:
/// damage comes from server events (bites), not from replaying commands,
/// so predicting it is impossible — and a non-predicted field inside a
/// Predictor's state pod freezes between corrections. Separate synced
/// pool, same id as the truck; clients read it raw.
#[pm::pod]
pub struct Health {
    pub hp: f32,
}

/// A biomod feral hog: server-owned, never predicted — clients read it
/// through `interp_pool` only. At horde scale this pod IS the bandwidth
/// experiment, so it rides the wire quantized (the `#[wire]` field
/// attributes make `#[pm::pod]` derive `pm::Wire`; register with
/// `wire_pool`): 20 B of f32s → a 9 B repr, 13 B/entry with the id →
/// ~90 entities per 1200 B snapshot instead of ~45. Coords at 1/64 u
/// (±512 u range — the walls sit at ±ARENA), angles at 1e-4 rad (the
/// server wraps `heading` to [-pi, pi) at every write — i16 saturates
/// past ±3.27), hp at 1/200 over its 0..=HOG_HP range.
#[pm::pod]
pub struct Hog {
    #[wire(i16, scale = 64.0)]
    pub x: f32,
    #[wire(i16, scale = 64.0)]
    pub z: f32,
    #[wire(i16, scale = 10000.0)]
    pub heading: f32,
    #[wire(i16, scale = 256.0)]
    pub speed: f32,
    /// 0..HOG_HP; clients tint by it. Dead hogs are REMOVED, not hp==0.
    #[wire(u8, scale = 200.0)]
    pub hp: f32,
}

/// Server-owned co-op scoreboard, replicated as a synced single (the
/// SingleRx path drive never exercised): one shared score, the live hog
/// count, and the wave number.
#[pm::pod]
pub struct Hunt {
    pub points: f32,
    pub alive: u32,
    pub wave: u32,
}

/// A live bullet: server-owned like the hogs — the server steps it,
/// judges its hits (lag-compensated per shooter, each tick of flight),
/// and removes it on impact or at max range; clients only interpolate
/// and draw the tracer. Which peer fired it is server-local state
/// (`id.peer()` is recycling, not control), so the pod stays lean —
/// and quantized like the hogs (bullets are the other every-tick pool;
/// `heading` is wrapped at spawn and never changes in flight).
#[pm::pod]
pub struct Bullet {
    #[wire(i16, scale = 64.0)]
    pub x: f32,
    #[wire(i16, scale = 64.0)]
    pub z: f32,
    #[wire(i16, scale = 10000.0)]
    pub heading: f32,
}

/// A transient replicated FACT (the contact-points pattern): the server
/// spawns one on a fresh id where something landed and `ttl_pool`
/// removes it. Clients render whatever entries exist, clean up nothing.
#[pm::pod]
pub struct Impact {
    #[wire(i16, scale = 64.0)]
    pub x: f32,
    #[wire(i16, scale = 64.0)]
    pub z: f32,
    /// What happened here — see the `IMPACT_*` constants. Small whole
    /// numbers, so the u8 roundtrip is exact and `==` still works.
    #[wire(u8)]
    pub kind: f32,
}

pub const IMPACT_HIT: f32 = 0.0; // a shot connected
pub const IMPACT_KILL: f32 = 1.0; // a hog died here
pub const IMPACT_BITE: f32 = 2.0; // a hog rammed a truck
pub const IMPACT_BOOM: f32 = 3.0; // a truck exploded (overheat or hp 0)
/// Marker lifetime — comfortably above one resend window so lossy
/// clients see every flash before it expires.
pub const IMPACT_TTL: f32 = 1.0;

// --- channels --------------------------------------------------------------

/// Command-frame input payload: driving plus the turret. `fire` is held
/// state, not an event — the server's gun cooldown turns it into shots.
/// `aim` is the turret angle the client wants THIS frame: the hold-to-aim
/// accumulation and the smooth snap-back on release are both client-side
/// animation; the server just gets a stream of absolute angles.
#[pm::pod]
pub struct Drive {
    pub thrust: f32, // -1..1
    pub turn: f32,   // -1..1
    pub fire: f32,   // 0/1: trigger held
    pub aim: f32,    // turret angle relative to heading, +-AIM_MAX
    pub boost: f32,  // 0/1: burn heat for speed (1.0 heat = boom)
    pub bot: f32,    // 0/1: AI controller — its steering lags
}

/// Reliable client→server event: "flip me back to my spawn."
#[pm::pod]
pub struct Respawn {
    pub pad: u32,
}

// --- tuning ----------------------------------------------------------------

/// Truck top speed (forward), and boosted.
pub const VMAX: f32 = 18.0;
pub const BOOST_VMAX: f32 = 30.0;
/// Heat per second while boosting / cooling per second while not. Full
/// burn to explosion in ~2.5 s; a full cooldown takes ~4 s.
pub const HEAT_RATE: f32 = 0.4;
pub const HEAT_COOL: f32 = 0.25;
/// Truck hitpoints and what one bite takes.
pub const TRUCK_HP: f32 = 1.0;
pub const BITE_DMG: f32 = 0.25;
/// Points an exploded truck costs the team (on top of the bites that
/// probably caused it).
pub const DEATH_COST: f32 = 30.0;
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
/// Charge / roam speeds.
pub const HOG_FAST: f32 = 11.0;
pub const HOG_ROAM: f32 = 4.5;
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
/// While roaming, a hog walks to a random goal and picks a new one
/// inside this many seconds (or on arrival) — real wandering, not the
/// old stand-and-wiggle.
pub const ROAM_REPICK: f32 = 9.0;

/// Turret gun: refire period, damage per shot, and the projectile —
/// bullets are real replicated entities now, so range is max travel.
pub const GUN_CD: f32 = 0.25;
pub const GUN_DMG: f32 = 0.5;
pub const GUN_RANGE: f32 = 45.0;
pub const BULLET_SPEED: f32 = 70.0;
/// Turret swing limit either side of straight ahead.
pub const AIM_MAX: f32 = 2.6;

// --- buildings ---------------------------------------------------------------

/// Static obstacles as `(center x, center z, half w, half d, height)`.
/// Shared const data compiled into BOTH binaries — server and clients
/// collide against the same walls, so nothing about them replicates
/// (height is render-only). The south strip (z < -85) stays clear: that's
/// where trucks spawn.
pub const BUILDINGS: [(f32, f32, f32, f32, f32); 14] = [
    (10.0, 8.0, 4.0, 4.0, 11.0), // the downtown tower
    (0.0, -22.0, 11.0, 4.0, 6.0),
    (-40.0, -30.0, 8.0, 6.0, 5.0),
    (35.0, -45.0, 6.0, 9.0, 4.0),
    (-20.0, -60.0, 5.0, 5.0, 4.0),
    (-80.0, -55.0, 6.0, 6.0, 5.0),
    (75.0, -20.0, 4.0, 8.0, 6.0),
    (-65.0, 10.0, 7.0, 7.0, 8.0),
    (60.0, 20.0, 9.0, 5.0, 5.0),
    (20.0, 45.0, 5.0, 5.0, 7.0),
    (-25.0, 55.0, 8.0, 4.0, 4.0),
    (45.0, 70.0, 7.0, 6.0, 9.0),
    (-55.0, 75.0, 5.0, 8.0, 5.0),
    (80.0, 60.0, 6.0, 6.0, 7.0),
];

/// Whether `(x, z)` is inside any building footprint grown by `pad`.
pub fn in_building(x: f32, z: f32, pad: f32) -> bool {
    BUILDINGS
        .iter()
        .any(|&(bx, bz, hw, hd, _)| (x - bx).abs() < hw + pad && (z - bz).abs() < hd + pad)
}

/// Push a circle at `(x, z)` radius `r` out of every building it
/// overlaps. Returns the corrected position and the last push normal
/// (zero if nothing touched) — callers use the normal to scrub speed
/// (trucks) or slide the heading along the wall (hogs).
pub fn building_push(x: f32, z: f32, r: f32) -> (f32, f32, f32, f32) {
    let (mut x, mut z) = (x, z);
    let (mut nx, mut nz) = (0.0, 0.0);
    for &(bx, bz, hw, hd, _) in &BUILDINGS {
        // Closest point on the box to the circle center.
        let cx = x.clamp(bx - hw, bx + hw);
        let cz = z.clamp(bz - hd, bz + hd);
        let (dx, dz) = (x - cx, z - cz);
        let d2 = dx * dx + dz * dz;
        if d2 >= r * r {
            continue;
        }
        if d2 > 1e-8 {
            // Center outside the box: push straight away from the wall.
            let d = d2.sqrt();
            nx = dx / d;
            nz = dz / d;
            x = cx + nx * r;
            z = cz + nz * r;
        } else {
            // Center INSIDE the box (tunneled): exit by the nearest face.
            let ex = hw + r - (x - bx).abs();
            let ez = hd + r - (z - bz).abs();
            if ex < ez {
                nx = (x - bx).signum();
                nz = 0.0;
                x = bx + nx * (hw + r);
            } else {
                nx = 0.0;
                nz = (z - bz).signum();
                z = bz + nz * (hd + r);
            }
        }
    }
    (x, z, nx, nz)
}

// --- THE truck step ----------------------------------------------------------

/// THE step — drive's physics minus drift: bot steering lags (first-order
/// filter, so the near future is a real prediction), humans steer crisp,
/// speed-scaled turning, drag, hard arena walls that scrub speed.
pub fn truck_step(t: &mut Truck, cmd: Drive, dt: f32) {
    // COMPILE-TIME COVERAGE: an exhaustive destructure (no `..`), so
    // adding a Truck field refuses to compile until it's named here —
    // and the rule this line sends you here to obey is: every field in
    // the predicted pod must be EVOLVED BY THIS FUNCTION from the
    // command. If the server writes it outside this step (damage,
    // pickups), it does NOT belong in Truck — give it its own
    // authoritative pool (that's why hp lives in `Health`). Then cover
    // the new field in `err_metric` and `truck_lerp` below (the lerp's
    // exhaustive struct literal breaks on its own; the metric won't).
    let Truck {
        x: _,
        z: _,
        heading: _,
        speed: _,
        steer: _,
        aim: _,
        heat: _,
    } = *t;

    if cmd.bot > 0.5 {
        let k = 1.0 - (-dt / STEER_TAU).exp();
        t.steer += (cmd.turn - t.steer) * k;
    } else {
        t.steer = cmd.turn;
    }
    // Turret: crisp copy of the commanded angle — the client animates
    // the hold/snap-back, so replaying commands reproduces it exactly.
    t.aim = cmd.aim.clamp(-AIM_MAX, AIM_MAX);
    // Boost: extra shove and a higher ceiling, paid in heat. Heat is
    // predicted state (this is THE shared step), so the client's meter
    // is live; the EXPLOSION at 1.0 is the server's move alone.
    let boosting = cmd.boost > 0.5 && cmd.thrust > 0.0 && t.heat < 1.0;
    t.heat = if boosting {
        (t.heat + HEAT_RATE * dt).min(1.0)
    } else {
        (t.heat - HEAT_COOL * dt).max(0.0)
    };
    let (accel, vmax) = if boosting {
        (26.0, BOOST_VMAX)
    } else {
        (14.0, VMAX)
    };
    t.speed = (t.speed + cmd.thrust * accel * dt) * (1.0 - 1.2 * dt);
    t.speed = t.speed.clamp(-7.0, vmax);
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
    // Buildings: same shared step on both sides, so driving into one
    // predicts byte-exact. The truck collides as a circle — close enough
    // at driving speeds, and capsule-vs-box isn't worth the code here.
    let (px, pz, nx, nz) = building_push(t.x, t.z, TRUCK_R + 0.3);
    if nx != 0.0 || nz != 0.0 {
        t.x = px;
        t.z = pz;
        t.speed *= 1.0 - 1.6 * dt; // grinding a wall bleeds speed
    }
}

/// Prediction error metric: max caring about position first.
pub fn err_metric(a: &Truck, b: &Truck) -> f32 {
    (a.x - b.x).abs()
        + (a.z - b.z).abs()
        + (a.heading - b.heading).abs()
        + (a.speed - b.speed).abs()
        + (a.steer - b.steer).abs()
        + (a.aim - b.aim).abs()
        + (a.heat - b.heat).abs()
}

// --- geometry ---------------------------------------------------------------

// Angle helpers come from the engine; re-exported so the whole example
// reaches them through `common::*` like the rest of the shared math.
pub use pm::{lerp_angle, wrap_angle};

/// Interpolate two truck samples (`pm::pool_interp`'s lerp).
pub fn truck_lerp(a: &Truck, b: &Truck, t: f32) -> Truck {
    let l = |x: f32, y: f32| x + (y - x) * t;
    Truck {
        x: l(a.x, b.x),
        z: l(a.z, b.z),
        heading: lerp_angle(a.heading, b.heading, t),
        speed: l(a.speed, b.speed),
        steer: l(a.steer, b.steer),
        aim: lerp_angle(a.aim, b.aim, t),
        heat: l(a.heat, b.heat),
    }
}

/// Interpolate two bullet samples.
pub fn bullet_lerp(a: &Bullet, b: &Bullet, t: f32) -> Bullet {
    let l = |x: f32, y: f32| x + (y - x) * t;
    Bullet {
        x: l(a.x, b.x),
        z: l(a.z, b.z),
        heading: lerp_angle(a.heading, b.heading, t),
    }
}

/// Interpolate two hog samples.
pub fn hog_lerp(a: &Hog, b: &Hog, t: f32) -> Hog {
    let l = |x: f32, y: f32| x + (y - x) * t;
    Hog {
        x: l(a.x, b.x),
        z: l(a.z, b.z),
        heading: lerp_angle(a.heading, b.heading, t),
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

/// Ray from `(x, z)` along `heading` against hog circles: the nearest
/// hog whose body the ray crosses within `range`, as `(index into hogs,
/// hit x, hit z)`. The server sweeps each bullet's per-tick travel with
/// it, against a REWOUND frame (the shooter's view) — which is the whole
/// lag-comp trick.
pub fn ray_hit_hog(
    x: f32,
    z: f32,
    heading: f32,
    range: f32,
    hogs: &[(Id, Hog)],
) -> Option<(usize, f32, f32)> {
    let (dx, dz) = (heading.sin(), heading.cos());
    let mut best: Option<(usize, f32)> = None;
    for (k, (_, h)) in hogs.iter().enumerate() {
        let (ox, oz) = (h.x - x, h.z - z);
        let t = ox * dx + oz * dz; // along-ray distance to closest approach
        if !(0.0..=range).contains(&t) {
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
        ..Truck::default()
    }
}
