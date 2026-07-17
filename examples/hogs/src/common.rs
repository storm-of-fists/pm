//! Shared hogs definitions: the replicated pods, THE truck step (same
//! code on server and in client prediction replay — drive's lesson), and
//! the pure geometry both sides use. Hogs are server-owned NPCs: clients
//! never step them, only interpolate — so `hog` state has no client-side
//! step function at all, just a lerp.

use pm::{Body, Id, Quat, vec3};

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

// --- game params (docs/params.md) ----------------------------------------

/// One tunable's contract: its file/signal name, live range, and shipped
/// default. The table drives everything — the file codec, the server's
/// clamp, and the telemetry knobs — so adding a param is one line here
/// plus one field on [`Params`].
pub struct ParamSpec {
    pub name: &'static str,
    pub default: f32,
    pub min: f32,
    pub max: f32,
}

/// Spec order IS [`Params`] field order. The count is pinned at compile
/// time by `Params::as_array`'s `must_cast`; the name↔field pairing is
/// pinned by the `params_spec_matches_fields` test below.
pub const PARAM_SPECS: [ParamSpec; 36] = [
    // Stage-1 pilot set. Spec order IS field order (the as_array cast),
    // so new params APPEND — a reshuffle must touch struct and table
    // together or the cast pairs values with the wrong fields.
    ParamSpec { name: "wave_base", default: 40.0, min: 1.0, max: 1000.0 },
    ParamSpec { name: "wave_grow", default: 15.0, min: 0.0, max: 200.0 },
    ParamSpec { name: "gunner_frac", default: 0.25, min: 0.0, max: 1.0 },
    ParamSpec { name: "friendly_dmg", default: 0.25, min: 0.0, max: 1.0 },
    ParamSpec { name: "gun_dmg", default: 0.5, min: 0.01, max: 1.0 },
    ParamSpec { name: "bite_dmg", default: 0.25, min: 0.0, max: 1.0 },
    ParamSpec { name: "hog_fast", default: 11.0, min: 1.0, max: 30.0 },
    ParamSpec { name: "net_kbps", default: 2000.0, min: 64.0, max: 6000.0 },
    ParamSpec { name: "ai_stride", default: 4.0, min: 1.0, max: 8.0 },
    // Stage 2a (2026-07-17): the remaining server-read tuning consts.
    ParamSpec { name: "hog_aggro", default: 26.0, min: 5.0, max: 80.0 },
    ParamSpec { name: "hog_roam", default: 4.5, min: 0.5, max: 15.0 },
    ParamSpec { name: "hog_flee", default: 1.5, min: 0.0, max: 6.0 },
    ParamSpec { name: "bite_cd", default: 1.0, min: 0.1, max: 5.0 },
    ParamSpec { name: "bite_cost", default: 15.0, min: 0.0, max: 100.0 },
    ParamSpec { name: "kill_points", default: 10.0, min: 0.0, max: 100.0 },
    ParamSpec { name: "death_cost", default: 30.0, min: 0.0, max: 200.0 },
    ParamSpec { name: "knock", default: 9.0, min: 0.0, max: 30.0 },
    ParamSpec { name: "gun_cd", default: 0.25, min: 0.05, max: 2.0 },
    ParamSpec { name: "gun_range", default: 45.0, min: 10.0, max: 120.0 },
    ParamSpec { name: "bullet_speed", default: 70.0, min: 20.0, max: 200.0 },
    ParamSpec { name: "hit_pad_truck", default: 0.35, min: 0.0, max: 2.0 },
    ParamSpec { name: "hit_pad_heli", default: 0.8, min: 0.0, max: 2.0 },
    ParamSpec { name: "hoggun_cd", default: 1.6, min: 0.2, max: 6.0 },
    ParamSpec { name: "hoggun_range", default: 28.0, min: 5.0, max: 80.0 },
    ParamSpec { name: "hoggun_dmg", default: 0.12, min: 0.0, max: 1.0 },
    ParamSpec { name: "heli_tail_kick", default: 0.5, min: 0.0, max: 2.0 },
    ParamSpec { name: "hog_leap", default: 2.4, min: 0.5, max: 8.0 },
    // Stage 2b (2026-07-17): SHARED-STEP constants. The client
    // predictors replay these, so they read the replicated single —
    // a live change mispredicts for one snapshot interval (a single
    // correction blip) and converges; soak-verified at lag=80/loss=3%.
    ParamSpec { name: "vmax", default: 18.0, min: 5.0, max: 40.0 },
    ParamSpec { name: "boost_vmax", default: 30.0, min: 10.0, max: 60.0 },
    ParamSpec { name: "truck_grip", default: 8.0, min: 0.5, max: 20.0 },
    ParamSpec { name: "truck_grip_boost", default: 3.2, min: 0.5, max: 20.0 },
    ParamSpec { name: "heat_rate", default: 0.4, min: 0.05, max: 2.0 },
    ParamSpec { name: "heat_cool", default: 0.25, min: 0.05, max: 2.0 },
    ParamSpec { name: "heli_lift", default: 16.0, min: 4.0, max: 40.0 },
    ParamSpec { name: "heli_t_max", default: 34.0, min: 10.0, max: 80.0 },
    ParamSpec { name: "heli_yaw", default: 1.9, min: 0.5, max: 5.0 },
];

/// Server-owned tuning scalars (docs/params.md): seeded from the params
/// file at startup, live-writable through the `"param.set"` event, and
/// replicated to every client as the `"params"` synced single. Server
/// tasks read these where the old consts used to be.
///
/// The derived `Default` is bytemuck's ZEROS — use [`Params::from_specs`]
/// for the shipped values (the replica single is zeros for the instant
/// before the first snapshot lands; nothing reads it that early).
#[pm::pod]
pub struct Params {
    /// First-wave horde size (was the `PM_HOGS` env knob).
    pub wave_base: f32,
    /// Extra hogs per wave past the first.
    pub wave_grow: f32,
    /// Fraction of each wave that spawns with a shoulder gun.
    pub gunner_frac: f32,
    /// Friendly-fire chip per cannon hit (gentler than `gun_dmg` —
    /// punish spraying, don't two-shot a buddy).
    pub friendly_dmg: f32,
    /// Cannon damage per hit on a hog (hp scale is 1.0).
    pub gun_dmg: f32,
    /// Truck/heli chip per hog bite.
    pub bite_dmg: f32,
    /// Hog chase/flee speed, u/s (roam speed stays `HOG_ROAM`).
    pub hog_fast: f32,
    /// Per-peer snapshot bandwidth, kilobits/sec — feeds the engine's
    /// send tune (`PmServer::send_tune`): how far the multi-datagram
    /// flight may extend past the always-sent first datagram. Low
    /// values degrade to the classic one-datagram cadence (~64 at the
    /// floor), never below it.
    pub net_kbps: f32,
    /// Hog think cadence: each hog re-decides (target scan, steering
    /// goal, bite) every Nth tick, staggered across the horde so cohorts
    /// alternate; movement integrates every tick regardless, so the
    /// horde stays change-dense on the wire.
    pub ai_stride: f32,

    // --- stage 2a: server-read tuning ---------------------------------
    /// A vehicle inside this range gets charged.
    pub hog_aggro: f32,
    /// Roam speed, u/s (charge/flee speed is `hog_fast`).
    pub hog_roam: f32,
    /// After a bite the hog breaks off for this long (seconds).
    pub hog_flee: f32,
    /// Per-hog re-bite lockout (seconds) — debounces overlap flicker.
    pub bite_cd: f32,
    /// Points a bite costs the team.
    pub bite_cost: f32,
    /// Points a kill earns the team.
    pub kill_points: f32,
    /// Points an exploded/downed vehicle costs the team (on top of the
    /// bites that probably caused it).
    pub death_cost: f32,
    /// Bullet-hit knockback speed on a surviving hog (u/s; the decay
    /// rate stays the paired `KNOCK_DECAY` const).
    pub knock: f32,
    /// Turret refire period (seconds). The client's cosmetic gun reads
    /// the replica so the click-tracer cadence matches the server's.
    pub gun_cd: f32,
    /// Bullet max travel (also the client aim line's reach).
    pub gun_range: f32,
    /// Bullet speed, u/s (also flies the client's cosmetic tracers and
    /// the bots' lead arithmetic).
    pub bullet_speed: f32,
    /// Friendly-fire hit-circle padding by victim platform: forgiveness
    /// for shots that would graze a teammate (heli > truck — the heli
    /// is the one you sweep past at speed).
    pub hit_pad_truck: f32,
    pub hit_pad_heli: f32,
    /// Gunner-hog refire period (seconds; each hog randomizes ±35%).
    pub hoggun_cd: f32,
    /// Gunner-hog engagement range.
    pub hoggun_range: f32,
    /// Gunner-hog chip per hit (lighter than a teammate's cannon).
    pub hoggun_dmg: f32,
    /// Tail-boom hit: yaw kick scale (torque scales with obliquity).
    pub heli_tail_kick: f32,
    /// Hog reach ceiling: bites and aggro only reach a heli hovering
    /// below this altitude — climb and the horde loses you.
    pub hog_leap: f32,

    // --- stage 2b: SHARED-STEP constants -------------------------------
    // The predictors replay these (truck_step / heli_step read them), so
    // clients read the REPLICA — server and client values agree except
    // for the one snapshot interval after a live change (one correction
    // blip, converges).
    /// Truck top speed (forward), and boosted.
    pub vmax: f32,
    pub boost_vmax: f32,
    /// Tire grip: how fast LATERAL velocity bleeds (1/s exponential
    /// rate). This is the whole "physics" of the truck — steering turns
    /// the chassis, grip drags the momentum around after it. High =
    /// rails; low = ice. Boosting loosens the rear (powerslide).
    pub truck_grip: f32,
    pub truck_grip_boost: f32,
    /// Heat per second while boosting / cooling per second while not.
    pub heat_rate: f32,
    pub heat_cool: f32,
    /// Collective authority above/below hover trim (m/s^2-ish), and the
    /// total thrust ceiling.
    pub heli_lift: f32,
    pub heli_t_max: f32,
    /// Tail-rotor yaw rate (rad/s).
    pub heli_yaw: f32,
}

impl Params {
    pub fn from_specs() -> Params {
        let mut p = Params::default();
        for (i, s) in PARAM_SPECS.iter().enumerate() {
            p.as_array_mut()[i] = s.default;
        }
        p
    }

    /// The fields as the spec-ordered array the codec/event path indexes.
    /// `must_cast` fails to COMPILE if the field count drifts off the
    /// spec table.
    pub fn as_array(&self) -> &[f32; PARAM_SPECS.len()] {
        bytemuck::must_cast_ref(self)
    }

    pub fn as_array_mut(&mut self) -> &mut [f32; PARAM_SPECS.len()] {
        bytemuck::must_cast_mut(self)
    }
}

/// Default params file path; a `params=PATH` arg overrides. Local tuning
/// state, gitignored — the shipped defaults live in [`PARAM_SPECS`].
pub const PARAMS_FILE: &str = "hogs.params";

/// Load the params file: pm-control save-file shape (`name=value` per
/// line, anything after a space is a human aid, `#` starts a comment).
/// Missing file or missing names keep spec defaults, unknown names warn
/// and load on (an old file is normal, a typo shouldn't be silent), and
/// every value CLAMPS to its spec range — a hand-edited file never loads
/// raw.
pub fn params_load(path: &str) -> Params {
    let mut p = Params::from_specs();
    let Ok(text) = std::fs::read_to_string(path) else {
        return p; // no file: shipped defaults
    };
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, rest)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let value = rest.split(' ').next().unwrap_or(rest).trim();
        let Some(i) = PARAM_SPECS.iter().position(|s| s.name == key) else {
            eprintln!("[params] {path}: unknown param '{key}' ignored");
            continue;
        };
        match value.parse::<f32>() {
            Ok(v) => p.as_array_mut()[i] = v.clamp(PARAM_SPECS[i].min, PARAM_SPECS[i].max),
            Err(_) => eprintln!("[params] {path}: bad value for '{key}' ignored"),
        }
    }
    p
}

/// Rewrite the params file from the authoritative set — the server's
/// answer to a [`PARAM_SAVE`] event. Whole-file rewrite, spec order,
/// range as the trailing human aid.
pub fn params_save(path: &str, p: &Params) -> std::io::Result<()> {
    use std::fmt::Write as _;
    let mut out = String::from("# hogs params — name=value, edited live via pm-watch (docs/params.md)\n");
    for (i, s) in PARAM_SPECS.iter().enumerate() {
        let _ = writeln!(out, "{}={} {}..{}", s.name, p.as_array()[i], s.min, s.max);
    }
    std::fs::write(path, out)
}

/// Reliable client→server event: set `params[idx] = value` (the server
/// clamps to the spec range). `idx ==` [`PARAM_SAVE`] instead persists
/// the current set to the server's params file.
#[pm::pod]
pub struct ParamSet {
    pub idx: u32,
    pub value: f32,
}

/// [`ParamSet::idx`] sentinel: "save the set to disk now".
pub const PARAM_SAVE: u32 = u32::MAX;

/// Parsed CLI flags every client run cares about (see main.rs header
/// for the grammar). One struct so signatures stop growing a parameter
/// per knob.
#[derive(Clone)]
pub struct Flags {
    /// (one-way lag ms, loss fraction) — the simulated link.
    pub link: (f32, f32),
    /// Day-night cycle length, seconds.
    pub day: f32,
    /// Interp delay in force, ms (report-only; frozen at creation).
    pub interp_ms: f32,
    /// Telemetry monitor address (`mon=IP:PORT`).
    pub mon: String,
    /// Game params loaded from the params file in `main` (before any
    /// thread spawns) — the client seeds its telemetry knobs from this.
    pub params: Params,
}

/// Live-tunable client knobs, bridged from the telemetry node's signals
/// into a pm single (`"hogs.tune"`) that game tasks read each frame.
#[derive(Clone, Copy)]
pub struct Tune {
    pub day_secs: f32,
}

impl Default for Tune {
    fn default() -> Self {
        Tune { day_secs: 480.0 }
    }
}

// --- replicated pods -----------------------------------------------------

/// Replicated truck state — the PREDICTED substate only, same discipline
/// as drive's Car: every field is something `truck_step` evolves. The
/// kinematic chunk is the shared [`pm::Body`] (embedded, per the
/// predicted-pod contract — pose and velocity must live in the pod the
/// step evolves): a truck is `Body` with the ground-vehicle constraints
/// (pos.y = 0, rot pure yaw, vel along forward) applied by its step.
#[pm::pod]
pub struct Truck {
    pub body: Body,
    /// Filtered steering (bots lag; humans are crisp) — replicated so a
    /// truck's near future is determined, like drive.
    pub steer: f32,
    /// Turret angle relative to heading (the mouse-aim seam). Evolved
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

impl Truck {
    /// The 2D heading gameplay reads everywhere (yaw of the body).
    pub fn heading(&self) -> f32 {
        self.body.yaw()
    }

    /// Signed forward speed — the forward component of the momentum.
    /// (`vel` may also carry a lateral sliding component; grip in
    /// `truck_step` is what bleeds it. Speedometers and gameplay
    /// checks want this, not `vel.len()`.)
    pub fn speed(&self) -> f32 {
        self.body.vel.dot(self.body.fwd())
    }
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

/// Replicated helicopter state — the other player vehicle, and the
/// engine's first full-3D predicted pod. It is EXACTLY a [`pm::Body`]:
/// attitude lives in the quaternion (pitch/roll limits are enforced by
/// the step via yaw-pitch-roll extract/clamp/rebuild — a jet would skip
/// the extraction and integrate body rates on the quat directly).
/// Deliberately NOT quantized: predicted pools stay full precision so
/// reconcile error never sits at the quantization step. Flight model:
/// one rotor-thrust vector along body-up vs gravity, fly-by-wire hover
/// trim, collective burns above it — see `heli_step`.
#[pm::pod]
pub struct Heli {
    pub body: Body,
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
    /// Muzzle height at spawn, then integrated by `pitch` — the 3D part.
    /// Truck shots fly flat at barrel height; heli shots descend along
    /// the nose. Hits require the shot's altitude inside the hog's
    /// `HOG_H` band, so the pod carries the whole trajectory.
    #[wire(i16, scale = 64.0)]
    pub y: f32,
    #[wire(i16, scale = 64.0)]
    pub z: f32,
    #[wire(i16, scale = 10000.0)]
    pub heading: f32,
    /// Climb angle: dy per unit of travel is `sin(pitch)`. 0 for trucks.
    #[wire(i16, scale = 10000.0)]
    pub pitch: f32,
    /// Which peer fired it. A client HIDES its own replicated bullets —
    /// it already drew a local [`Tracer`] at the click (the ~RTT-late
    /// twin would double-draw) — and skips their bang in sfx the same
    /// way. Whole small numbers, so the u8 roundtrip is exact.
    #[wire(u8)]
    pub owner: f32,
}

/// CLIENT-LOCAL cosmetic tracer — never synced, no wire repr: your own
/// shot, spawned at the CLICK from the predicted muzzle so the gun
/// answers your finger at 0 ms. The authoritative [`Bullet`] (hits,
/// damage, what other players see) still round-trips; `Bullet::owner`
/// is what keeps the two from both drawing. Flies and dies on the same
/// walls as the real one (`tracer_step`), minus hog tests — the kill
/// flash is the server's word and arrives when it arrives.
#[pm::pod]
pub struct Tracer {
    pub x: f32,
    pub y: f32,
    pub z: f32,
    pub heading: f32,
    pub pitch: f32,
    pub left: f32,
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
    pub thrust: f32, // -1..1 (truck only)
    pub turn: f32,   // -1..1: steer (truck) / yaw (heli)
    pub fire: f32,   // 0/1: trigger held
    pub aim: f32,    // turret angle relative to heading, +-AIM_MAX (truck only)
    pub boost: f32,  // 0/1: burn heat for speed (truck only)
    pub bot: f32,    // 0/1: AI controller — its steering lags
    // Heli axes, dead weight in a truck. ONE continuous channel per
    // connection is the input doctrine, so the pod is the union of every
    // vehicle's axes and each step reads its own — the seam input-map
    // will eventually own (per-vehicle key contexts live client-side).
    pub pitch: f32, // -1..1: nose down (forward) / up (heli only)
    pub lift: f32,  // -1..1: collective climb / descend (heli only)
}

/// Reliable client→server event: respawn as the chosen vehicle (the
/// server swaps your ENTITY — see the server's respawn task for why a
/// swap must be a fresh id).
#[pm::pod]
pub struct Respawn {
    pub vehicle: u32, // VEH_TRUCK | VEH_HELI
}

pub const VEH_TRUCK: u32 = 0;
pub const VEH_HELI: u32 = 1;

// --- tuning ----------------------------------------------------------------

// Tuning that designers move live migrated into [`Params`] (stages 1+2,
// docs/params.md). What remains const here is STRUCTURAL: geometry,
// physics identities, and control internals whose live mutation would
// be meaningless or break contracts.

/// Gravity (also the heli's hover-trim baseline).
pub const G: f32 = 9.81;
/// Truck hitpoints (what one bite chips is `Params::bite_dmg`; hp is
/// the scale everything else is expressed in, so it stays 1.0).
pub const TRUCK_HP: f32 = 1.0;
/// Truck collision capsule: half-length along forward, radius.
pub const TRUCK_HL: f32 = 0.8;
pub const TRUCK_R: f32 = 0.9;
/// Steering control-lag time constant for bot drivers (seconds).
pub const STEER_TAU: f32 = 0.18;

/// Hog body radius (they're round; the biomod part is the attitude).
pub const HOG_R: f32 = 0.7;
/// Shots to drop a hog: HOG_HP / `Params::gun_dmg`.
pub const HOG_HP: f32 = 1.0;
/// Hog turn rate (rad/s) — slower than a truck can steer, so you can
/// juke a charge.
pub const HOG_TURN: f32 = 2.6;
/// While roaming, a hog walks to a random goal and picks a new one
/// inside this many seconds (or on arrival) — real wandering, not the
/// old stand-and-wiggle.
pub const ROAM_REPICK: f32 = 9.0;
/// Turret swing limit either side of straight ahead.
pub const AIM_MAX: f32 = 2.6;
/// Hog GAMEPLAY hit ceiling: a shot connects if its altitude is inside
/// [0, HOG_H] at the hit point. Taller than the drawn hog on purpose —
/// truck barrels sit at ~1.45 and flat shots must keep connecting (2D
/// behavior preserved); it's a hitbox, not a silhouette.
pub const HOG_H: f32 = 1.8;

// --- gunner hogs -------------------------------------------------------------

// Biomod gunner hogs: a fraction of every wave (`Params::gunner_frac`)
// spawns with a shoulder gun and fires REAL bullets (same `Bullet` pool
// — tracers, bangs, building hits, and the collider sweep all come
// free) at the nearest vehicle in 3D range. They are deliberately bad
// shots — angular spread, no target leading — but a low helicopter is
// in range of many at once, and that's the point: altitude is safety,
// the deck gets you fried.

/// Max gunner bullet travel (a touch past `Params::hoggun_range` so
/// edge shots complete at the default; a live range crank past it just
/// shortens edge shots).
pub const HOGGUN_TRAVEL: f32 = 32.0;
/// Aim error, ± radians on heading AND climb: "kinda bad". At 20 u
/// that's a ~±2.8 u miss cone vs a ~1 u cabin.
pub const HOGGUN_SPREAD: f32 = 0.14;
/// Muzzle height (shoulder-mounted) and where they aim on a truck.
pub const HOGGUN_Y: f32 = 0.6;
pub const TRUCK_AIM_Y: f32 = 1.0;

// --- helicopter tuning -------------------------------------------------------

/// How hard the heli cyclic chases the stick (1/s) — attitude is still
/// first-order servo'd; the FORCES are honest.
pub const HELI_ATT_K: f32 = 5.0;
/// Attitude limits: pitch tilts up to ~40°, banks up to ~29°. Tilt is
/// the throttle now (it vectors the rotor), so the nose gets more range
/// than the old cosmetic lean.
pub const HELI_PITCH_MAX: f32 = 0.70;
pub const HELI_ROLL_MAX: f32 = 0.50;
/// Airframe drag, split by axis: the rotor disc brakes horizontal
/// motion gently (full nose-down cruises ≈ 30 u/s — still the fastest
/// thing in the arena), induced drag damps vertical (this is what makes
/// centered-stick hover settle instead of bobbing).
pub const HELI_HDRAG: f32 = 0.28;
pub const HELI_VDRAG: f32 = 1.6;
/// Hard horizontal airspeed cap (advancing-blade limit, flavor-wise):
/// full collective + full tilt would otherwise run away.
pub const HELI_VCAP: f32 = 34.0;
/// Altitude band: skid height when landed, hard ceiling.
pub const HELI_GROUND: f32 = 0.6;
pub const HELI_CEIL: f32 = 45.0;
/// Hull circle for buildings/bites (the client's hold-fire
/// approximation too).
pub const HELI_R: f32 = 1.4;
/// Stage-4 part geometry (docs/collisions.md §7.4): cabin ball, tail
/// boom capsule behind it, rotor disc above. The old single ball
/// (`HELI_R`, `heli_hull`) stays as the CLIENT's hold-fire
/// approximation — a courtesy heuristic, not a judge.
pub const HELI_CABIN_R: f32 = 1.0;
pub const HELI_TAIL_R: f32 = 0.45;
/// Tail boom near/far distance behind the cabin center.
pub const HELI_TAIL_A: f32 = 1.2;
pub const HELI_TAIL_B: f32 = 2.8;
pub const HELI_ROTOR_R: f32 = 1.7;
/// Rotor disc altitude band, relative to the body center.
pub const HELI_ROTOR_LO: f32 = 0.6;
pub const HELI_ROTOR_HI: f32 = 1.1;

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

/// `building_push` for something at altitude `y`: only buildings whose
/// roof is above you shove the hull — above the roofline you overfly.
/// Same closest-point math so ground-level callers stay byte-identical.
pub fn building_push_below(x: f32, z: f32, r: f32, y: f32) -> (f32, f32, f32, f32) {
    let (mut x, mut z) = (x, z);
    let (mut nx, mut nz) = (0.0, 0.0);
    for &(bx, bz, hw, hd, bh) in &BUILDINGS {
        if y >= bh {
            continue;
        }
        let cx = x.clamp(bx - hw, bx + hw);
        let cz = z.clamp(bz - hd, bz + hd);
        let (dx, dz) = (x - cx, z - cz);
        let d2 = dx * dx + dz * dz;
        if d2 >= r * r {
            continue;
        }
        if d2 > 1e-8 {
            let d = d2.sqrt();
            nx = dx / d;
            nz = dz / d;
            x = cx + nx * r;
            z = cz + nz * r;
        } else {
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

/// Roof height at `(x, z)`: the tallest building whose footprint covers
/// the point, 0.0 in the open — the bullets' altitude gate for walls.
pub fn building_top(x: f32, z: f32) -> f32 {
    BUILDINGS
        .iter()
        .filter(|&&(bx, bz, hw, hd, _)| (x - bx).abs() < hw && (z - bz).abs() < hd)
        .map(|&(_, _, _, _, h)| h)
        .fold(0.0, f32::max)
}

// --- muzzles + cosmetic tracers ----------------------------------------------

/// Muzzle pose, `(x, y, z, heading, climb)` — ONE definition so the
/// server's real bullet and the client's cosmetic tracer (spawned at
/// the click from PREDICTED pose) leave the same barrel the same way.
/// Turret muzzle at the barrel tip: flat shot.
pub fn truck_muzzle(t: &Truck) -> (f32, f32, f32, f32, f32) {
    let dir = t.heading() + t.aim;
    let (x, z) = (t.body.pos.x, t.body.pos.z);
    (x + dir.sin() * 1.9, 1.45, z + dir.cos() * 1.9, dir, 0.0)
}

/// Heli nose gun fires where the nose points — dive to strafe the
/// horde. Body pitch>0 = nose down, so the bullet's climb is its
/// negation.
pub fn heli_muzzle(h: &Heli) -> (f32, f32, f32, f32, f32) {
    let b = h.body;
    let (yaw, pitch, _) = b.rot.to_yaw_pitch_roll();
    (
        b.pos.x + yaw.sin() * 2.3,
        (b.pos.y - 0.35).max(0.2),
        b.pos.z + yaw.cos() * 2.3,
        yaw,
        -pitch,
    )
}

/// Advance a cosmetic [`Tracer`] one `dt`; `false` = expired. Dies on
/// exactly the walls the real bullet dies on (ground, buildings below
/// the roofline, arena, ceiling, range) so the visual never outlives
/// where the shot could truthfully be — hogs excepted, on purpose.
pub fn tracer_step(tr: &mut Tracer, dt: f32, speed: f32) -> bool {
    let step = speed * dt;
    tr.x += tr.heading.sin() * tr.pitch.cos() * step;
    tr.z += tr.heading.cos() * tr.pitch.cos() * step;
    tr.y += tr.pitch.sin() * step;
    tr.left -= step;
    tr.left > 0.0
        && tr.y > 0.0
        && !(tr.y < building_top(tr.x, tr.z) && in_building(tr.x, tr.z, 0.0))
        && tr.x.abs() <= ARENA
        && tr.z.abs() <= ARENA
        && tr.y <= HELI_CEIL
}

// --- THE truck step ----------------------------------------------------------

/// THE step — force-based ground vehicle: bot steering lags (first-order
/// filter, so the near future is a real prediction), humans steer crisp.
/// Steering turns the CHASSIS; the momentum vector follows through tire
/// grip (lateral velocity decays at `TRUCK_GRIP`), so hard corners at
/// speed carry sideways momentum, boost loosens into a powerslide, and a
/// server shove (bite scrub, knockback) is real momentum the tires then
/// grip out — friction, not scripting. Ground constraints still project
/// into the shared `Body` (pos.y = 0, rot pure yaw); `vel` is now the
/// true 2D momentum, and `Truck::speed()` reads its forward component.
pub fn truck_step(t: &mut Truck, cmd: Drive, dt: f32, p: &Params) {
    // COMPILE-TIME COVERAGE: an exhaustive destructure (no `..`), so
    // adding a Truck field refuses to compile until it's named here —
    // and the rule this line sends you here to obey is: every field in
    // the predicted pod must be EVOLVED BY THIS FUNCTION from the
    // command. If the server writes it outside this step (damage,
    // pickups), it does NOT belong in Truck — give it its own
    // authoritative pool (that's why hp lives in `Health`). Then cover
    // the new field in `err_metric` and `truck_lerp` below.
    let Truck {
        body: _,
        steer: _,
        aim: _,
        heat: _,
    } = *t;
    let mut heading = t.heading();
    let speed = t.speed();

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
        (t.heat + p.heat_rate * dt).min(1.0)
    } else {
        (t.heat - p.heat_cool * dt).max(0.0)
    };
    let (accel, vmax) = if boosting {
        (26.0, p.boost_vmax)
    } else {
        (14.0, p.vmax)
    };
    // Steering turns the chassis (front-wheel authority still scales
    // with forward speed) — the momentum vector is caught up below.
    let authority = (speed.abs() / 6.0).min(1.0);
    heading = wrap_angle(heading + t.steer * 2.2 * authority * dt * speed.signum());
    // Decompose the world-frame momentum against the NEW chassis axes:
    // engine force + rolling drag act along forward, tire grip bleeds
    // whatever is left pointing out the doors.
    let (mut vx, mut vz) = (t.body.vel.x, t.body.vel.z);
    let (fx, fz) = (heading.sin(), heading.cos());
    let (rx, rz) = (heading.cos(), -heading.sin());
    let vf = ((vx * fx + vz * fz) + cmd.thrust * accel * dt) * (1.0 - 1.2 * dt);
    let vf = vf.clamp(-7.0, vmax);
    let grip = if boosting { p.truck_grip_boost } else { p.truck_grip };
    let vl = (vx * rx + vz * rz) * (-grip * dt).exp();
    vx = fx * vf + rx * vl;
    vz = fz * vf + rz * vl;
    let (mut x, mut z) = (t.body.pos.x, t.body.pos.z);
    x += vx * dt;
    z += vz * dt;
    if x.abs() > ARENA {
        x = x.clamp(-ARENA, ARENA);
        vx *= 0.4;
        vz *= 0.4;
    }
    if z.abs() > ARENA {
        z = z.clamp(-ARENA, ARENA);
        vx *= 0.4;
        vz *= 0.4;
    }
    // Buildings: same shared step on both sides, so driving into one
    // predicts byte-exact. The truck collides as a circle — close enough
    // at driving speeds, and capsule-vs-box isn't worth the code here.
    let (px, pz, nx, nz) = building_push(x, z, TRUCK_R + 0.3);
    if nx != 0.0 || nz != 0.0 {
        x = px;
        z = pz;
        // Momentum can point INTO the wall now (it used to ride the
        // heading): kill that component, keep the slide, and grind off
        // some of the rest.
        let into = vx * nx + vz * nz;
        if into < 0.0 {
            vx -= into * nx;
            vz -= into * nz;
        }
        vx *= 1.0 - 1.6 * dt;
        vz *= 1.0 - 1.6 * dt;
    }
    // Project back into the shared body under the ground constraints.
    t.body.pos = vec3(x, 0.0, z);
    t.body.rot = Quat::from_yaw(heading);
    t.body.vel = vec3(vx, 0.0, vz);
}

// --- THE heli step -----------------------------------------------------------

/// THE heli step — same contract as `truck_step`: shared by the server
/// and client prediction, so flying is byte-exact under replay. Rotor
/// physics: the tail rotor is the yaw rate, the cyclic servos attitude
/// (extract → clamp → rebuild on the quat), and the main rotor is ONE
/// thrust vector along body-up fighting real gravity — a fly-by-wire
/// collective trims it to hover at centered stick, the lift stick burns
/// above/below trim, and tilt vectors the force. Skids catch the ground,
/// buildings shove the hull only below their roofline.
pub fn heli_step(h: &mut Heli, cmd: Drive, dt: f32, p: &Params) {
    // COMPILE-TIME COVERAGE — the predicted-pod contract, same as
    // truck_step: every field here is evolved from the command by THIS
    // function. Cover new fields in `heli_err` and `heli_lerp` too.
    let Heli { body: _ } = *h;
    let b = &mut h.body;

    // Attitude on the quat via the constrained-vehicle path: extract,
    // steer, rebuild. Yaw wraps at the write like every angle; pitch
    // and roll ease toward the stick (yaw input banks the roll).
    let (yaw0, pitch0, roll0) = b.rot.to_yaw_pitch_roll();
    let yaw = wrap_angle(yaw0 + cmd.turn * p.heli_yaw * dt);
    let k = 1.0 - (-HELI_ATT_K * dt).exp();
    let pitch = pitch0 + (cmd.pitch.clamp(-1.0, 1.0) * HELI_PITCH_MAX - pitch0) * k;
    let roll = roll0 + (-cmd.turn.clamp(-1.0, 1.0) * HELI_ROLL_MAX - roll0) * k;
    b.rot = Quat::from_yaw_pitch_roll(yaw, pitch, roll).norm();

    // Main rotor: ONE thrust vector along body-up, against real gravity.
    // Fly-by-wire collective trims to exactly cancel gravity at centered
    // stick (trim = G / up.y — hands-off hover by construction, level or
    // tilted); the lift stick burns above/below trim. The tilt DIRECTION
    // does everything else: nose-down vectors those newtons forward,
    // banking slides you into the turn (the tail-rotor yaw above banks
    // the roll, so turns are coordinated), and because trim follows
    // attitude, a hard dive costs you climb authority — the machine has
    // momentum and a weight now, not axes.
    let up = b.up();
    let trim = G / up.y.clamp(0.6, 1.0);
    let thrust = (trim + cmd.lift.clamp(-1.0, 1.0) * p.heli_lift).clamp(0.0, p.heli_t_max);
    b.vel.x = (b.vel.x + up.x * thrust * dt) * (1.0 - HELI_HDRAG * dt);
    b.vel.z = (b.vel.z + up.z * thrust * dt) * (1.0 - HELI_HDRAG * dt);
    b.vel.y = (b.vel.y + (up.y * thrust - G) * dt) * (1.0 - HELI_VDRAG * dt);
    // Advancing-blade cap: full collective + full tilt can't run away.
    let h2 = b.vel.x * b.vel.x + b.vel.z * b.vel.z;
    if h2 > HELI_VCAP * HELI_VCAP {
        let s = HELI_VCAP / h2.sqrt();
        b.vel.x *= s;
        b.vel.z *= s;
    }
    b.integrate(dt);

    // Altitude band: skids on the deck (extra drag — parked, not
    // sliding), hard ceiling.
    if b.pos.y <= HELI_GROUND {
        b.pos.y = HELI_GROUND;
        b.vel.y = b.vel.y.max(0.0);
        b.vel.x *= 1.0 - 3.0 * dt;
        b.vel.z *= 1.0 - 3.0 * dt;
    } else if b.pos.y >= HELI_CEIL {
        b.pos.y = HELI_CEIL;
        b.vel.y = b.vel.y.min(0.0);
    }
    // Arena walls stop you in the air too (biomod containment field).
    if b.pos.x.abs() > ARENA {
        b.pos.x = b.pos.x.clamp(-ARENA, ARENA);
        b.vel.x *= -0.2;
    }
    if b.pos.z.abs() > ARENA {
        b.pos.z = b.pos.z.clamp(-ARENA, ARENA);
        b.vel.z *= -0.2;
    }
    // Buildings shove the hull only below their roofline — clearing the
    // downtown tower matters, so this can't reuse the trucks' flat
    // `building_push`.
    let (px, pz, nx, nz) = building_push_below(b.pos.x, b.pos.z, HELI_R, b.pos.y);
    if nx != 0.0 || nz != 0.0 {
        b.pos.x = px;
        b.pos.z = pz;
        // Kill the velocity component into the wall; keep the slide.
        let into = b.vel.x * nx + b.vel.z * nz;
        if into < 0.0 {
            b.vel.x -= into * nx;
            b.vel.z -= into * nz;
        }
    }
}

/// Shared kinematic-chunk error term: position + velocity + attitude
/// (quat dot → 0 error when aligned; ±q counts as aligned).
pub fn body_err(a: &Body, b: &Body) -> f32 {
    (a.pos.x - b.pos.x).abs()
        + (a.pos.y - b.pos.y).abs()
        + (a.pos.z - b.pos.z).abs()
        + (a.vel.x - b.vel.x).abs()
        + (a.vel.y - b.vel.y).abs()
        + (a.vel.z - b.vel.z).abs()
        + (1.0 - a.rot.dot(b.rot).abs()) * 8.0
}

/// Shared kinematic-chunk lerp: linear pos/vel, short-arc nlerp attitude.
pub fn body_lerp(a: &Body, b: &Body, t: f32) -> Body {
    let l = |x: f32, y: f32| x + (y - x) * t;
    Body {
        pos: vec3(l(a.pos.x, b.pos.x), l(a.pos.y, b.pos.y), l(a.pos.z, b.pos.z)),
        vel: vec3(l(a.vel.x, b.vel.x), l(a.vel.y, b.vel.y), l(a.vel.z, b.vel.z)),
        rot: Quat::nlerp(a.rot, b.rot, t),
    }
}

/// Heli prediction error metric — the pod IS a body.
pub fn heli_err(a: &Heli, b: &Heli) -> f32 {
    body_err(&a.body, &b.body)
}

/// Prediction error metric: the shared body term plus the scalars.
pub fn err_metric(a: &Truck, b: &Truck) -> f32 {
    body_err(&a.body, &b.body)
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
        body: body_lerp(&a.body, &b.body, t),
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
        y: l(a.y, b.y),
        z: l(a.z, b.z),
        heading: lerp_angle(a.heading, b.heading, t),
        pitch: lerp_angle(a.pitch, b.pitch, t),
        owner: b.owner, // identity, not a quantity — never blend it
    }
}

/// Interpolate two heli samples — the pod is a body, so the shared
/// body lerp (nlerp attitude) IS the heli lerp.
pub fn heli_lerp(a: &Heli, b: &Heli, t: f32) -> Heli {
    Heli {
        body: body_lerp(&a.body, &b.body, t),
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
    let h = t.heading();
    let (fx, fz) = (h.sin() * TRUCK_HL, h.cos() * TRUCK_HL);
    let (x, z) = (t.body.pos.x, t.body.pos.z);
    ((x - fx, z - fz), (x + fx, z + fz))
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

/// The truck hull's bullet height band (friendly-fire chip per hit is
/// `Params::friendly_dmg`).
pub const TRUCK_HULL_H: f32 = 1.6;

/// A vehicle's bullet-collision shape, decoupled from which pool the
/// vehicle lives in: a ground-plane capsule (equal endpoints = a
/// cylinder) plus an altitude band. Every "does a shot touch this
/// vehicle" question — the server's friendly-fire sweep, the bots'
/// hold-fire gate — goes through [`ray_hits_hull`]; a NEW VEHICLE adds
/// its `*_hull` fn here and one registry line per side (the server's
/// `hulls` list, `ClientWorld::hulls`), and the sweep code never
/// changes.
#[derive(Clone, Copy)]
pub struct Hull {
    /// Capsule segment endpoints on the ground plane.
    pub a: (f32, f32),
    pub b: (f32, f32),
    pub r: f32,
    /// Altitude band (lo, hi) a shot must be inside at the hit point.
    pub y: (f32, f32),
}

impl Hull {
    /// The hull padded by `m` on every surface — hold-fire gates use a
    /// grown hull so bots err toward not shooting a buddy.
    pub fn grow(self, m: f32) -> Hull {
        Hull {
            r: self.r + m,
            y: (self.y.0 - m, self.y.1 + m),
            ..self
        }
    }
}

pub fn truck_hull(t: &Truck) -> Hull {
    let (a, b) = truck_seg(t);
    Hull { a, b, r: TRUCK_R, y: (0.0, TRUCK_HULL_H) }
}

pub fn heli_hull(h: &Heli) -> Hull {
    let p = h.body.pos;
    Hull {
        a: (p.x, p.z),
        b: (p.x, p.z),
        r: HELI_R,
        y: (p.y - HELI_R, p.y + HELI_R),
    }
}

/// A hog is a ground cylinder: its round body over the gameplay hit
/// band [0, `HOG_H`] (taller than the drawn hog on purpose — see
/// `HOG_H`). The shooter's forgiveness is NOT in here: `Shot::pad`
/// grows the query, never the collider (docs/collisions.md §8).
pub fn hog_hull(h: &Hog) -> Hull {
    Hull {
        a: (h.x, h.z),
        b: (h.x, h.z),
        r: HOG_R,
        y: (0.0, HOG_H),
    }
}

/// The heli's stage-4 parts, posed from the body: `[0]` is the cabin
/// (the BODY part — bites test `ids[0]` by convention), then the tail
/// boom, then the rotor disc. Yaw poses the boom; pitch/roll are
/// ignored by hulls (altitude bands stay vertical — an approximation
/// the shot pads already forgive).
pub fn heli_hulls(h: &Heli) -> [(u8, Hull); 3] {
    let b = h.body;
    let (x, y, z) = (b.pos.x, b.pos.y, b.pos.z);
    let yaw = b.yaw();
    let (fx, fz) = (yaw.sin(), yaw.cos());
    [
        (
            PART_BODY,
            Hull {
                a: (x, z),
                b: (x, z),
                r: HELI_CABIN_R,
                y: (y - HELI_CABIN_R, y + HELI_CABIN_R),
            },
        ),
        (
            PART_TAIL,
            Hull {
                a: (x - fx * HELI_TAIL_A, z - fz * HELI_TAIL_A),
                b: (x - fx * HELI_TAIL_B, z - fz * HELI_TAIL_B),
                r: HELI_TAIL_R,
                y: (y - HELI_TAIL_R, y + HELI_TAIL_R),
            },
        ),
        (
            PART_ROTOR,
            Hull {
                a: (x, z),
                b: (x, z),
                r: HELI_ROTOR_R,
                y: (y + HELI_ROTOR_LO, y + HELI_ROTOR_HI),
            },
        ),
    ]
}

/// Where a gunner points to hit `(tx, ty, tz)` from its muzzle:
/// `(heading, climb)`. No lead — hogs shoot at where you ARE, which
/// is most of why they're bad at it (`HOGGUN_SPREAD` is the rest).
pub fn hog_aim(x: f32, y: f32, z: f32, tx: f32, ty: f32, tz: f32) -> (f32, f32) {
    let (dx, dy, dz) = (tx - x, ty - y, tz - z);
    (dx.atan2(dz), dy.atan2((dx * dx + dz * dz).sqrt()))
}

/// A shot's travel — `reach` along `heading` on the ground plane, `dy`
/// total altitude change over it — against one hull, in PRESENT time
/// (vehicles aren't in the history ring; they're slow enough that
/// rewind buys little). SAMPLED, not solved: the step size rides the
/// hull radius (≤ 80% of it), so nothing tunnels whether `reach` is a
/// bullet's per-tick travel or a bot's whole line of fire. Returns the
/// hit point.
pub fn ray_hits_hull(
    x: f32,
    z: f32,
    y: f32,
    heading: f32,
    reach: f32,
    dy: f32,
    hull: &Hull,
) -> Option<(f32, f32)> {
    let (sx, sz) = (heading.sin(), heading.cos());
    let n = (reach / (hull.r * 0.8).max(0.05)).ceil().max(1.0) as usize;
    for i in 0..n {
        let frac = (i as f32 + 0.5) / n as f32;
        let (px, pz) = (x + sx * reach * frac, z + sz * reach * frac);
        let py = y + dy * frac;
        if (hull.y.0..=hull.y.1).contains(&py)
            && seg_point_dist(hull.a, hull.b, (px, pz)) < hull.r
        {
            return Some((px, pz));
        }
    }
    None
}

// --- the collider pool (docs/collisions.md) ----------------------------------

/// One collidable PART, registered into the server's collider pool by
/// its owner: detection is data the sweep iterates, never functions
/// that know a vehicle kind (docs/collisions.md §2). The entry is
/// keyed by the part's OWN id — a vehicle's parts are child entities
/// (`id_add` per part, the parent→child link lives in the server's
/// `parts` pool); a single-part swarm entity may be its own part,
/// keyed by its owner id, which makes its cleanup free. Owners
/// re-pose `hull` every tick (the heli-rotor-matrix habit applied to
/// shapes); the sweep never looks up a pose.
#[derive(Clone, Copy)]
pub struct Collider {
    /// The entity this part belongs to — response is ITS business.
    pub owner: Id,
    /// Owner-private part tag (`PART_*`); the sweep carries it through
    /// to the contact untouched and never interprets it.
    pub part: u8,
    /// Category bits — what this entry IS. Sweeps bring their own mask
    /// of what they TEST (the `MASK_SHOT` pattern, doc §9).
    pub cat: u8,
    /// World-space shape, pre-posed by the owner every tick.
    pub hull: Hull,
}

/// Part tags — meaningful only to the owner's own response code.
/// Everything single-part is a BODY; the heli is the first multi-part
/// owner (stage 4): cabin (its body), tail boom, rotor disc.
pub const PART_BODY: u8 = 0;
pub const PART_TAIL: u8 = 1;
pub const PART_ROTOR: u8 = 2;

/// Category bits. Add sparingly — a bit is a vocabulary word.
pub const CAT_VEHICLE: u8 = 1 << 0;
pub const CAT_HOG: u8 = 1 << 1;

/// A hit the sweep found: who was struck, where, and how far along the
/// travel — `frac` is what orders competing hits (nearest wins).
#[derive(Clone, Copy)]
pub struct SweepHit {
    pub owner: Id,
    pub part: u8,
    pub frac: f32,
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

/// THE collisions sweep: one shot's travel against every collider
/// entry matching `mask`, nearest hit along the path winning (not
/// registry order — a hog can shield a teammate). `skip` drops the
/// shooter's own vehicle (bullets are born at its muzzle); `pad`
/// grows each tested hull QUERY-side (doc §8: the pad is the shot's
/// forgiveness — a collider doesn't know who's shooting at it).
pub fn sweep_colliders(
    x: f32,
    z: f32,
    y: f32,
    heading: f32,
    reach: f32,
    dy: f32,
    pad: f32,
    mask: u8,
    skip: Option<Id>,
    colliders: &[(Id, Collider)],
) -> Option<SweepHit> {
    let mut best: Option<SweepHit> = None;
    for (_, c) in colliders {
        if c.cat & mask == 0 || skip == Some(c.owner) {
            continue;
        }
        let Some((hx, hz)) = ray_hits_hull(x, z, y, heading, reach, dy, &c.hull.grow(pad))
        else {
            continue;
        };
        let (dx, dz) = (hx - x, hz - z);
        let frac = (dx * dx + dz * dz).sqrt() / reach.max(1e-6);
        if best.is_none_or(|b| frac < b.frac) {
            best = Some(SweepHit {
                owner: c.owner,
                part: c.part,
                frac,
                x: hx,
                y: y + dy * frac,
                z: hz,
            });
        }
    }
    best
}

/// A detected touch — written by the sweep on a fresh id, drained the
/// SAME tick by the struck entity's response task (sweep at prio 31,
/// responses at 32): transient facts as pool entries, the
/// contact-points rule. The sweep applies nothing; whoever owns
/// `owner` owns every consequence (docs/collisions.md §2).
#[derive(Clone, Copy)]
pub struct Contact {
    pub owner: Id,
    pub part: u8,
    /// What touched — `KIND_*`.
    pub kind: u8,
    /// Acting peer (the shooter); 0 for NPC causes.
    pub source_peer: u8,
    /// World hit point. `y` matters: a rotor strike at altitude is not
    /// a ground splash.
    pub x: f32,
    pub y: f32,
    pub z: f32,
    /// The causer's travel direction — knockback reads it.
    pub heading: f32,
}

/// A bullet connected.
pub const KIND_BULLET: u8 = 0;
/// A hog rammed the owner.
pub const KIND_BITE: u8 = 1;

/// Whether a hull touches a vertical circle — the bite test: 2D
/// capsule-vs-circle plus altitude-band overlap ([ylo, yhi] is the
/// biter's reach; a heli above it is safe however close in plan view).
pub fn hull_hits_circle(hull: &Hull, cx: f32, cz: f32, r: f32, ylo: f32, yhi: f32) -> bool {
    hull.y.0 <= yhi && ylo <= hull.y.1 && seg_point_dist(hull.a, hull.b, (cx, cz)) < hull.r + r
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

/// Spawn slot for a peer: spread along the south wall, facing in
/// (identity rot = +z = north = into the arena).
pub fn spawn_truck(peer: u8) -> Truck {
    Truck {
        body: Body {
            pos: vec3((peer as f32 - 4.5) * 5.0, 0.0, -ARENA + 6.0),
            ..Body::default()
        },
        ..Truck::default()
    }
}

/// Helipad row behind the truck slots, skids down, facing in.
pub fn spawn_heli(peer: u8) -> Heli {
    Heli {
        body: Body {
            pos: vec3((peer as f32 - 4.5) * 5.0, HELI_GROUND, -ARENA + 2.5),
            ..Body::default()
        },
    }
}

// --- physics sanity ----------------------------------------------------------

/// The force model's invariants, pinned so a tuning pass can't silently
/// break them: grip actually bleeds lateral momentum, the FBW trim
/// actually hovers, tilt actually goes places (and not past the cap).
#[cfg(test)]
mod hull_tests {
    use super::*;
    use std::f32::consts::FRAC_PI_2;

    // Truck at the origin facing +z: capsule (0,∓0.8) r 0.9, band 0..1.6.
    // Shots travel +x (heading = π/2).

    #[test]
    fn sweep_hits_a_crossing_truck() {
        let t = Truck::default();
        let hit = ray_hits_hull(-5.0, 0.0, 1.0, FRAC_PI_2, 10.0, 0.0, &truck_hull(&t));
        assert!(hit.is_some(), "flat shot through the hull must connect");
    }

    #[test]
    fn altitude_band_rejects_overflight() {
        let t = Truck::default();
        let hit = ray_hits_hull(-5.0, 0.0, 5.0, FRAC_PI_2, 10.0, 0.0, &truck_hull(&t));
        assert!(hit.is_none(), "a shot 5u up overflies a 1.6u hull");
        let mut h = Heli::default();
        h.body.pos = vec3(0.0, 10.0, 0.0);
        let under = ray_hits_hull(-5.0, 0.0, 1.0, FRAC_PI_2, 10.0, 0.0, &heli_hull(&h));
        assert!(under.is_none(), "a flat shot passes under a heli at 10u");
        let level = ray_hits_hull(-5.0, 0.0, 10.0, FRAC_PI_2, 10.0, 0.0, &heli_hull(&h));
        assert!(level.is_some(), "a shot at its altitude hits it");
    }

    #[test]
    fn long_reach_cannot_tunnel() {
        // A bot's whole line of fire (GUN_RANGE), heli mid-way: the
        // sampling must scale with reach or this skips right over it.
        let mut h = Heli::default();
        h.body.pos = vec3(30.0, 8.0, 0.0);
        let pp = Params::from_specs();
        let dy = 8.0 / 30.0 * pp.gun_range; // climb that crosses its altitude there
        let hit = ray_hits_hull(0.0, 0.0, 0.0, FRAC_PI_2, pp.gun_range, dy, &heli_hull(&h));
        assert!(hit.is_some(), "45u sweep must still sample densely enough");
    }

    #[test]
    fn grow_pads_the_hold_fire_gate() {
        let t = Truck::default();
        let graze = |hull: &Hull| ray_hits_hull(-5.0, 2.0, 1.0, FRAC_PI_2, 10.0, 0.0, hull);
        assert!(graze(&truck_hull(&t)).is_none(), "1.2u off the capsule misses");
        assert!(
            graze(&truck_hull(&t).grow(0.5)).is_some(),
            "the grown gate holds fire on the same pass"
        );
    }

    /// Two trucks on the same firing line, entered far-first: the sweep
    /// must order by travel, honor the category mask, and drop the
    /// shooter's own vehicle.
    #[test]
    fn sweep_orders_masks_and_skips() {
        let near = Truck::default(); // origin
        let mut far = Truck::default();
        far.body.pos = vec3(6.0, 0.0, 0.0);
        let (nid, fid) = (Id::new(0, 0, 1), Id::new(0, 0, 2));
        let part = |owner, t: &Truck| Collider {
            owner,
            part: PART_BODY,
            cat: CAT_VEHICLE,
            hull: truck_hull(t),
        };
        let cols = vec![(fid, part(fid, &far)), (nid, part(nid, &near))];
        let shot = |mask, skip| {
            sweep_colliders(-5.0, 0.0, 1.0, FRAC_PI_2, 15.0, 0.0, 0.0, mask, skip, &cols)
        };
        let hit = shot(CAT_VEHICLE, None).expect("two hulls on the line");
        assert_eq!(hit.owner, nid, "nearest along the ray wins, not list order");
        assert!(hit.frac < 0.5, "the near truck sits in the first half");
        assert!(shot(0, None).is_none(), "an empty mask tests nothing");
        let hit = shot(CAT_VEHICLE, Some(nid)).expect("far truck still there");
        assert_eq!(hit.owner, fid, "the shooter's own vehicle is invisible");
    }

    /// The pad grows the QUERY, not the collider (doc §8) — and the hit
    /// altitude rides the climb.
    #[test]
    fn sweep_pad_is_query_side() {
        let t = Truck::default();
        let id = Id::new(0, 0, 1);
        let cols = vec![(
            id,
            Collider {
                owner: id,
                part: PART_BODY,
                cat: CAT_VEHICLE,
                hull: truck_hull(&t),
            },
        )];
        let graze = |pad| {
            sweep_colliders(-5.0, 2.0, 1.0, FRAC_PI_2, 10.0, 0.0, pad, CAT_VEHICLE, None, &cols)
        };
        assert!(graze(0.0).is_none(), "1.2u off the capsule misses unpadded");
        let hit = graze(0.5).expect("this shot's forgiveness connects it");
        assert!(
            (hit.y - 1.0).abs() < 1e-4,
            "flat shot reports its own altitude, got {}",
            hit.y
        );
    }

    /// A hog collider sweeps like everything else, and the pad grows
    /// radius AND altitude band (from a heli most near-misses are
    /// vertical) — both query-side.
    #[test]
    fn sweep_pads_hog_radius_and_band() {
        let h = Hog {
            hp: 1.0,
            ..Hog::default()
        };
        let id = Id::new(0, 0, 3);
        let cols = vec![(
            id,
            Collider {
                owner: id,
                part: PART_BODY,
                cat: CAT_HOG,
                hull: hog_hull(&h),
            },
        )];
        let shot = |y, pad| {
            sweep_colliders(-5.0, 0.0, y, FRAC_PI_2, 10.0, 0.0, pad, CAT_HOG, None, &cols)
        };
        assert!(shot(1.0, 0.0).is_some(), "flat shot through the body connects");
        assert!(shot(HOG_H + 0.5, 0.0).is_none(), "overflight misses the band");
        assert!(
            shot(HOG_H + 0.5, Params::from_specs().hit_pad_heli).is_some(),
            "the heli pad forgives a vertical near-miss"
        );
    }

    /// Stage-4 heli parts: cabin first (the bite convention), boom
    /// behind, rotor above — and part-level nearest-along-ray means
    /// the boom shields the cabin from astern and the disc catches
    /// overflying fire.
    #[test]
    fn heli_parts_pose_and_shield() {
        let mut h = Heli::default();
        h.body.pos = vec3(0.0, 10.0, 0.0); // facing +z (identity rot)
        let parts = heli_hulls(&h);
        assert_eq!(parts[0].0, PART_BODY, "ids[0] convention: cabin first");
        let (_, tail) = parts[1];
        assert!(
            tail.a.1 < 0.0 && tail.b.1 < tail.a.1,
            "boom extends behind a +z-facing heli"
        );
        let id = Id::new(0, 0, 7);
        let cols: Vec<(Id, Collider)> = parts
            .iter()
            .map(|&(part, hull)| {
                (id, Collider { owner: id, part, cat: CAT_VEHICLE, hull })
            })
            .collect();
        let flank = |y| {
            sweep_colliders(-6.0, 0.0, y, FRAC_PI_2, 12.0, 0.0, 0.0, CAT_VEHICLE, None, &cols)
                .map(|hit| hit.part)
        };
        assert_eq!(flank(10.0), Some(PART_BODY), "cabin-height shot hits the cabin");
        assert_eq!(
            flank(10.0 + HELI_CABIN_R + 0.05),
            Some(PART_ROTOR),
            "just over the cabin, into the disc"
        );
        let astern =
            sweep_colliders(0.0, -6.0, 10.0, 0.0, 12.0, 0.0, 0.0, CAT_VEHICLE, None, &cols)
                .map(|hit| hit.part);
        assert_eq!(astern, Some(PART_TAIL), "from astern the boom eats it first");
    }

    /// Gunner ballistics: the aim solution points up at an elevated
    /// target (the whole reason helis fear the deck).
    #[test]
    fn hog_aim_points_up_at_a_heli() {
        let (heading, pitch) = hog_aim(0.0, HOGGUN_Y, 0.0, 10.0, HOGGUN_Y + 10.0, 0.0);
        assert!((heading - FRAC_PI_2).abs() < 1e-4, "target due +x");
        assert!(
            (pitch - std::f32::consts::FRAC_PI_4).abs() < 1e-3,
            "10 up over 10 out = 45°, got {pitch}"
        );
    }

    /// The bite test: capsule-vs-circle in plan view, altitude band as
    /// the biter's reach — climb and the horde loses you.
    #[test]
    fn bite_touches_in_plan_and_band() {
        let t = Truck::default(); // capsule (0,∓0.8) r 0.9, band 0..1.6
        let leap = Params::from_specs().hog_leap;
        let touch = |z| hull_hits_circle(&truck_hull(&t), 0.0, z, HOG_R, 0.0, leap);
        assert!(touch(TRUCK_HL + TRUCK_R + HOG_R - 0.05), "nose contact bites");
        assert!(!touch(TRUCK_HL + TRUCK_R + HOG_R + 0.05), "clear of the capsule");
        let mut h = Heli::default();
        h.body.pos = vec3(0.0, 10.0, 0.0);
        assert!(
            !hull_hits_circle(&heli_hull(&h), 0.0, 0.0, HOG_R, 0.0, leap),
            "a heli at 10u is out of leaping reach however close in plan"
        );
        h.body.pos.y = 2.0;
        assert!(
            hull_hits_circle(&heli_hull(&h), 0.0, 0.0, HOG_R, 0.0, leap),
            "hovering low over the horde gets nipped"
        );
    }
}

#[cfg(test)]
mod params_tests {
    use super::*;

    /// Spec order IS field order (the count is already a compile error
    /// via `must_cast`; this pins the pairing — a reorder that matters
    /// changes some default and trips here).
    #[test]
    fn params_spec_matches_fields() {
        let p = Params::from_specs();
        let by_name = |n: &str| {
            PARAM_SPECS
                .iter()
                .find(|s| s.name == n)
                .unwrap_or_else(|| panic!("no spec named {n}"))
                .default
        };
        assert_eq!(p.wave_base, by_name("wave_base"));
        assert_eq!(p.wave_grow, by_name("wave_grow"));
        assert_eq!(p.gunner_frac, by_name("gunner_frac"));
        assert_eq!(p.friendly_dmg, by_name("friendly_dmg"));
        assert_eq!(p.gun_dmg, by_name("gun_dmg"));
        assert_eq!(p.bite_dmg, by_name("bite_dmg"));
        assert_eq!(p.hog_fast, by_name("hog_fast"));
        assert_eq!(p.net_kbps, by_name("net_kbps"));
        assert_eq!(p.ai_stride, by_name("ai_stride"));
        assert_eq!(p.hog_aggro, by_name("hog_aggro"));
        assert_eq!(p.hog_roam, by_name("hog_roam"));
        assert_eq!(p.hog_flee, by_name("hog_flee"));
        assert_eq!(p.bite_cd, by_name("bite_cd"));
        assert_eq!(p.bite_cost, by_name("bite_cost"));
        assert_eq!(p.kill_points, by_name("kill_points"));
        assert_eq!(p.death_cost, by_name("death_cost"));
        assert_eq!(p.knock, by_name("knock"));
        assert_eq!(p.gun_cd, by_name("gun_cd"));
        assert_eq!(p.gun_range, by_name("gun_range"));
        assert_eq!(p.bullet_speed, by_name("bullet_speed"));
        assert_eq!(p.hit_pad_truck, by_name("hit_pad_truck"));
        assert_eq!(p.hit_pad_heli, by_name("hit_pad_heli"));
        assert_eq!(p.hoggun_cd, by_name("hoggun_cd"));
        assert_eq!(p.hoggun_range, by_name("hoggun_range"));
        assert_eq!(p.hoggun_dmg, by_name("hoggun_dmg"));
        assert_eq!(p.heli_tail_kick, by_name("heli_tail_kick"));
        assert_eq!(p.hog_leap, by_name("hog_leap"));
        assert_eq!(p.vmax, by_name("vmax"));
        assert_eq!(p.boost_vmax, by_name("boost_vmax"));
        assert_eq!(p.truck_grip, by_name("truck_grip"));
        assert_eq!(p.truck_grip_boost, by_name("truck_grip_boost"));
        assert_eq!(p.heat_rate, by_name("heat_rate"));
        assert_eq!(p.heat_cool, by_name("heat_cool"));
        assert_eq!(p.heli_lift, by_name("heli_lift"));
        assert_eq!(p.heli_t_max, by_name("heli_t_max"));
        assert_eq!(p.heli_yaw, by_name("heli_yaw"));
    }

    #[test]
    fn params_file_roundtrip_clamps_and_survives_noise() {
        let path = std::env::temp_dir().join("hogs-params-test.params");
        let path = path.to_str().unwrap();

        // Hand-shaped file: comments, junk, unknown names, out-of-range
        // and trailing-metadata values.
        std::fs::write(
            path,
            "# tuned live\nwave_base=200 1..1000\ngunner_frac=9.5\nnot_a_param=3\nbad line\nhog_fast=oops\n",
        )
        .unwrap();
        let p = params_load(path);
        assert_eq!(p.wave_base, 200.0); // loaded (metadata ignored)
        assert_eq!(p.gunner_frac, 1.0); // clamped to range
        assert_eq!(p.hog_fast, 11.0); // unparseable: default kept
        assert_eq!(p.wave_grow, 15.0); // absent: default kept

        // Save → load is identity for in-range sets.
        params_save(path, &p).unwrap();
        assert_eq!(params_load(path), p);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn params_missing_file_is_the_shipped_defaults() {
        assert_eq!(params_load("does/not/exist.params"), Params::from_specs());
    }
}

#[cfg(test)]
mod physics_tests {
    use super::*;
    const DT: f32 = 1.0 / 60.0;

    /// The shipped tuning — steps read [`Params`] now, tests pin the
    /// invariants at the defaults.
    fn pp() -> Params {
        Params::from_specs()
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
        let mut h = spawn_heli(1);
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

    #[test]
    fn heli_full_tilt_cruises_fast_but_capped() {
        let mut h = spawn_heli(1);
        h.body.pos.y = 20.0;
        let cmd = Drive {
            pitch: 1.0,
            ..Default::default()
        };
        for _ in 0..600 {
            heli_step(&mut h, cmd, DT, &pp());
        }
        let hs = (h.body.vel.x * h.body.vel.x + h.body.vel.z * h.body.vel.z).sqrt();
        assert!(
            hs > 20.0 && hs <= HELI_VCAP + 0.1,
            "full nose-down should cruise 20..{HELI_VCAP} u/s, got {hs}"
        );
        assert!(
            (h.body.pos.y - 20.0).abs() < 2.0,
            "FBW trim should hold altitude through a full-tilt dash, y {}",
            h.body.pos.y
        );
    }
}
