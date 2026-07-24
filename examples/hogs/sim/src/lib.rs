//! THE DETERMINISM BOUNDARY (engine-v2 item 3, extracted 2026-07-22).
//!
//! Everything in this crate runs BYTE-IDENTICAL on the server and in
//! client prediction replay: the predicted pods ([`Truck`], [`Heli`]),
//! the command frame ([`Drive`]) and params ([`Params`]) that evolve
//! them, THE shared steps ([`truck_step`], [`heli_step`]), the geometry
//! both sides collide against ([`BUILDINGS`] and the push helpers), the
//! spawn seeds, and the muzzle solutions (one definition, so the
//! server's real bullet and the client's cosmetic tracer leave the same
//! barrel the same way).
//!
//! The crate boundary is the contract the old "same compiled math"
//! comments only asked for: no clock, no RNG, no I/O, no pool access —
//! the dependency list (pm math + the params macro) makes an impurity a
//! visible diff instead of a soak finding. `tests/golden.rs` is the
//! tripwire: scripted command streams replayed over the steps, hashed,
//! and pinned against [`SIM_VERSION`] — CI answers "did this refactor
//! change the physics" instead of a soak.
//!
//! Everything here is re-exported through hogs' `common.rs`, so game
//! code keeps reaching it as `common::*`.

use pm::{Body, Quat, vec3};

// Angle helpers come from the engine; re-exported so the whole example
// reaches them through `common::*` like the rest of the shared math.
pub use pm::{lerp_angle, wrap_angle};

/// Version of the shared-step math. The golden replay hashes in
/// `tests/golden.rs` are recorded against this: an INTENTIONAL physics
/// change bumps the version and re-records the hashes (the failing test
/// prints them); an unintentional change is a red test — which is the
/// entire point of the boundary.
pub const SIM_VERSION: u32 = 4; // v2: thruster heli; v3: truck terrain/RAMPS; v4: heli skids ride ramp surfaces (playtest 1)

/// Fixed simulation step on both sides (prediction replays it).
pub const FIXED_DT: f32 = 1.0 / 60.0;
/// Half-extent of the square arena (walls at +-ARENA on x and z).
/// Big: the horde needs room to flank and the trucks need room to run,
/// with buildings breaking up the sightlines.
pub const ARENA: f32 = 100.0;

// --- game params ----------------------------------------------------------
//
// The design record (2026-07-17). A param is a
// SERVER-OWNED TUNING SCALAR: a number a designer wants to move while
// the game runs, whose meaning survives the change. Everything else
// stays a Rust const:
//
// - STRUCTURAL constants — wire quantization scales, geometry tables
//   (`BUILDINGS`), part/category ids, colors, `ADDR`, `FIXED_DT`.
//   Changing these mid-run is meaningless or breaks contracts (a wire
//   scale is a handshake fact, not a feel knob).
// - CLIENT-COSMETIC knobs — day length, link sim. Live-tunable per
//   client via the telemetry node; per-client state, not shared truth.
// - CREATION-FROZEN config — DONE 2026-07-23: the interp delay is the
//   `interp_ms` param now (the redial loop rebuilds registrations, so
//   a new value takes effect per session; the server rewind reads it
//   live). The category is empty — it was only ever this one value.
// - SHARED-STEP constants (`vmax`, grips, heli thrust…) ARE params:
//   the steps take `&Params` and each end reads its copy.
//
// The flow (doctrine unchanged — clients send channels, the server
// replicates state):
//
//   hogs.params file ─(load, clamp)→ server Params single ─sync→ every client
//        ^                                 ^    └ reads: server tasks;
//        └─(save event: server rewrites)   |      shared steps via replica
//   pm-watch `set` → telemetry knobs ─diff→ ParamSet event (reliable)
//
// Why not env vars / CLI flags: no live path, no save path, a second
// source of truth next to the file. Why not TOML: the codec is 20
// lines and pm-control already defined the line shape — one format
// across the platform beats a second parser. Why not a client-owned
// file: the server is the authority; a remote client could neither
// seed wave 1 in time nor save the server's truth. The save file
// belongs to the process that owns the values — a dedicated server
// saves ITS file; an in-process session saves the one `main` loaded.
//
// TODO(roadmap): params stage 3, queued — startup echo of non-default
// params; range-corner invariant soaks; a host-only param gate the day
// public servers exist; a dedicated pm-mon panel (ranges,
// dirty-vs-file markers). Unscheduled ideas: autosave-with-debounce as
// an opt-in knob; per-map param files; a file header with a schema
// version if params ever need migrations.

pm_control_core::pm_params! {
    /// Server-owned tuning scalars (design record in the section
    /// comment above): seeded from the params file at startup,
    /// live-writable through the `"param.set"` event, and replicated to
    /// every client as the `"params"` synced single. Server tasks read
    /// these where the old consts used to be; client reads (bot gates,
    /// the cosmetic gun, the aim line) come off the replica
    /// (`ClientWorld::params`) — never a const. `Default` IS the
    /// shipped set — a replica is sane even before its first snapshot.
    ///
    /// One line per param — name (the field ident), default, live range,
    /// doc — and `pm_params!` generates everything else: the wire pod,
    /// `SPECS`, clamped indexed writes, save-set text, and the monitor
    /// knobs ([`ParamKnobs`], save button included). The last nine are
    /// SHARED-STEP constants: `truck_step`/`heli_step` read them, so the
    /// server passes its single and the predictors read the replica — a
    /// live change mispredicts only for the inputs in flight (one
    /// correction blip; soak-verified at lag=80/loss=3%).
    pub struct Params knobs ParamKnobs {
        /// First-wave horde size (was the `PM_HOGS` env knob).
        pub wave_base: 40.0 in 1.0..1000.0,
        /// Extra hogs per wave past the first.
        pub wave_grow: 15.0 in 0.0..200.0,
        /// Fraction of each wave that spawns with a shoulder gun.
        pub gunner_frac: 0.25 in 0.0..1.0,
        /// Friendly-fire chip per cannon hit (gentler than `gun_dmg` —
        /// punish spraying, don't two-shot a buddy).
        pub friendly_dmg: 0.25 in 0.0..1.0,
        /// Cannon damage per hit on a hog (hp scale is 1.0).
        pub gun_dmg: 0.5 in 0.01..1.0,
        /// Truck/heli chip per hog bite.
        pub bite_dmg: 0.25 in 0.0..1.0,
        /// Hog chase/flee speed, u/s (roam speed is `hog_roam`).
        pub hog_fast: 11.0 in 1.0..30.0,
        /// Per-peer snapshot bandwidth, kilobits/sec — feeds the
        /// engine's send tune (`PmServer::send_tune`): how far the
        /// multi-datagram flight extends past the always-sent first
        /// datagram (~64 = the classic one-datagram cadence).
        pub net_kbps: 2000.0 in 64.0..6000.0,
        /// Hog think cadence: each hog re-decides every Nth tick in
        /// slot-staggered cohorts; movement integrates every tick.
        pub ai_stride: 4.0 in 1.0..8.0,
        /// A vehicle inside this range gets charged.
        pub hog_aggro: 26.0 in 5.0..80.0,
        /// Roam speed, u/s (charge/flee speed is `hog_fast`).
        pub hog_roam: 4.5 in 0.5..15.0,
        /// After a bite the hog breaks off for this long (seconds).
        pub hog_flee: 1.5 in 0.0..6.0,
        /// Per-hog re-bite lockout (seconds) — debounces overlap flicker.
        pub bite_cd: 1.0 in 0.1..5.0,
        /// Points a bite costs the team.
        pub bite_cost: 15.0 in 0.0..100.0,
        /// Points a kill earns the team.
        pub kill_points: 10.0 in 0.0..100.0,
        /// Points an exploded/downed vehicle costs the team (on top of
        /// the bites that probably caused it).
        pub death_cost: 30.0 in 0.0..200.0,
        /// Bullet-hit knockback speed on a surviving hog (u/s; the decay
        /// rate stays the paired `KNOCK_DECAY` const).
        pub knock: 9.0 in 0.0..30.0,
        /// Turret refire period (seconds). The client's cosmetic gun
        /// reads the replica so the click-tracer cadence matches.
        pub gun_cd: 0.25 in 0.05..2.0,
        /// Truck turret slew rate, rad/s on both axes — the turret
        /// chases the commanded angles instead of snapping (tank
        /// feel: a flick runs ahead, the barrel catches up; the
        /// camera follows the BARREL, so it swings at this rate too).
        /// The heli gimbal stays crisp.
        pub turret_rate: 1.8 in 0.2..10.0,
        /// Bullet max travel (also the client aim line's reach).
        pub gun_range: 45.0 in 10.0..120.0,
        /// Bullet speed, u/s (also flies the client's cosmetic tracers
        /// and the bots' lead arithmetic).
        pub bullet_speed: 100.0 in 20.0..200.0,
        /// Friendly-fire hit-circle padding by victim platform:
        /// forgiveness for shots that would graze a teammate (heli >
        /// truck — the heli is the one you sweep past at speed).
        pub hit_pad_truck: 0.35 in 0.0..2.0,
        pub hit_pad_heli: 0.8 in 0.0..2.0,
        /// Gunner-hog refire period (seconds; each hog jitters ±35%).
        pub hoggun_cd: 1.6 in 0.2..6.0,
        /// Gunner-hog engagement range.
        pub hoggun_range: 28.0 in 5.0..80.0,
        /// Gunner-hog chip per hit (lighter than a teammate's cannon).
        pub hoggun_dmg: 0.12 in 0.0..1.0,
        /// Tail-boom hit: bullet impulse (mass·u/s) applied at the tail
        /// thruster mount along the shot line — `Body::impulse_at`
        /// turns it into the yaw swing (obliquity falls out of r × J).
        /// Default re-tuned 2026-07-23 for the unit change (was 0.5 rad
        /// of instant yaw): 1.5 peaks ~0.4 rad before the FBW wins.
        pub heli_tail_kick: 1.5 in 0.0..6.0,
        /// Hog reach ceiling: bites and aggro only reach a heli hovering
        /// below this altitude — climb and the horde loses you.
        pub hog_leap: 2.4 in 0.5..8.0,
        /// Fraction of each wave that spawns WINGED (the biomod program
        /// takes the fight upstairs — see the flyer section below).
        pub flyer_frac: 0.15 in 0.0..1.0,
        /// Flyer chase speed, u/s (roaming cruises at ~half of it).
        pub flyer_speed: 14.0 in 1.0..30.0,
        /// Flyer cruise altitude while roaming.
        pub flyer_alt: 9.0 in 2.0..30.0,
        /// Flyer chase ceiling: climb past it and the flock sheds — the
        /// band between here and the hard ceiling is the heli's refuge.
        pub flyer_ceil: 30.0 in 5.0..45.0,
        /// Truck top speed (forward), and boosted.
        pub vmax: 18.0 in 5.0..40.0,
        pub boost_vmax: 30.0 in 10.0..60.0,
        /// Tire grip: how fast LATERAL velocity bleeds (1/s exponential
        /// rate). This is the whole "physics" of the truck — steering
        /// turns the chassis, grip drags the momentum around after it.
        /// High = rails; low = ice. Boosting loosens the rear
        /// (powerslide).
        pub truck_grip: 8.0 in 0.5..20.0,
        pub truck_grip_boost: 3.2 in 0.5..20.0,
        /// Heat per second while boosting / cooling per second while
        /// not. STYLE(motorstorm): this pair IS the MotorStorm boost
        /// bar — ride the edge for speed, overcook it and the server
        /// explodes you.
        pub heat_rate: 0.4 in 0.05..2.0,
        pub heat_cool: 0.25 in 0.05..2.0,
        /// Collective authority above/below hover trim, and the total
        /// thrust ceiling.
        pub heli_lift: 16.0 in 4.0..40.0,
        pub heli_t_max: 34.0 in 10.0..80.0,
        /// Commanded yaw rate at full stick (rad/s) — the tail
        /// thruster's rate loop chases it.
        pub heli_yaw: 1.9 in 0.5..5.0,
        /// Remote interpolation delay, ms — ONE replicated number for
        /// the whole lag-comp contract: clients render remotes this far
        /// behind, the server rewinds exactly this far to judge their
        /// shots ([`interp_ticks`]). Formerly the INTERP_DELAY const +
        /// PM_INTERP_MS env folklore; 33 is the played-in default
        /// (2026-07-18, lag=80/loss=3%: "fixed nearly everything" vs
        /// 50). Try 200: soup, but shots land. Try 8: fresh but
        /// strobing under loss. A choice, not a law. Takes effect per
        /// session (interp buffers capture it at install).
        pub interp_ms: 33.0 in 0.0..200.0,
        /// Prediction-correction DEAD ZONE (2026-07-23, the spike-4
        /// answer): reconcile only rewinds/replays when the pod error
        /// (`pod_err`: |pos| + |vel| + rot×8 + |ang|, summed) exceeds
        /// this. The client's shared step is an APPROXIMATION of the
        /// server's Box3D truth — mm-scale drift at cruise is
        /// permanent and harmless, and correcting it every snapshot
        /// dragged the own-pose feel behind the stick (the input-lag
        /// complaint). Real events (ramps, shoves, tumbles, tail
        /// kicks) blow past this and still correct. 0 restores
        /// correct-on-everything.
        pub predict_tol: 0.5 in 0.0..5.0,
        // --- THE PARAMS SWEEP batch 1 (2026-07-23): everything below
        // was a `pub const` wearing tuning clothing; the const/param
        // split is FINAL (see common.rs) — consts are for enum
        // discriminants, determinism anchors (FIXED_DT/SIM_VERSION),
        // authored geometry (hull radii, BUILDINGS, LEVELS), and
        // invocation identity. Values are the played-in defaults.
        /// Steering filter time constant, s (bots lag the wheel more).
        pub steer_tau: 0.18 in 0.02..1.0,
        /// Turret azimuth stops, rad each side of forward.
        pub aim_max: 2.6 in 0.5..3.14,
        /// Truck turret elevation stops, rad (asymmetric on purpose:
        /// flyers overhead beat ditch-aiming).
        pub truck_aim_up: 0.9 in 0.1..1.5,
        pub truck_aim_down: 0.35 in 0.0..1.0,
        /// Heli fly-by-wire attitude P gain: cyclic tilt per radian of
        /// attitude error (rate damping is the `HELI_CYC_D` const).
        pub heli_att_k: 5.0 in 1.0..15.0,
        /// Commanded attitude limits, rad.
        pub heli_pitch_max: 0.70 in 0.1..1.2,
        pub heli_roll_max: 0.50 in 0.1..1.2,
        /// Chin-gun elevation gimbal, rad each way.
        pub heli_aim_pitch: 1.0 in 0.2..1.5,
        /// Horizontal / vertical drag while flying.
        pub heli_hdrag: 0.28 in 0.0..2.0,
        pub heli_vdrag: 1.6 in 0.0..4.0,
        /// Horizontal speed cap, u/s.
        pub heli_vcap: 34.0 in 5.0..80.0,
        /// Skid height when landed / hard altitude ceiling (the hog
        /// refuge band lives between `heli_ceil` and flyer shed).
        pub heli_ground: 0.6 in 0.1..2.0,
        pub heli_ceil: 45.0 in 10.0..120.0,
        /// Gravity, u/s² — arcade knob, not physics dogma.
        pub gravity: 9.81 in 1.0..30.0,
        /// Vehicle/NPC hit points (hp scale is 1.0 = one cannon hit
        /// per `gun_dmg` 0.5 ≈ two hits).
        pub truck_hp: 1.0 in 0.2..10.0,
        pub hog_hp: 1.0 in 0.1..10.0,
        pub flyer_hp: 0.5 in 0.1..5.0,
        pub depot_hp: 6.0 in 1.0..50.0,
        pub boss_hp: 30.0 in 5.0..200.0,
        /// Boss render/collider scale and its low-hp growl growth.
        pub boss_scale: 3.0 in 1.0..8.0,
        pub boss_grow: 1.4 in 1.0..3.0,
        /// Hog/flyer turn rates (rad/s) and the roam goal re-pick, s.
        pub hog_turn: 2.6 in 0.5..8.0,
        pub flyer_turn: 2.2 in 0.5..8.0,
        pub roam_repick: 9.0 in 1.0..30.0,
        /// Flyer climb rate (u/s), 3D aggro radius, and bite reach.
        pub flyer_climb: 7.0 in 1.0..20.0,
        pub flyer_aggro: 45.0 in 5.0..120.0,
        pub flyer_reach: 1.3 in 0.5..4.0,
        /// Gunner-hog shoulder gun: bullet travel (u), spread (rad),
        /// muzzle height (u).
        pub hoggun_travel: 32.0 in 5.0..100.0,
        pub hoggun_spread: 0.14 in 0.0..0.5,
        pub hoggun_y: 0.6 in 0.1..2.0,
        /// Mission brief hold, s; impact marker lifetime, s.
        pub brief_secs: 5.0 in 0.0..30.0,
        pub impact_ttl: 1.0 in 0.2..5.0,
        /// Reconnect window, s — peer id + parked vehicle survive this
        /// long (one clock: engine grace, roster parking, the client's
        /// give-up timer and token-file freshness).
        pub reconnect_grace: 20.0 in 0.0..120.0,
        /// Interp extrapolation cap, ms (rides loss bursts; was the
        /// PM_EXTRAP_MS env).
        pub extrap_ms: 50.0 in 0.0..200.0,
        /// Day-night cycle length, s (cosmetic sky time — shared so a
        /// squad sees one sky; was the `day=` arg + Tune single).
        pub day_secs: 480.0 in 10.0..3600.0,
    }
}

/// The interp delay in whole sim ticks — what the server subtracts from
/// a peer's acked tick to find the tick that peer was *seeing* (the
/// client hands the same param, in seconds, to `interp_pool`).
pub fn interp_ticks(p: &Params) -> u32 {
    (p.interp_ms / 1000.0 / FIXED_DT).round() as u32
}

/// Handshake schema identity for the params pod (`pm_params!` can't
/// emit this itself — pm-control-core stays engine-free): hash the
/// macro's generated field descriptor, so a client whose param LIST
/// drifted from the server's fails the connect with a named diff
/// instead of misreading the replica.
impl pm::PodSchema for Params {
    const SCHEMA_HASH: u64 = pm::schema_hash_str(Params::SCHEMA);
}

// --- the predicted pods ------------------------------------------------------

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
    /// turret swing. NOT `#[lerp(angle)]`: a clamped gimbal isn't
    /// cyclic — the barrel physically slews through ZERO between the
    /// stops, so remote interpolation must too (lerp_angle sent a
    /// −2.5→2.5 flick the short way through the BACK, through the
    /// stops; caught watching bot turrets, 2026-07-23).
    pub aim: f32,
    /// Turret ELEVATION off level, −p.truck_aim_down..p.truck_aim_up — the
    /// heli chin gun's law on the same mouse-y axis (added 2026-07-22:
    /// flat-only trucks had no answer to the flock). Evolved by
    /// `truck_step`; the barrel visibly elevates on every client.
    pub aim_pitch: f32,
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

/// Replicated helicopter state — the other player vehicle, and the
/// engine's first full-3D predicted pod. It is EXACTLY a [`pm::Body`]:
/// attitude lives in the quaternion (pitch/roll limits are enforced by
/// the step via yaw-pitch-roll extract/clamp/rebuild — a jet would skip
/// the extraction and integrate body rates on the quat directly).
/// Deliberately NOT quantized: predicted pools stay full precision so
/// reconcile error never sits at the quantization step. Flight model:
/// a COMBINED BODY — rotor thruster at the hub (cyclic tilts it) and a
/// tail thruster on the boom, every force propagated `r × F` through
/// `pm::Forces`, fly-by-wire on top — see `heli_step`. The body's
/// `ang` is live state here (the first vehicle to integrate it).
#[pm::pod]
pub struct Heli {
    pub body: Body,
    /// Chin-gun gimbal, relative to the airframe: azimuth off the nose
    /// (±p.aim_max — the truck turret's law) and elevation off level
    /// (±p.heli_aim_pitch). Evolved by `heli_step` from the command like
    /// every predicted field, so it replicates for free — remote
    /// players watch a heli's gun train around under a steady nose.
    /// NOT `#[lerp(angle)]` — clamped gimbals, not cyclic; the
    /// through-zero rule on `Truck::aim` applies to both axes here.
    pub aim: f32,
    pub aim_pitch: f32,
}

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
    pub aim: f32,    // turret angle relative to heading, +-p.aim_max (truck only)
    pub boost: f32,  // 0/1: burn heat for speed (truck only)
    pub bot: f32,    // 0/1: AI controller — its steering lags
    // Heli axes, dead weight in a truck. ONE continuous channel per
    // connection is the input doctrine, so the pod is the union of every
    // vehicle's axes and each step reads its own — the seam input-map
    // will eventually own (per-vehicle key contexts live client-side).
    // STYLE(unreal): the whole input model deliberately matches UE —
    // unreliable+redundant intent structs and resim (their CMC/Mover),
    // never reliable forces; input-map = Enhanced Input on this seam
    // (re-litigated against UE docs 2026-07-16 and confirmed).
    pub pitch: f32, // -1..1: nose down (forward) / up (heli only)
    pub lift: f32,  // -1..1: collective climb / descend (heli only)
    /// Gun elevation off level — the heli chin gun (±p.heli_aim_pitch)
    /// AND the truck turret (−p.truck_aim_down..p.truck_aim_up; each step
    /// clamps its own stops). Azimuth shares `aim` the same way. Same
    /// client-side hold/ease-back animation, streamed as absolute
    /// angles.
    pub aim_pitch: f32,
}

// --- tuning ----------------------------------------------------------------

// Tuning that designers move live migrated into [`Params`]. What
// remains const here is STRUCTURAL: geometry, physics identities, and
// control internals whose live mutation would be meaningless or break
// contracts (the param-vs-const taxonomy on the Params declaration).

/// Truck body radius — the shared step's building-push circle.
/// Prediction replays the step byte-exact on both ends, so this stays
/// a CONST, never model-derived; the bullet/bite capsule lives in the
/// truck model's `collide.body` box instead (models.rs).
pub const TRUCK_R: f32 = 0.9;

// --- helicopter tuning -------------------------------------------------------

/// Hull circle for the shared step's building pushes (prediction
/// replays it — stays a const, same rule as `TRUCK_R`). The stage-4
/// hitbox parts (cabin/tail/rotor) are `collide.*` boxes in the heli
/// model now (models.rs derives and poses them).
pub const HELI_R: f32 = 1.4;

// The heli as a COMBINED BODY (2026-07-23): two thrusters bolted to one
// rigid frame, every force propagated through `pm::Forces` (r × F —
// push the tail, the nose comes around). Mass properties and mount
// points are STRUCTURAL consts, not params: the steps replay them
// byte-exact on both ends (the `TRUCK_R` rule), and they must roughly
// match the authored model (models.rs boxes), which live tuning can't.

/// Mass 1.0 so thrust params keep reading as accelerations — the
/// numbers Connor tuned don't change meaning under the new model.
pub const HELI_MASS: f32 = 1.0;
/// Diagonal body-frame inertia (x pitch, y yaw, z roll): boom-heavy —
/// yaw carries the most metal, roll the least.
pub const HELI_INERTIA: pm::Vec3 = vec3(0.7, 1.4, 0.4);
/// Rotor hub, model-space offset from the CoM — the main thruster's
/// mount. Its height IS the cyclic's lever arm.
pub const HELI_ROTOR: pm::Vec3 = vec3(0.0, 1.3, 0.0);
/// Tail thruster on the boom (matches the model's `collide.tail` reach).
/// The 3.3 u arm is the yaw authority; the 0.6 u height couples a
/// little roll into hard yaws — combined-body flavor, FBW trims it.
pub const HELI_TAIL: pm::Vec3 = vec3(0.0, 0.6, -3.3);

/// Cyclic authority: how far the rotor thrust vector may tilt off the
/// mast, rad. Control internals (the taxonomy on `Params`), not feel
/// knobs — `heli_att_k` is the param that shapes response.
pub const HELI_CYCLIC: f32 = 0.5;
/// Cyclic rate damping (the D to `heli_att_k`'s P — sized near critical
/// for the hover-thrust torque at `HELI_INERTIA`).
pub const HELI_CYC_D: f32 = 1.1;
/// Tail thruster: rate-loop gain (force per rad/s of yaw-rate error)
/// and force cap.
pub const HELI_YAW_P: f32 = 3.0;
pub const HELI_TAIL_MAX: f32 = 3.0;
/// Rotational air drag, 1/s — spins decay even with the FBW saturated.
pub const HELI_ANG_DRAG: f32 = 0.6;

// --- buildings ---------------------------------------------------------------

/// Static obstacles as `(center x, center z, half w, half d, height)`.
/// Shared const data compiled into BOTH binaries — server and clients
/// collide against the same walls, so nothing about them replicates
/// (height is render-only). The south strip (z < -85) stays clear: that's
/// where trucks spawn.
///
/// TODO(roadmap): buildings stop being a const — a synced world pool
/// threaded into the shared steps params-style (a shared-step input
/// must be replicated state; `&Params` proved the seam, and a collapse
/// mispredicts for one blip, the documented params contract). The
/// prerequisite for ANY destructibility, engine choice independent.
///
/// TODO(roadmap): BOX3D FFI — GREENLIT 2026-07-23 (was "only if
/// demanded"; Connor pulled the trigger for the initial release after
/// the four qualifiers below). The 2026-07-20 doctrine stands: FFI the
/// real thing (`cc` + bindings, PIN a commit, one world owned by a
/// server task, pods in / poses out) — NEVER rewrite the solver.
/// Vehicle FEEL stays pm code (our steps become force generators
/// driving Box3D bodies); the netcode pillar stays (self-prediction is
/// spike 4's question — candidate (a) predict-self-vs-static-world,
/// candidate (b) rollback resim if state save/restore is cheap,
/// fallback (c) pm steps + Box3D contact as server shoves through the
/// bite-scrub seam). We keep OUR lag-comp history ring, filled with
/// Box3D poses.
///
/// STYLE(connor-qualifiers-2026-07-23): the release bar that fired the
/// trigger — treat these as feel targets on par with STYLE(motorstorm):
///   1. tank-v-tank = ram-as-combat-verb AND sustained wrestling
///      (door-to-door grinding, pinning) — MotorStorm had both;
///   2. horde = MASS YOU CAN FEEL: hogs shove each other and bog the
///      truck down, a charging wall has momentum;
///   3. vehicles FULLY TUMBLE: flips/rolls/roof-landings are real
///      states with recovery gameplay;
///   4. world goes MULTI-LEVEL: bridges, overpasses, drive-unders —
///      the answer that structurally retires capsule+band collision.
///
/// The spikes, in order, each a kill/reshape gate:
/// (1) FFI skeleton — DONE 2026-07-23: crates/box3d-sys, vendored pin,
///     pmb3 shim (no bindgen/libclang, b3* never reaches Rust),
///     50-box pile settles + island-sleeps, two runs BIT-IDENTICAL,
///     windows-gnu cross-compiles.
/// (2) horde scale — DONE 2026-07-23 (tests/horde.rs): 300 wandering
///     colliding hog capsules = 254 µs/step release (1.5% of the 60 Hz
///     tick), settled horde = 0 µs and 300/300 asleep; bog-down proof
///     = force-driven truck 10.0 m/s open → 5.7 m/s through 200 packed
///     hogs with 45 shoved aside — qualifier 2 is PHYSICS, zero
///     gameplay code. THE HOG IDIOM: capsule + FULL angular lock (yaw
///     is collision-irrelevant on a y-symmetric shape, heading stays
///     pod data) — upright-only locks left hogs spinning forever
///     (no torsional friction) and blocked island sleep.
/// (3) feel parity — DONE 2026-07-23 (tests/truck_feel.rs): the pm
///     drive laws re-expressed as forces on a free solver body pass
///     the pm invariants (grip bleeds a 10 m/s shove in 1 s, top
///     speed at the accel/drag equilibrium, boost powerslides) — the
///     feel survives the solver. FULL TUMBLE proven: a boosted kicker
///     launch flips the truck onto its ROOF, at rest, a real state.
///     THE WHEELS-DOWN GATE is doctrine: engine/grip/steer forces
///     exist only while body-up·world-up > 0.5 — a roof-landed truck
///     is a wreck until recovered (the qualifier-3 moment). Solver
///     contact friction combines √(μa·μb): keep vehicle μ tiny, OUR
///     grip forces are the tire model. Multi-level sanity passed
///     (drove under a slab with a crate asleep on it). b3WheelJoint
///     audition drives + absorbs a curb (spin sign: +x spin rolls −z)
///     — real suspension exists when MotorStorm chatter gets built.
/// SLICE 2, THE REPLICATED-BODY FORMAT — LANDED 2026-07-23 (Connor's
///     qualifier answers: full pose always / one format for
///     everything / ~150 scale / spend-now-tighten-later): Hog and
///     Flyer pods carry the full pm::Body (pos/vel/quat/ang) at FULL
///     precision — quantized wire_pool repr and heading/speed fields
///     dissolved (they're derivations now; steering scalars live in
///     the server-local brains). One format, one interp path; what
///     the solver knows, clients see — a future hog knockdown is
///     "unlock + let readback carry the quat", zero wire changes.
///     Bots lead targets with REAL velocity off the wire. Bandwidth
///     diet = dirty-tracking + solver sleep; if the horde starves the
///     flight under load, net_kbps is the live knob.
/// INTEGRATION SLICE 1 — LANDED 2026-07-23 (Connor: "just send it"):
///     hogs/src/phys.rs owns THE server physical world. Statics =
///     ground/walls/BUILDINGS/RAMP wedge hulls; ground hogs = dynamic
///     capsules (AI writes velocities, solver integrates — the hand
///     integration, wall clamp, and hog building_push are DELETED
///     from hog_ai); trucks = dynamic boxes driven by the spike-3
///     force laws (truck_step still evolves turret/heat on a scratch
///     copy; the body is the solver's); helis = kinematic mirrors of
///     the pm-stepped pod. Hog pod grew `y` (they climb ramps now).
///     Task order: phys (26) = membership diff → step → readback,
///     then hog_ai (28) / drive (30) write next tick's intents.
///     Soaked 45 s headless: 117-hog wave stable, kills flowing, no
///     panics. NOT play-verified.
/// (4) prediction seam — NOW LIVE as an approximation experiment:
///     clients predict with the shared steps against ground_probe
///     while the server's truth is the solver. Judge at lag=80/
///     loss=3% in-chair; if corrections blip too hard, promote to
///     (a) client-side self world or (b) rollback resim (recon:
///     b3RecPlayer keyframe ring + hash-verified replay is candidate
///     (b) machinery).
/// Known costs beyond physics: hog navigation on multi-level
/// (navmesh-shaped, Box3D doesn't touch it), level authoring (const
/// tables → glb levels through the models pipeline), Box3D is
/// weeks-old ALPHA (Catto pedigree, but wrap it behind our own seam).
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
/// TODO(box3d-move): hand footprint test — becomes a statics query on
/// the client's local solver world (master note atop hogs phys.rs).
pub fn in_building(x: f32, z: f32, pad: f32) -> bool {
    BUILDINGS
        .iter()
        .any(|&(bx, bz, hw, hd, _)| (x - bx).abs() < hw + pad && (z - bz).abs() < hd + pad)
}

/// Push a circle at `(x, z)` radius `r` out of every building it
/// overlaps. Returns the corrected position and the last push normal
/// (zero if nothing touched) — callers use the normal to scrub speed
/// (trucks) or slide the heading along the wall (hogs). Ground level is
/// altitude 0: no roof sits below it, so the roofline skip never fires
/// and this IS `building_push_below` at y = 0 — same compiled math, so
/// prediction byte-exactness survives (the golden replays pin it).
///
/// TODO(box3d-move): hand closest-point push-out (this fn and
/// `building_push_below`) — dies when the predicted step runs on the
/// client's local solver and contacts do this (master note atop hogs
/// phys.rs).
pub fn building_push(x: f32, z: f32, r: f32) -> (f32, f32, f32, f32) {
    building_push_below(x, z, r, 0.0)
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

// --- ramps -------------------------------------------------------------------

/// Jump ramps as `(center x, center z, yaw, half w, half l, height)`:
/// a wedge rotated by `yaw` — local +z runs UP the slope, entry lip at
/// local z = −hl (ground) rising to `height` at the top edge. Shared
/// const geometry like [`BUILDINGS`] (and the same destructibility
/// TODO applies): both sides drive the same terrain, nothing
/// replicates. Placed in open lanes clear of the building field; the
/// south spawn strip stays flat.
pub const RAMPS: [(f32, f32, f32, f32, f32, f32); 4] = [
    (-45.0, 40.0, 1.9, 4.0, 7.0, 3.0),
    (30.0, -8.0, -0.7, 4.0, 7.0, 3.0),
    (0.0, 62.0, 3.6, 4.5, 8.0, 3.4),
    (66.0, -55.0, 0.0, 4.0, 7.0, 2.8),
];

/// The ground surface under `(x, z)`: height and unit normal — the
/// flat arena with ramp wedges rising out of it. THE terrain query the
/// truck step drives on (shared, so ramp launches predict byte-exact).
/// Overlapping ramps resolve tallest-wins, like `building_top`.
///
/// TODO(box3d-move) (Connor, 2026-07-23, DECIDED — master note atop
/// hogs phys.rs): dies with client local solving. The statics world
/// already holds these SAME wedges as real hulls and the server's
/// bullets and hogs judge against those (the old "bullets and hog
/// walkers ignore ramps" TODO(ship) died with collisions slice 2);
/// only client prediction keeps this fn alive — the shared truck step
/// runs on clients, which have no solver world YET. Until the local
/// solver lands this stays byte-shared math — don't fork it, and don't
/// let the RAMPS/BUILDINGS tables drift from the bodies phys.rs builds
/// from them.
pub fn ground_probe(x: f32, z: f32) -> (f32, pm::Vec3) {
    let (mut h, mut n) = (0.0f32, vec3(0.0, 1.0, 0.0));
    for &(rx, rz, yaw, hw, hl, rh) in &RAMPS {
        let (s, c) = yaw.sin_cos();
        // World → ramp-local (the transpose of the heading basis:
        // local +z maps to world (sin, cos)).
        let (dx, dz) = (x - rx, z - rz);
        let (lx, lz) = (dx * c - dz * s, dx * s + dz * c);
        if lx.abs() > hw || lz.abs() > hl {
            continue;
        }
        let slope = rh / (2.0 * hl);
        let hh = (lz + hl) * slope;
        if hh > h {
            h = hh;
            // Local surface normal (0, 1, −slope) normalized, carried
            // back to world by the same yaw.
            let inv = 1.0 / (1.0 + slope * slope).sqrt();
            let nz = -slope * inv;
            n = vec3(nz * s, inv, nz * c);
        }
    }
    (h, n)
}

/// Just the height — arena floor 0, wedge surface on a ramp.
pub fn ground_height(x: f32, z: f32) -> f32 {
    ground_probe(x, z).0
}

/// Terrain-follow limits for the truck's tires: a rise steeper than
/// `CLIMB_STEP` per tick is a WALL (the back face of a ramp), a drop
/// deeper than `FOLLOW_DROP` breaks contact (you're flying).
pub const CLIMB_STEP: f32 = 0.5;
pub const FOLLOW_DROP: f32 = 0.45;

/// Roof height at `(x, z)`: the tallest building whose footprint covers
/// the point, 0.0 in the open — the bullets' altitude gate for walls.
///
/// TODO(box3d-move): no server users left (bullets ray-cast the solver
/// since collisions slice 2) — only the client tracer/reticle marchers
/// read this; becomes a statics query on the client's local solver
/// world (master note atop hogs phys.rs).
pub fn building_top(x: f32, z: f32) -> f32 {
    BUILDINGS
        .iter()
        .filter(|&&(bx, bz, hw, hd, _)| (x - bx).abs() < hw && (z - bz).abs() < hd)
        .map(|&(_, _, _, _, h)| h)
        .fold(0.0, f32::max)
}

// --- muzzles -----------------------------------------------------------------

/// Muzzle pose, `(x, y, z, heading, climb)` — ONE definition so the
/// server's real bullet and the client's cosmetic tracer (spawned at
/// the click from PREDICTED pose) leave the same barrel the same way.
/// Turret muzzle at the barrel tip, elevated by the turret pitch: the
/// barrel pivots at (0, 1.45, 0) in truck space (where the mesh's
/// barrel box starts), so the tip rises along the aimed line. The
/// turret is STABILIZED: the fire line is heading + aim regardless of
/// chassis pitch on a ramp or in the air (tank stabilization — and the
/// barrel draw uses the same flat frame, so what you see is the line
/// you shoot); only the pivot height rides `pos.y`.
pub fn truck_muzzle(t: &Truck) -> (f32, f32, f32, f32, f32) {
    let dir = t.heading() + t.aim;
    let p = t.aim_pitch;
    let (x, z) = (t.body.pos.x, t.body.pos.z);
    (
        x + dir.sin() * 1.9 * p.cos(),
        t.body.pos.y + 1.45 + 1.9 * p.sin(),
        z + dir.cos() * 1.9 * p.cos(),
        dir,
        p,
    )
}

/// Heli chin gun fires where the GIMBAL points: azimuth trains off the
/// nose (`Heli::aim`), elevation off level tilted by the airframe
/// (body pitch>0 = nose down, so `climb = aim_pitch - pitch` — a dive
/// still steepens a centered gun, and the gimbal corrects on top). The
/// muzzle leads along the GUN azimuth at chin radius, so tracers leave
/// the barrel whichever way it's slewed.
pub fn heli_muzzle(h: &Heli) -> (f32, f32, f32, f32, f32) {
    let b = h.body;
    let (yaw, pitch, _) = b.rot.to_yaw_pitch_roll();
    let dir = wrap_angle(yaw + h.aim);
    (
        b.pos.x + dir.sin() * 1.6,
        (b.pos.y - 0.35).max(0.2),
        b.pos.z + dir.cos() * 1.6,
        dir,
        h.aim_pitch - pitch,
    )
}

// --- THE truck step ----------------------------------------------------------

/// THE step — force-based ground vehicle: bot steering lags (first-order
/// filter, so the near future is a real prediction), humans steer crisp.
/// Steering turns the CHASSIS; the momentum vector follows through tire
/// grip (lateral velocity decays at `TRUCK_GRIP`), so hard corners at
/// speed carry sideways momentum, boost loosens into a powerslide, and a
/// server shove (bite scrub, knockback) is real momentum the tires then
/// grip out — friction, not scripting.
///
/// STYLE(motorstorm): the truck's feel target is MotorStorm (Connor's
/// call, 2026-07-23) — heavyweight off-road arcade: momentum first,
/// the chassis always talking (lean, dive, big stupid air), boost as
/// a risk economy you cash in or explode on, and landings that
/// matter. Already convergent: heat 1.0 = explosion IS MotorStorm's
/// boost bar, grip-loosens-under-boost is the powerslide, ramps +
/// downhill pull landed 2026-07-23. Still owed to the feel: weight
/// transfer (accel pitches the nose up, braking dives it, cornering
/// rolls — scripted attitude on top of the terrain ease, zero wire),
/// mid-air attitude control on the truck-dead `Drive.pitch` axis with
/// landing quality (flat landing keeps speed, nose-first scrubs it),
/// surface types out of `ground_probe` (mud lanes), and — the big
/// rock — vehicle-vehicle ramming, which is the day bodies trade
/// momentum and masses become real ratios (see `HELI_MASS`).
///
/// TERRAIN (2026-07-23): the ground constraint generalized from the
/// plane y = 0 to [`ground_probe`] — the truck DRIVES the surface
/// (tires follow height, gravity's in-plane component pulls the
/// momentum downhill, the chassis eases onto the slope) until the
/// ground falls away faster than [`FOLLOW_DROP`], and then it FLIES:
/// ballistic arc, no tire authority, nose easing along the arc, until
/// touchdown. Run a ramp fast and the launch pop is just the climb
/// rate the slope was already imparting — nothing scripted. The truck
/// stays a CONSTRAINED body (attitude scripted from terrain/arc, `ang`
/// projected to zero like `pos.y` used to be); the combined-body
/// force path is for vehicles whose attitude is theirs to lose.
pub fn truck_step(t: &mut Truck, cmd: Drive, dt: f32, p: &Params) {
    // COMPILE-TIME COVERAGE: an exhaustive destructure (no `..`), so
    // adding a Truck field refuses to compile until it's named here —
    // and the rule this line sends you here to obey is: every field in
    // the predicted pod must be EVOLVED BY THIS FUNCTION from the
    // command. If the server writes it outside this step (damage,
    // pickups), it does NOT belong in Truck — give it its own
    // authoritative pool (that's why hp lives in `Health`). Lerp/err
    // are GENERATED off the pod (tag angular fields #[lerp(angle)]) —
    // the destructure covers the step, the derive covers the rest.
    let Truck {
        body: _,
        steer: _,
        aim: _,
        aim_pitch: _,
        heat: _,
    } = *t;
    let mut heading = t.heading();
    let speed = t.speed();

    if cmd.bot > 0.5 {
        let k = 1.0 - (-dt / p.steer_tau).exp();
        t.steer += (cmd.turn - t.steer) * k;
    } else {
        t.steer = cmd.turn;
    }
    // Turret: SLEWS toward the commanded angles at `turret_rate` (both
    // axes) instead of snapping — the command is where you want the
    // gun, the pod is where the barrel actually is, and replaying
    // commands reproduces the chase exactly. No wrap handling needed:
    // the stops mean the short way is always through zero.
    let slew = p.turret_rate * dt;
    let want = cmd.aim.clamp(-p.aim_max, p.aim_max);
    t.aim += (want - t.aim).clamp(-slew, slew);
    let want = cmd.aim_pitch.clamp(-p.truck_aim_down, p.truck_aim_up);
    t.aim_pitch += (want - t.aim_pitch).clamp(-slew, slew);

    // Where the tires stand decides the tick: DRIVING or FLYING.
    let (gh, n) = ground_probe(t.body.pos.x, t.body.pos.z);
    let grounded = t.body.pos.y <= gh + 0.05;

    // Boost: extra shove and a higher ceiling, paid in heat — wheels
    // need ground to push (held boost midair just cools). Heat is
    // predicted state (this is THE shared step), so the client's meter
    // is live; the EXPLOSION at 1.0 is the server's move alone.
    let boosting = grounded && cmd.boost > 0.5 && cmd.thrust > 0.0 && t.heat < 1.0;
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

    let (mut vx, mut vz) = (t.body.vel.x, t.body.vel.z);
    let mut vy = t.body.vel.y;
    let (mut x, mut z) = (t.body.pos.x, t.body.pos.z);
    let mut y = t.body.pos.y;
    let (_, pitch0, roll0) = t.body.rot.to_yaw_pitch_roll();
    let (pitch, roll);

    if grounded {
        // Steering turns the chassis (front-wheel authority still
        // scales with forward speed) — the momentum vector is caught
        // up below.
        let authority = (speed.abs() / 6.0).min(1.0);
        heading = wrap_angle(heading + t.steer * 2.2 * authority * dt * speed.signum());
        // Decompose the world-frame momentum against the NEW chassis
        // axes: engine force + rolling drag act along forward, tire
        // grip bleeds whatever is left pointing out the doors.
        let (fx, fz) = (heading.sin(), heading.cos());
        let (rx, rz) = (heading.cos(), -heading.sin());
        let vf = ((vx * fx + vz * fz) + cmd.thrust * accel * dt) * (1.0 - 1.2 * dt);
        let vf = vf.clamp(-7.0, vmax);
        let grip = if boosting { p.truck_grip_boost } else { p.truck_grip };
        let vl = (vx * rx + vz * rz) * (-grip * dt).exp();
        vx = fx * vf + rx * vl;
        vz = fz * vf + rz * vl;
        // Slopes are real: gravity's in-plane component pulls the
        // momentum downhill (exactly zero on flat ground) — park on a
        // ramp and you roll back off it.
        vx += p.gravity * n.y * n.x * dt;
        vz += p.gravity * n.y * n.z * dt;
        x += vx * dt;
        z += vz * dt;
    } else {
        // FLYING: gravity owns the arc, the tires own nothing —
        // heading holds, momentum carries.
        vy -= p.gravity * dt;
        x += vx * dt;
        z += vz * dt;
        y += vy * dt;
    }

    // TODO(box3d-move): hand arena-wall clamp — the solver world has
    // real walls; dies when this step runs on the client's local
    // solver (master note atop hogs phys.rs).
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
    // Airborne too, at full height: no ramp jump clears the shortest
    // roof by enough to land on it, so the flat push keeps every
    // trajectory out of the interiors.
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

    // Terrain resolution at the new footprint.
    let (gh1, n1) = ground_probe(x, z);
    if grounded {
        if gh1 - y > CLIMB_STEP {
            // The back face of a ramp is a wall, not a climb: stay
            // put, bounce a little of the speed back.
            x = t.body.pos.x;
            z = t.body.pos.z;
            vx *= -0.1;
            vz *= -0.1;
            vy = 0.0;
        } else if y - gh1 <= FOLLOW_DROP {
            // Tires follow the surface; the implied vertical rate IS
            // the launch pop when the ground later falls away.
            vy = (gh1 - y) / dt;
            y = gh1;
        }
        // else: drove off an edge — y and the climb rate carry into
        // next tick's flying branch.

        // Chassis eases onto the slope under the heading (fast — the
        // suspension settling), rebuilt with the authoritative yaw.
        let (gx, gz) = (-n1.x / n1.y, -n1.z / n1.y); // surface gradient
        let (fx, fz) = (heading.sin(), heading.cos());
        let (rx, rz) = (heading.cos(), -heading.sin());
        let k = 1.0 - (-10.0 * dt).exp();
        pitch = pitch0 + (-(fx * gx + fz * gz).atan() - pitch0) * k;
        roll = roll0 + ((rx * gx + rz * gz).atan() - roll0) * k;
    } else {
        if y <= gh1 {
            // Touchdown: the tires catch, vertical momentum is spent,
            // grip takes it from here next tick.
            y = gh1;
            vy = 0.0;
        }
        // Nose eases along the flight arc (half of it — trucks are
        // not darts), wings level out.
        let hs = (vx * vx + vz * vz).sqrt();
        let k = 1.0 - (-3.0 * dt).exp();
        let pitch_t = if hs > 1.0 { ((-vy).atan2(hs) * 0.5).clamp(-0.55, 0.55) } else { pitch0 };
        pitch = pitch0 + (pitch_t - pitch0) * k;
        roll = roll0 + (0.0 - roll0) * k;
    }

    // Project back into the shared body under the tire constraints:
    // attitude is SCRIPTED (terrain/arc), so ang projects to zero the
    // way pos.y used to — which also keeps `integrate`'s rotation half
    // byte-inert for trucks.
    t.body.pos = vec3(x, y, z);
    t.body.rot = Quat::from_yaw_pitch_roll(heading, pitch, roll).norm();
    t.body.vel = vec3(vx, vy, vz);
    t.body.ang = pm::Vec3::ZERO;
}

// --- THE heli step -----------------------------------------------------------

/// THE heli step — same contract as `truck_step`: shared by the server
/// and client prediction, so flying is byte-exact under replay.
///
/// COMBINED-BODY flight model (2026-07-23, replacing the servo-attitude
/// version): the airframe is one rigid body with two THRUSTERS bolted
/// to it, and every newton lands through [`pm::Forces`] — linearly
/// through the center of mass and as `r × F` torque about it.
///
/// - **Main rotor** at [`HELI_ROTOR`]: collective sets the magnitude,
///   cyclic TILTS the thrust vector inside the disc (±[`HELI_CYCLIC`]).
///   The tilted component at the hub's lever arm is what pitches and
///   rolls the machine — the same force that torques you over then
///   vectors you forward once you're tipped, like the real article.
/// - **Tail thruster** at [`HELI_TAIL`]: sideways force on the boom;
///   3.3 u of lever arm turns it into yaw (plus a touch of roll from
///   its height, plus real tail drift while it burns).
///
/// Fly-by-wire keeps the STICK SEMANTICS of the old model: pitch/turn
/// command attitude and yaw RATE, a PD loop (`heli_att_k` P, const D)
/// works the cyclic, a rate loop works the tail, and the collective
/// trims thrust to hover at centered stick (trim = G / axis.y — tip
/// over and climb authority pays for it). The difference is that
/// disturbances are now real: a tail hit is an angular impulse the FBW
/// visibly fights back to level. Skids catch the ground, buildings
/// shove the hull only below their roofline.
pub fn heli_step(h: &mut Heli, cmd: Drive, dt: f32, p: &Params) {
    // COMPILE-TIME COVERAGE — the predicted-pod contract, same as
    // truck_step: every field here is evolved from the command by THIS
    // function. Lerp/err are generated off the pod — tag angular
    // fields #[lerp(angle)] and the derive does the rest.
    let Heli {
        body: _,
        aim: _,
        aim_pitch: _,
    } = *h;
    // Chin gun: crisp copy of the commanded gimbal, the truck turret's
    // law — the client animates the hold and the ease-back, so replay
    // reproduces it exactly.
    h.aim = cmd.aim.clamp(-p.aim_max, p.aim_max);
    h.aim_pitch = cmd.aim_pitch.clamp(-p.heli_aim_pitch, p.heli_aim_pitch);
    let b = &mut h.body;

    // FBW sensors: attitude (yaw unused — yaw is the tail loop's, and
    // it reads a RATE) and body-frame rates (x pitch, y yaw, z roll).
    let (_, pitch, roll) = b.rot.to_yaw_pitch_roll();
    let w = b.rot.conj().rotate(b.ang);

    // Cyclic: PD from commanded attitude to a rotor-disc tilt. Yaw
    // input still banks the roll, so turns stay coordinated.
    let pitch_cmd = cmd.pitch.clamp(-1.0, 1.0) * p.heli_pitch_max;
    let roll_cmd = -cmd.turn.clamp(-1.0, 1.0) * p.heli_roll_max;
    let tilt_p =
        (p.heli_att_k * (pitch_cmd - pitch) - HELI_CYC_D * w.x).clamp(-HELI_CYCLIC, HELI_CYCLIC);
    let tilt_r =
        (p.heli_att_k * (roll_cmd - roll) - HELI_CYC_D * w.z).clamp(-HELI_CYCLIC, HELI_CYCLIC);
    // The thrust axis: body-up tilted by the two cyclic angles
    // (R_roll · R_pitch · ŷ), carried to world.
    let axis = b.rot.rotate(vec3(
        -tilt_r.sin() * tilt_p.cos(),
        tilt_r.cos() * tilt_p.cos(),
        tilt_p.sin(),
    ));

    // Collective: trim cancels gravity exactly at centered stick —
    // hands-off hover by construction — and the lift stick burns
    // above/below trim, all capped by the rotor's ceiling.
    let trim = HELI_MASS * p.gravity / axis.y.clamp(0.6, 1.0);
    let thrust = (trim + cmd.lift.clamp(-1.0, 1.0) * p.heli_lift).clamp(0.0, p.heli_t_max);

    // Tail thruster: a rate loop — force per rad/s of yaw-rate error,
    // pushed the way that swings the NOSE toward the command (+x force
    // at −z lever = −y torque, hence the sign).
    let tail =
        (HELI_YAW_P * (cmd.turn.clamp(-1.0, 1.0) * p.heli_yaw - w.y)).clamp(-HELI_TAIL_MAX, HELI_TAIL_MAX);

    // Every force lands on the ONE body: gravity through the CoM, each
    // thruster at its mount — r × F propagates the parts' pushes into
    // the whole machine's motion.
    let mut f = pm::Forces::default();
    f.central(vec3(0.0, -HELI_MASS * p.gravity, 0.0));
    f.at(axis * thrust, b.rot.rotate(HELI_ROTOR));
    f.at(b.rot.rotate(vec3(-tail, 0.0, 0.0)), b.rot.rotate(HELI_TAIL));
    f.apply(b, HELI_MASS, HELI_INERTIA, dt);

    // Airframe drags: the old horizontal/vertical split on the linear
    // half, plus rotational drag so a spin decays even at cyclic
    // saturation.
    b.vel.x *= 1.0 - p.heli_hdrag * dt;
    b.vel.z *= 1.0 - p.heli_hdrag * dt;
    b.vel.y *= 1.0 - p.heli_vdrag * dt;
    b.ang = b.ang * (1.0 - HELI_ANG_DRAG * dt);
    // Advancing-blade cap: full collective + full tilt can't run away.
    let h2 = b.vel.x * b.vel.x + b.vel.z * b.vel.z;
    if h2 > p.heli_vcap * p.heli_vcap {
        let s = p.heli_vcap / h2.sqrt();
        b.vel.x *= s;
        b.vel.z *= s;
    }
    b.integrate(dt);

    // Altitude band: skids on the deck (extra drag — parked, not
    // sliding, and the skids resist spinning too), hard ceiling.
    if b.pos.y <= p.heli_ground {
        b.pos.y = p.heli_ground;
        b.vel.y = b.vel.y.max(0.0);
        b.vel.x *= 1.0 - 3.0 * dt;
        b.vel.z *= 1.0 - 3.0 * dt;
        b.ang = b.ang * (1.0 - 6.0 * dt);
    } else if b.pos.y >= p.heli_ceil {
        b.pos.y = p.heli_ceil;
        b.vel.y = b.vel.y.min(0.0);
    }
    // Arena walls stop you in the air too (biomod containment field).
    // TODO(box3d-move): hand clamp, same story as the truck's (master
    // note atop hogs phys.rs) — and the heli itself is on that list
    // (kinematic mirror → real solver body).
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
    // Ramps are terrain for the skids too (playtest 1: a heli used to
    // hover INSIDE a wedge — the flat `heli_ground` clamp was its whole
    // idea of the floor). Below the local surface, ride it: pop to the
    // surface, kill the into-slope velocity, keep the slide — same
    // shared const geometry as the trucks and the solver, so prediction
    // stays byte-exact.
    let (gh, n) = ground_probe(b.pos.x, b.pos.z);
    if gh > 0.0 && b.pos.y < gh + p.heli_ground {
        b.pos.y = gh + p.heli_ground;
        let into = b.vel.x * n.x + b.vel.y * n.y + b.vel.z * n.z;
        if into < 0.0 {
            b.vel.x -= into * n.x;
            b.vel.y -= into * n.y;
            b.vel.z -= into * n.z;
        }
    }
}

// --- spawns ------------------------------------------------------------------

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
pub fn spawn_heli(peer: u8, p: &Params) -> Heli {
    Heli {
        body: Body {
            pos: vec3((peer as f32 - 4.5) * 5.0, p.heli_ground, -ARENA + 2.5),
            ..Body::default()
        },
        ..Heli::default()
    }
}
