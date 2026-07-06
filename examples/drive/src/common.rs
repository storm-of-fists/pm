//! Shared drive definitions: the replicated car pod, the command pod,
//! and THE step function — the same code advances the car on the server
//! and in client prediction replay; determinism is what makes
//! reconciliation byte-exact (the demo's lesson, now in 3D).

use bytemuck::{Pod, Zeroable};

pub const ADDR: &str = "127.0.0.1:48222";
/// Fixed simulation step on both sides (prediction replays it).
pub const FIXED_DT: f32 = 1.0 / 60.0;
/// Half-extent of the square arena (walls at +-ARENA on x and z).
pub const ARENA: f32 = 38.0;

/// Remote-car interpolation delay (seconds): clients render rivals this
/// far behind the newest snapshot. Shared like `FIXED_DT` because BOTH
/// sides read it — the client hands it to `interp_pool`, the server
/// subtracts it (in ticks) from a peer's acked tick to rewind scoring to
/// what that peer was looking at. `PM_INTERP_MS` overrides it for feel
/// A/B's; drive runs server and clients in one process, so one env keeps
/// them agreeing.
pub const INTERP_DELAY: f32 = 0.05;

/// [`INTERP_DELAY`] with the `PM_INTERP_MS` override applied.
pub fn interp_delay() -> f32 {
    std::env::var("PM_INTERP_MS")
        .ok()
        .and_then(|v| v.parse::<f32>().ok())
        .map_or(INTERP_DELAY, |ms| ms / 1000.0)
}

/// The interp delay in whole sim ticks — what the server subtracts from a
/// peer's acked tick to find the tick that peer was *seeing*.
pub fn interp_ticks() -> u32 {
    (interp_delay() / FIXED_DT).round() as u32
}

/// Replicated car state. Ground-plane physics: heading 0 faces +z,
/// forward = (sin h, cos h) on (x, z). y stays 0 — the presentation is
/// 3D, the simulation deliberately isn't (yet). This is the PREDICTED
/// substate only — every field is something `drive_step` evolves and the
/// client predicts then reconciles. Server-owned state the client must NOT
/// predict (scoring) lives in a separate `Score` pool joined by id, so an
/// un-stepped field can never freeze inside the predictor between
/// corrections — the bug that motivated the split.
#[derive(Clone, Copy, PartialEq, Debug, Default, Pod, Zeroable)]
#[repr(C)]
pub struct Car {
    pub x: f32,
    pub z: f32,
    pub heading: f32,
    pub speed: f32,
    /// Filtered steering: the wheel lags the commanded turn (control
    /// weight). Because it's part of the replicated/predicted state, the
    /// next ~0.5s of motion is determined — that's what makes the
    /// projected path a real prediction, not a dead-reckoned guess.
    pub steer: f32,
}

/// Server-owned scoring for a car, in its OWN replicated pool keyed by the
/// SAME id as the motion `Car`. Split out on purpose: `Car` is predicted,
/// this is authoritative-only — the server computes it from global
/// proximity and collisions, and it mirrors straight down to the HUD, read
/// raw. Folding it into `Car` let the un-stepped points freeze inside the
/// predictor between corrections (it only refreshed on a position rewind);
/// a separate pool — exactly what pools are for — makes that impossible.
#[derive(Clone, Copy, PartialEq, Debug, Default, Pod, Zeroable)]
#[repr(C)]
pub struct Score {
    pub points: f32,
    /// Live rate (points/sec) the HUD shows green/red; `-HIT_COST` while
    /// the post-collision lockout runs so the bite reads as a number.
    pub rate: f32,
    /// Seconds left on the post-collision charge lockout (`HIT_COOLDOWN`).
    pub hit_cd: f32,
}

/// Command-frame input payload.
#[derive(Clone, Copy, PartialEq, Debug, Default, Pod, Zeroable)]
#[repr(C)]
pub struct Drive {
    pub thrust: f32, // -1..1
    pub turn: f32,   // -1..1 (positive = left)
    pub drift: f32,  // 0/1: shift held — sharper steering, less drag
    pub bot: f32,    // 0/1: AI controller — its steering lags (see drive_step)
}

/// Reliable client→server event: "flip me back to my spawn." A discrete,
/// must-arrive intent that doesn't belong in the continuous `Drive` input
/// pod — the textbook one-way event (the *effect*, a reset car, comes back
/// as ordinary synced state). Carries nothing; `pad` only exists because a
/// wire pod needs a field.
#[derive(Clone, Copy, Default, Pod, Zeroable)]
#[repr(C)]
pub struct Respawn {
    pub pad: u32,
}

/// A billed collision, as a transient replicated FACT: the server spawns
/// one on a fresh id at the impact point and its `ttl_pool` lifetime
/// removes it — the contact-points pattern. No event channel down, no
/// client-side cleanup; clients just render whatever entries exist.
#[derive(Clone, Copy, PartialEq, Debug, Default, Pod, Zeroable)]
#[repr(C)]
pub struct Contact {
    pub x: f32,
    pub z: f32,
}

/// Contact marker lifetime (seconds). Comfortably above one resend window
/// (~RTT + a couple of snapshot intervals) so even a lossy client sees
/// every hit before it expires — the contact-points rule.
pub const CONTACT_TTL: f32 = 1.0;

/// Top speed (forward); the `drive_step` clamp and the speed-match scale.
pub const VMAX: f32 = 18.0;
/// Car collision capsule: a segment of half-length `CAR_HL` along the
/// car's forward axis, radius `CAR_R`. Total footprint 2*(HL+R) long by
/// 2*R wide — a car shape, NOT a fat circle, so side-by-side cars don't
/// falsely register as touching.
pub const CAR_HL: f32 = 0.8;
pub const CAR_R: f32 = 0.9;
/// Beyond this center-to-center distance a car contributes no score.
pub const SCORE_RANGE: f32 = 14.0;
/// Points per second per rival at point-blank (before the speed-match
/// bonus). The dominant payoff: just being near a moving car earns.
pub const SCORE_BASE: f32 = 40.0;
/// Points a collision costs outright. A flat bite, NOT a continuous rate:
/// the collision push flicks cars in and out of overlap every few ticks,
/// so a per-second drain barely accrued — a hit you can feel needs to be
/// an impulse, debounced (see `HIT_COOLDOWN`).
pub const HIT_COST: f32 = 25.0;
/// Seconds after a charged hit before another can land — debounces the
/// push bouncing you across the overlap boundary into repeat billing.
pub const HIT_COOLDOWN: f32 = 1.0;
/// Surface gap (capsule clearance) below which a rival grants NO proximity
/// reward — the contact dead-zone. Without it, point-blank pays the most,
/// so leaning on someone farms the reward back between hits. With it, the
/// scrape band earns nothing (the hit impulse owns contact) while near-miss
/// passes just outside it still pay the peak.
pub const CONTACT_DEADZONE: f32 = 0.6;
/// Below this speed a car neither earns nor grants score (no farming a
/// parked car; no points while stuck).
pub const MOVE_MIN: f32 = 1.5;
/// Steering control-lag time constant (seconds): the wheel reaches ~63%
/// of the commanded turn after this long. ~200-300ms of perceived lag.
pub const STEER_TAU: f32 = 0.18;

/// THE step. For BOT controllers (`cmd.bot`) steering LAGS the input (a
/// first-order filter on `c.steer`), giving the AI weight and a
/// meaningful lead — so its projected arrow is a real prediction. Human
/// players steer crisply (no lag); their arrow just traces the live turn.
/// Drift (shift) tightens the turn rate (and the bot lag). Speed-scaled
/// steering, quadratic-ish drag, hard arena walls that scrub speed.
pub fn drive_step(c: &mut Car, cmd: Drive, dt: f32) {
    let drifting = cmd.drift > 0.5;
    if cmd.bot > 0.5 {
        // Bot steering catches up to the commanded turn over STEER_TAU
        // (snappier while drifting) — the control lag behind the arrow.
        let tau = if drifting { 0.10 } else { STEER_TAU };
        let k = 1.0 - (-dt / tau).exp();
        c.steer += (cmd.turn - c.steer) * k;
    } else {
        c.steer = cmd.turn; // human: instant, crisp
    }

    let drag = if drifting { 0.6 } else { 1.2 };
    c.speed = (c.speed + cmd.thrust * 14.0 * dt) * (1.0 - drag * dt);
    c.speed = c.speed.clamp(-7.0, VMAX);
    let authority = (c.speed.abs() / 6.0).min(1.0);
    let turn_rate = if drifting { 3.4 } else { 2.2 };
    c.heading += c.steer * turn_rate * authority * dt * c.speed.signum();
    c.x += c.heading.sin() * c.speed * dt;
    c.z += c.heading.cos() * c.speed * dt;
    if c.x.abs() > ARENA {
        c.x = c.x.clamp(-ARENA, ARENA);
        c.speed *= 0.4;
    }
    if c.z.abs() > ARENA {
        c.z = c.z.clamp(-ARENA, ARENA);
        c.speed *= 0.4;
    }
}

/// Interpolate between two replicated car samples for snapshot
/// interpolation (`pm::pool_interp`). Linear on the scalar fields;
/// heading takes the SHORTEST arc so a car crossing the +z wrap (pi to
/// -pi) eases across instead of spinning the long way round.
pub fn car_lerp(a: &Car, b: &Car, t: f32) -> Car {
    let l = |x: f32, y: f32| x + (y - x) * t;
    let dh = (b.heading - a.heading + std::f32::consts::PI)
        .rem_euclid(std::f32::consts::TAU)
        - std::f32::consts::PI;
    Car {
        x: l(a.x, b.x),
        z: l(a.z, b.z),
        heading: a.heading + dh * t,
        speed: l(a.speed, b.speed),
        steer: l(a.steer, b.steer),
    }
}

/// Center-to-center distance between two cars on the ground plane.
pub fn car_dist(a: &Car, b: &Car) -> f32 {
    let (dx, dz) = (a.x - b.x, a.z - b.z);
    (dx * dx + dz * dz).sqrt()
}

/// A car's collision capsule as its two segment endpoints (front, back).
fn car_seg(c: &Car) -> ((f32, f32), (f32, f32)) {
    let (fx, fz) = (c.heading.sin() * CAR_HL, c.heading.cos() * CAR_HL);
    ((c.x - fx, c.z - fz), (c.x + fx, c.z + fz))
}

/// Closest distance between two 2D segments (Ericson, RTCD) and the unit
/// axis from segment 2's closest point toward segment 1's: `(nx, nz, d)`.
fn seg_seg(
    p1: (f32, f32),
    q1: (f32, f32),
    p2: (f32, f32),
    q2: (f32, f32),
) -> (f32, f32, f32) {
    let dot = |a: (f32, f32), b: (f32, f32)| a.0 * b.0 + a.1 * b.1;
    let sub = |a: (f32, f32), b: (f32, f32)| (a.0 - b.0, a.1 - b.1);
    let (d1, d2, r) = (sub(q1, p1), sub(q2, p2), sub(p1, p2));
    let (a, e, f) = (dot(d1, d1), dot(d2, d2), dot(d2, r));
    let eps = 1e-8;
    let (mut s, mut t) = (0.0f32, 0.0f32);
    if a <= eps && e <= eps {
        // both degenerate
    } else if a <= eps {
        t = (f / e).clamp(0.0, 1.0);
    } else {
        let c = dot(d1, r);
        if e <= eps {
            s = (-c / a).clamp(0.0, 1.0);
        } else {
            let b = dot(d1, d2);
            let denom = a * e - b * b;
            s = if denom.abs() > eps {
                ((b * f - c * e) / denom).clamp(0.0, 1.0)
            } else {
                0.0
            };
            t = (b * s + f) / e;
            if t < 0.0 {
                t = 0.0;
                s = (-c / a).clamp(0.0, 1.0);
            } else if t > 1.0 {
                t = 1.0;
                s = ((b - c) / a).clamp(0.0, 1.0);
            }
        }
    }
    let c1 = (p1.0 + d1.0 * s, p1.1 + d1.1 * s);
    let c2 = (p2.0 + d2.0 * t, p2.1 + d2.1 * t);
    let (dx, dz) = (c1.0 - c2.0, c1.1 - c2.1);
    let d = (dx * dx + dz * dz).sqrt();
    if d > 1e-4 {
        (dx / d, dz / d, d)
    } else {
        (1.0, 0.0, 0.0)
    }
}

/// Capsule overlap between two cars: `Some((nx, nz, penetration))` with a
/// unit push axis (from `b` toward `a`) when they intersect.
pub fn capsule_overlap(a: &Car, b: &Car) -> Option<(f32, f32, f32)> {
    let (p1, q1) = car_seg(a);
    let (p2, q2) = car_seg(b);
    let (nx, nz, d) = seg_seg(p1, q1, p2, q2);
    let min = 2.0 * CAR_R;
    (d < min).then_some((nx, nz, min - d))
}

/// Surface gap between two cars' capsules along the closest axis: 0 at
/// touching, NEGATIVE while overlapping. `capsule_overlap(..).is_some()`
/// is exactly `capsule_clearance(..) < 0.0`; this gives scoring the same
/// orientation-correct contact metric the collision push uses.
pub fn capsule_clearance(a: &Car, b: &Car) -> f32 {
    let (p1, q1) = car_seg(a);
    let (p2, q2) = car_seg(b);
    let (_, _, d) = seg_seg(p1, q1, p2, q2);
    d - 2.0 * CAR_R
}

/// The scoring RATE (points/sec) for `me`: an exponential proximity
/// reward that spikes on a close pass, weighted per rival by a SPEED
/// MATCH — both cars must be moving and at similar speed (a paced
/// near-miss, not buzzing a parked car). This is the POSITIVE earning rate
/// only; contact is owned by the flat hit impulse (`HIT_COST`), not a
/// per-second drain. Pays NOTHING inside the contact dead-zone so a scrape
/// can't farm the point-blank reward back between hits. Pure +
/// deterministic, but the server is the sole caller — the HUD reads the
/// banked `Score.rate`, it does not recompute this.
pub fn score_rate(me: &Car, others: &[Car]) -> f32 {
    let a = me.speed.abs();
    let mut gain = 0.0;
    for o in others {
        let clr = capsule_clearance(me, o); // surface gap; < 0 == overlap
        let d = car_dist(me, o);
        let b = o.speed.abs();
        if d > SCORE_RANGE || a < MOVE_MIN || b < MOVE_MIN {
            continue;
        }
        // Contact dead-zone: a rival you're scraping (or a hair off it)
        // earns nothing — the hit impulse, not the reward curve, is what
        // contact pays out.
        if clr < CONTACT_DEADZONE {
            continue;
        }
        let prox = (-d / 5.0).exp(); // 0..1, sharp when really close
        // Speed match is a BONUS, not a gate: half the reward for just
        // being near a moving car, the other half for pacing its speed.
        let speed_match = (1.0 - (a - b).abs() / VMAX).clamp(0.0, 1.0);
        gain += prox * (0.5 + 0.5 * speed_match);
    }
    SCORE_BASE * gain
}

/// Positional-push collision resolution (capsule): separate every
/// overlapping pair along the contact normal — half the penetration each
/// plus a small bias so resting contacts break instead of sticking.
pub fn collide_push(cars: &[Car]) -> Vec<(f32, f32)> {
    let mut push = vec![(0.0f32, 0.0f32); cars.len()];
    for i in 0..cars.len() {
        for j in (i + 1)..cars.len() {
            if let Some((nx, nz, pen)) = capsule_overlap(&cars[i], &cars[j]) {
                let s = pen * 0.5 + 0.04; // split + un-stick bias
                push[i].0 += nx * s;
                push[i].1 += nz * s;
                push[j].0 -= nx * s;
                push[j].1 -= nz * s;
            }
        }
    }
    push
}

/// Replay `drive_step` forward from `c` holding `cmd`, collecting the
/// ground positions each step — the projected path (a real prediction
/// because steering lag makes the near future deterministic).
pub fn predict_path(mut c: Car, cmd: Drive, steps: u32, dt: f32) -> Vec<(f32, f32)> {
    let mut pts = Vec::with_capacity(steps as usize);
    for _ in 0..steps {
        drive_step(&mut c, cmd, dt);
        pts.push((c.x, c.z));
    }
    pts
}

/// Per-peer body tints.
pub const PCOL: [(f32, f32, f32); 8] = [
    (0.98, 0.82, 0.16), // you (peer colors start at 1; index peer-1)
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

/// Spawn slot for a peer: spread along the back wall, facing +z.
pub fn spawn_car(peer: u8) -> Car {
    Car {
        x: (peer as f32 - 4.5) * 5.0,
        z: -ARENA + 6.0,
        heading: 0.0,
        speed: 0.0,
        steer: 0.0,
    }
}
