//! Shared hogs definitions: the replicated pods, THE truck step (same
//! code on server and in client prediction replay — drive's lesson), and
//! the pure geometry both sides use. Hogs are server-owned NPCs: clients
//! never step them, only interpolate — so `hog` state has no client-side
//! step function at all, just a lerp.

use pm::{Body, Id, Quat, vec3};

// (`addr=` + `password=` landed 2026-07-20 — menu Join field, CLI args,
// and the deploy/ box all dial anywhere now.)
// TODO(ship): reconnect-in-place — real sessions have drops, and a drop
// currently costs your vehicle. Join-in-progress is the same seam (a
// late peer is a reconnect with no history).
pub const ADDR: &str = "127.0.0.1:48223";
/// What the menu's HOST verb binds: every interface, so friends can
/// dial the host's IP while the host itself joins over loopback.
pub const HOST_BIND: &str = "0.0.0.0:48223";
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
/// overrides for feel A/B's. 33 ms is the played-in default (picked
/// 2026-07-18 after the lag=80/loss=3% sessions: "fixed nearly
/// everything" vs 50 — fresher remotes, and loss bursts still hide
/// inside the extrapolation cap). Try `interp=200 lag=80 loss=0.03`:
/// the world turns to soup but shots still land — lag comp rewinds
/// deeper. Now `interp=8`: fresh but strobing under loss. 33 is a
/// *choice*, not a law.
pub const INTERP_DELAY: f32 = 0.033;

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
// - CREATION-FROZEN config — the interp delay: baked into pool
//   registration at connect. A param must be hot-readable; interp
//   needs a reconnect story first.
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
        /// Tail-boom hit: yaw kick scale (torque scales with obliquity).
        pub heli_tail_kick: 0.5 in 0.0..2.0,
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
        /// Heat per second while boosting / cooling per second while not.
        pub heat_rate: 0.4 in 0.05..2.0,
        pub heat_cool: 0.25 in 0.05..2.0,
        /// Collective authority above/below hover trim, and the total
        /// thrust ceiling.
        pub heli_lift: 16.0 in 4.0..40.0,
        pub heli_t_max: 34.0 in 10.0..80.0,
        /// Tail-rotor yaw rate (rad/s).
        pub heli_yaw: 1.9 in 0.5..5.0,
    }
}

/// Default params file path; a `params=PATH` arg overrides. Local tuning
/// state, gitignored — the shipped defaults live in the [`Params`]
/// declaration above.
pub const PARAMS_FILE: &str = "hogs.params";

/// Load the params file through the generated clamped codec. Missing
/// file = shipped defaults; unknown names and bad values are skipped
/// with a COUNT warning (a typo shouldn't be silent).
pub fn params_load(path: &str) -> Params {
    let mut p = Params::default();
    if let Ok(text) = std::fs::read_to_string(path) {
        let lines = text
            .lines()
            .filter(|l| {
                let l = l.trim();
                !l.is_empty() && !l.starts_with('#') && l.contains('=')
            })
            .count();
        let applied = p.apply_save_text(&text);
        if applied < lines {
            eprintln!(
                "[params] {path}: {} line(s) ignored (unknown name or bad value)",
                lines - applied
            );
        }
    }
    p
}

/// Rewrite the params file from the authoritative set — the server's
/// answer to a [`PARAM_SAVE`] event. Whole-file rewrite in the platform
/// save-set line shape.
pub fn params_save(path: &str, p: &Params) -> std::io::Result<()> {
    std::fs::write(
        path,
        format!(
            "# hogs params — name=value, edited live via pm-watch (`set hogs params.<name> V`)\n{}",
            p.to_save_text()
        ),
    )
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
    /// Where the params file lives — the menu's HOST path hands it to
    /// the in-process server so live saves land in the same file.
    pub params_path: String,
    /// Server address (`addr=` — connect target for client/bot modes,
    /// bind address for server mode; defaults to [`ADDR`]). Seeds the
    /// menu's join field.
    pub addr: String,
    /// Session password (`password=`, empty = none) — presented when
    /// joining, required of clients when hosting/serving.
    pub password: String,
    /// Show the pre-connect menu (bare launch); CLI modes skip it.
    pub menu: bool,
}

/// Live-tunable client knobs, bridged from the telemetry node's signals
/// into a pm single (`"hogs.tune"`) that game tasks read each frame.
#[derive(Clone, Copy)]
pub struct Tune {
    pub day_secs: f32,
    /// The DEBUG VIEW (tilde toggles): hitbox cages — every entity's
    /// DERIVED collision hulls, the capsule+band the server sweeps,
    /// as emissive magenta wireframes (`models::hull_cage_tris`) —
    /// plus the engine overlay: per-task timings, pool populations,
    /// and net counters off this client's `Pm`. Client-side only,
    /// nothing on the wire. The eventual live console opens here.
    pub show_debug: bool,
}

impl Default for Tune {
    fn default() -> Self {
        Tune { day_secs: 480.0, show_debug: false }
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
    /// Turret ELEVATION off level, −TRUCK_AIM_DOWN..TRUCK_AIM_UP — the
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
    /// Chin-gun gimbal, relative to the airframe: azimuth off the nose
    /// (±AIM_MAX — the truck turret's law) and elevation off level
    /// (±HELI_AIM_PITCH). Evolved by `heli_step` from the command like
    /// every predicted field, so it replicates for free — remote
    /// players watch a heli's gun train around under a steady nose.
    pub aim: f32,
    pub aim_pitch: f32,
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
///
/// The deliberate asymmetry: predicted pools (`Truck`, `Heli`) are
/// NOT quantized — reconcile error must be able to reach zero, and a
/// quantization step would leave prediction permanently correcting
/// against rounded truth. Try it: change x/z scale from 64 to 8 and
/// play — 1/8-unit position steps make the horde visibly jitter.
/// Quantization is a *visible* budget decision.
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

/// Server-owned co-op scoreboard AND session state, replicated as a
/// synced single (the SingleRx path drive never exercised): one shared
/// score, the live hog count — and since the game-loop ship item, the
/// whole mission arc every client renders the same screen from. The
/// server's director task is the only writer; see [`LEVELS`] for what
/// the mission fields index into.
#[pm::pod]
pub struct Hunt {
    pub points: f32,
    pub alive: u32,
    /// Wave number within the CURRENT mission (waves/defend kinds).
    pub wave: u32,
    /// Session phase, `PHASE_*` — what screen everyone is on.
    pub phase: u32,
    /// Which level ([`LEVELS`] index) and which mission within it.
    pub level: u32,
    pub mission: u32,
    /// Mirror of the current mission's kind/goal (`MISSION_*`) so
    /// clients render objectives without re-indexing the tables.
    pub kind: u32,
    pub goal: u32,
    /// Progress toward `goal`: waves cleared, checkpoints hit, or the
    /// boss's remaining hp in percent.
    pub done: u32,
    /// Live countdown: brief time remaining, or the race clock.
    pub timer: f32,
}

// --- missions + levels -------------------------------------------------------
//
// The game-loop design record (2026-07-21). A LEVEL is a series of
// MISSIONS on one map, each teaching a skill: fight (waves), protect
// (defend), run (race), then the exam (boss). The tables below are
// CONTENT — compiled const data like `BUILDINGS`, not params: a mission
// list is authored, not live-tuned (wave sizing inside a mission still
// rides `Params::wave_base`/`wave_grow` times the mission's `size`).
//
// The arc is a server-side state machine (the director task) that
// writes ONLY the `Hunt` single; clients render whatever phase says.
// LOBBY waits for the first vehicle, BRIEF is a titled countdown,
// PLAYING runs the current mission's objective, WON/LOST are end
// screens that wait for any player's `Session` event — retry the
// failed mission, or roll to the next level after a win. Level
// "loading" is cheap on purpose: same map, purge the NPCs, brief the
// next mission — a real map switch slots in behind the same seam when
// maps stop being const.

pub const PHASE_LOBBY: u32 = 0;
pub const PHASE_BRIEF: u32 = 1;
pub const PHASE_PLAYING: u32 = 2;
pub const PHASE_WON: u32 = 3;
pub const PHASE_LOST: u32 = 4;

/// Mission kinds — what `goal` counts and what fails you:
/// WAVES: clear `goal` waves; can't fail (deaths just bleed points).
/// DEFEND: clear `goal` waves while the depot stands; depot down = lost.
/// RACE: hit `goal` beacons around [`RACE_LOOP`] before `time` runs out.
/// BOSS: drop the giant hog; `done` reports its hp%.
pub const MISSION_WAVES: u32 = 0;
pub const MISSION_DEFEND: u32 = 1;
pub const MISSION_RACE: u32 = 2;
pub const MISSION_BOSS: u32 = 3;

/// One mission's authored shape. `size` scales the wave engine
/// (`wave_base`/`wave_grow` × size) so later levels reuse kinds harder.
pub struct MissionDef {
    pub kind: u32,
    pub goal: u32,
    /// Race clock, seconds (0 = untimed; only RACE reads it today).
    pub time: f32,
    pub size: f32,
    pub name: &'static str,
    pub brief: &'static str,
}

pub struct LevelDef {
    pub name: &'static str,
    pub missions: &'static [MissionDef],
}

/// The campaign. Two levels on the one map — the second exists to prove
/// level switching and to re-run the kinds harder.
/// TODO(ship): capture points — the fifth mission kind (hold zones to
/// tick `done` up); slots into the director's PLAYING match like the
/// others. And real per-level maps once buildings stop being const.
pub const LEVELS: &[LevelDef] = &[
    LevelDef {
        name: "outbreak",
        missions: &[
            MissionDef {
                kind: MISSION_WAVES,
                goal: 2,
                time: 0.0,
                size: 0.7,
                name: "first blood",
                brief: "clear 2 waves. wasd drives, rmb aims, lmb fires.",
            },
            MissionDef {
                kind: MISSION_DEFEND,
                goal: 3,
                time: 0.0,
                size: 0.8,
                name: "hold the depot",
                brief: "the horde wants the fuel depot. 3 waves - keep it standing.",
            },
            MissionDef {
                kind: MISSION_RACE,
                goal: 8,
                time: 150.0,
                size: 0.6,
                name: "supply run",
                brief: "hit every beacon before the clock dies. outrun them - don't brawl.",
            },
            MissionDef {
                kind: MISSION_BOSS,
                goal: 100,
                time: 0.0,
                size: 0.4,
                name: "the sow",
                brief: "the biomod program's finest. aim for the big one.",
            },
        ],
    },
    LevelDef {
        name: "deeper country",
        missions: &[
            MissionDef {
                kind: MISSION_WAVES,
                goal: 3,
                time: 0.0,
                size: 1.0,
                name: "no rest",
                brief: "3 waves, bigger and meaner.",
            },
            MissionDef {
                kind: MISSION_RACE,
                goal: 12,
                time: 190.0,
                size: 0.9,
                name: "long haul",
                brief: "the loop again - half more of it, under real pressure.",
            },
            MissionDef {
                kind: MISSION_DEFEND,
                goal: 4,
                time: 0.0,
                size: 1.1,
                name: "last depot",
                brief: "4 waves on the depot. gunners in force.",
            },
            MissionDef {
                kind: MISSION_BOSS,
                goal: 100,
                time: 0.0,
                size: 0.7,
                name: "matriarch",
                brief: "she brought the family.",
            },
        ],
    },
];

/// Countdown on the briefing screen before a mission goes live.
pub const BRIEF_SECS: f32 = 5.0;

/// The current level, clamped — a stale replica can't index out.
pub fn level_def(level: u32) -> &'static LevelDef {
    &LEVELS[(level as usize).min(LEVELS.len() - 1)]
}

/// The current mission, clamped the same way.
pub fn mission_def(level: u32, mission: u32) -> &'static MissionDef {
    let m = level_def(level).missions;
    &m[(mission as usize).min(m.len() - 1)]
}

/// The DEFEND objective: a static structure the horde can bite and
/// gunners can shoot. Replicated so every client draws it and its
/// damage; the server registers it in the collider pool as
/// CAT_VEHICLE, which is the whole trick — hog aggro, bites, gunner
/// fire, and friendly-fire chip all route to it through seams that
/// already exist (a defend mission cost zero AI code).
#[pm::pod]
pub struct Depot {
    pub x: f32,
    pub z: f32,
    pub hp: f32,
}

/// Where the depot stands (open ground, downtown — sightlines on
/// purpose), its collider size, and how much chewing it survives
/// (bites chip `Params::bite_dmg`, stray rounds their usual chips).
pub const DEPOT_POS: (f32, f32) = (0.0, 20.0);
pub const DEPOT_R: f32 = 2.4;
pub const DEPOT_H: f32 = 3.2;
pub const DEPOT_HP: f32 = 6.0;

/// The depot's world-space hull — one definition, posed nowhere (it's
/// static): the server registers it, the client mirrors it so bot
/// hold-fire and debug cages agree.
pub fn depot_hull(d: &Depot) -> Hull {
    Hull { a: (d.x, d.z), b: (d.x, d.z), r: DEPOT_R, y: (0.0, DEPOT_H) }
}

/// The RACE loop: beacons around the arena's rim, clear of every
/// building footprint, in running order. `Hunt::done % len` is the
/// current beacon; laps just keep indexing.
pub const RACE_LOOP: [(f32, f32); 8] = [
    (0.0, -70.0),
    (70.0, -70.0),
    (85.0, 0.0),
    (70.0, 50.0),
    (25.0, 85.0),
    (-40.0, 85.0),
    (-85.0, 35.0),
    (-85.0, -40.0),
];
/// Beacon capture radius, and the ceiling a heli must dip under to
/// take one (no scoring the loop from the refuge band).
pub const RACE_CP_R: f32 = 7.0;
pub const RACE_CP_H: f32 = 12.0;

/// The BOSS rides the hog pool (same AI, same interp, same collider
/// seam — adopt, don't rewrite), plus this one-entry synced pool keyed
/// by the hog's id: real hp (the `Hog::hp` wire repr saturates at
/// 1.275, so the big number lives here and `Hog::hp` mirrors the
/// FRACTION for tinting), and membership = "draw me huge".
#[pm::pod]
pub struct Boss {
    pub hp: f32,
}

pub const BOSS_HP: f32 = 30.0;
/// Render scale on the hog model, and the collider grow (m on every
/// surface) that makes the hitbox match the spectacle.
pub const BOSS_SCALE: f32 = 3.0;
pub const BOSS_GROW: f32 = 1.4;

/// Reliable client→server event: advance the session from an end
/// screen (ENTER on won/lost). The server decides what "go" means by
/// phase — retry the failed mission, or next level after a win.
#[pm::pod]
pub struct Session {
    pub op: u32,
}

pub const SESSION_GO: u32 = 0;

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

// TODO(refactor): pool/channel NAMES ("truck", "hog", "param.set", …)
// are string literals duplicated across server.rs and client_setup — a
// typo is a runtime handshake rejection. Pin them as consts here next
// to their pods.

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
    /// Gun elevation off level — the heli chin gun (±HELI_AIM_PITCH)
    /// AND the truck turret (−TRUCK_AIM_DOWN..TRUCK_AIM_UP; each step
    /// clamps its own stops). Azimuth shares `aim` the same way. Same
    /// client-side hold/ease-back animation, streamed as absolute
    /// angles.
    pub aim_pitch: f32,
}

/// Reliable client→server event: respawn as the chosen vehicle (the
/// server swaps your ENTITY — see the server's respawn task for why a
/// swap must be a fresh id).
// TODO(ship): PLAYABLE HOGS (Connor, 2026-07-22) — let a player
// control a hog, probably only the SPECIAL kinds (boss / gunner /
// flyer; a regular hog is too weak to be fun as an avatar). This seam
// is the door: a hog avatar is "a third vehicle" to the engine — a
// VEH_* choice here, a spawn branch in the respawn task, own_set, and
// a hog_step evolving a predicted pod from `Drive` (the current Hog
// pod is server-stepped/interp-only, so a player hog wants its own
// predicted pod the way Truck/Heli do, or the avatar keys into the AI
// pools with the brain skipped). Versus-mode implications (a player
// boss vs the hunters) can come later — mission kinds already gate
// what spawns, so a "play the horde" mission slots into the director.
#[pm::pod]
pub struct Respawn {
    pub vehicle: u32, // VEH_TRUCK | VEH_HELI
}

pub const VEH_TRUCK: u32 = 0;
pub const VEH_HELI: u32 = 1;

// --- tuning ----------------------------------------------------------------

// Tuning that designers move live migrated into [`Params`]. What
// remains const here is STRUCTURAL: geometry, physics identities, and
// control internals whose live mutation would be meaningless or break
// contracts (the param-vs-const taxonomy on the Params declaration).

/// Gravity (also the heli's hover-trim baseline).
pub const G: f32 = 9.81;
/// Truck hitpoints (what one bite chips is `Params::bite_dmg`; hp is
/// the scale everything else is expressed in, so it stays 1.0).
pub const TRUCK_HP: f32 = 1.0;
/// Truck body radius — the shared step's building-push circle.
/// Prediction replays the step byte-exact on both ends, so this stays
/// a CONST, never model-derived; the bullet/bite capsule lives in the
/// truck model's `collide.body` box instead (models.rs).
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
/// Turret elevation stops: real-tank asymmetry — plenty of sky (a
/// flyer at `flyer_ceil` inside gun range needs ~0.55), little
/// depression (flat shots already connect on the deck via `HOG_H`).
pub const TRUCK_AIM_UP: f32 = 0.9;
pub const TRUCK_AIM_DOWN: f32 = 0.35;
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
/// Muzzle height (shoulder-mounted). Where they aim comes from the
/// target hull's band via [`WorldIndex::nearest`] — no aim consts.
pub const HOGGUN_Y: f32 = 0.6;

// --- flying hogs -------------------------------------------------------------

// Biomod WINGED hogs: a slice of every wave (`Params::flyer_frac`)
// grows wings and takes the fight upstairs. The ground horde's reach
// stops at `hog_leap`; flyers cruise at `flyer_alt`, chase the nearest
// vehicle by real 3D distance, and bite through the same contact seam
// as everything else — so altitude stops being absolute safety. It
// stays RELATIVE safety: climb past `flyer_ceil` and the flock sheds,
// leaving a thin refuge band under the hard ceiling (you're safe up
// there; the hunt is thirty units below you). Their colliders join the
// same pool and history ring as the horde, so player bullets hit them
// lag-compensated with zero new sweep code.

/// A winged biomod hog: server-owned like the ground horde, clients
/// interp only. Same quantization scheme as [`Hog`] plus the altitude
/// (this pool joins the change-dense workload, so it rides the wire
/// small: 15 B of payload/entry).
#[pm::pod]
pub struct Flyer {
    #[wire(i16, scale = 64.0)]
    pub x: f32,
    #[wire(i16, scale = 64.0)]
    pub y: f32,
    #[wire(i16, scale = 64.0)]
    pub z: f32,
    #[wire(i16, scale = 10000.0)]
    pub heading: f32,
    #[wire(i16, scale = 256.0)]
    pub speed: f32,
    /// 0..FLYER_HP; clients tint by it. Dead flyers are REMOVED — the
    /// client's falling ragdoll is the death animation.
    #[wire(u8, scale = 200.0)]
    pub hp: f32,
}

/// Flyer body radius, and the collider's altitude half-band about the
/// body center (query pads still grow it shooter-side).
pub const FLYER_R: f32 = 0.7;
pub const FLYER_H: f32 = 0.8;
/// One clean cannon hit drops a flyer at the shipped `gun_dmg` —
/// they're hard to hit, so they're soft.
pub const FLYER_HP: f32 = 0.5;
/// Turn rate (rad/s) — a touch under the ground hog's; wide swoops.
pub const FLYER_TURN: f32 = 2.2;
/// Vertical authority, u/s — how fast a swoop commits (or breaks off).
pub const FLYER_CLIMB: f32 = 7.0;
/// 3D aggro radius. Bigger than the ground horde's `hog_aggro`:
/// flyers are the interceptors.
pub const FLYER_AGGRO: f32 = 45.0;
/// Bite reach above/below the body — the vertical half-band handed to
/// `hull_hits_circle` (ground hogs use [0, `hog_leap`] instead).
pub const FLYER_REACH: f32 = 1.3;

/// Interpolate two flyer samples.
pub fn flyer_lerp(a: &Flyer, b: &Flyer, t: f32) -> Flyer {
    let l = |x: f32, y: f32| x + (y - x) * t;
    Flyer {
        x: l(a.x, b.x),
        y: l(a.y, b.y),
        z: l(a.z, b.z),
        heading: lerp_angle(a.heading, b.heading, t),
        speed: l(a.speed, b.speed),
        hp: l(a.hp, b.hp),
    }
}

// --- helicopter tuning -------------------------------------------------------

/// How hard the heli cyclic chases the stick (1/s) — attitude is still
/// first-order servo'd; the FORCES are honest.
pub const HELI_ATT_K: f32 = 5.0;
/// Attitude limits: pitch tilts up to ~40°, banks up to ~29°. Tilt is
/// the throttle now (it vectors the rotor), so the nose gets more range
/// than the old cosmetic lean.
pub const HELI_PITCH_MAX: f32 = 0.70;
pub const HELI_ROLL_MAX: f32 = 0.50;
/// Chin-gun elevation limit either side of level (~57°) — steep enough
/// to hover flat and strafe the deck, or to track a flyer overhead.
/// Azimuth shares the truck turret's `AIM_MAX`.
pub const HELI_AIM_PITCH: f32 = 1.0;
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
/// Hull circle for the shared step's building pushes (prediction
/// replays it — stays a const, same rule as `TRUCK_R`). The stage-4
/// hitbox parts (cabin/tail/rotor) are `collide.*` boxes in the heli
/// model now (models.rs derives and poses them).
pub const HELI_R: f32 = 1.4;

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
/// Behind that: a Box3D server-side spike for tier-2 dynamics only
/// (collapsing buildings, gameplay debris) — the server steps a Box3D
/// world, poses stream out as quantized pods, sleeping bodies are free
/// bandwidth. Vehicles STAY on the shared steps: solver state can't
/// replay, traces can (the engine-survey lesson on the server's
/// bullets task); rollback physics would be its own future project.
/// DECIDED 2026-07-20 (source review of erincatto/box3d): if this gets
/// demanded, FFI the real thing (`cc` + bindgen, pin a commit, one
/// world owned by a server task) — NEVER rewrite the solver (~15k
/// lines of manifolds/TGS/islands; a Rust-native solver means
/// evaluating Rapier, not hand-porting). Box3D's own FAQ rules out
/// rollback determinism, which confirms this boundary from their side:
/// the predicted steps and the rewound hit path stay pm's, pods in,
/// poses out. AND: under ship-mode, cosmetic client-local chaos
/// (ragdolls, gibs, shake) delivers most of the felt physics at ~zero
/// cost — do that first; this spike only if destruction is a design
/// PILLAR.
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

// TODO(refactor): building_push IS building_push_below at y = 0 (no
// roof sits below 0, so the skip never fires) — delegate one to the
// other and delete the copied body; same compiled math, so prediction
// byte-exactness survives.
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
/// Turret muzzle at the barrel tip, elevated by the turret pitch: the
/// barrel pivots at (0, 1.45, 0) in truck space (where the mesh's
/// barrel box starts), so the tip rises along the aimed line.
pub fn truck_muzzle(t: &Truck) -> (f32, f32, f32, f32, f32) {
    let dir = t.heading() + t.aim;
    let p = t.aim_pitch;
    let (x, z) = (t.body.pos.x, t.body.pos.z);
    (
        x + dir.sin() * 1.9 * p.cos(),
        1.45 + 1.9 * p.sin(),
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
        aim_pitch: _,
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
    // Turret: SLEWS toward the commanded angles at `turret_rate` (both
    // axes) instead of snapping — the command is where you want the
    // gun, the pod is where the barrel actually is, and replaying
    // commands reproduces the chase exactly. No wrap handling needed:
    // the stops mean the short way is always through zero.
    let slew = p.turret_rate * dt;
    let want = cmd.aim.clamp(-AIM_MAX, AIM_MAX);
    t.aim += (want - t.aim).clamp(-slew, slew);
    let want = cmd.aim_pitch.clamp(-TRUCK_AIM_DOWN, TRUCK_AIM_UP);
    t.aim_pitch += (want - t.aim_pitch).clamp(-slew, slew);
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
    let Heli {
        body: _,
        aim: _,
        aim_pitch: _,
    } = *h;
    // Chin gun: crisp copy of the commanded gimbal, the truck turret's
    // law — the client animates the hold and the ease-back, so replay
    // reproduces it exactly.
    h.aim = cmd.aim.clamp(-AIM_MAX, AIM_MAX);
    h.aim_pitch = cmd.aim_pitch.clamp(-HELI_AIM_PITCH, HELI_AIM_PITCH);
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

/// Heli prediction error metric — the shared body term plus the gimbal.
pub fn heli_err(a: &Heli, b: &Heli) -> f32 {
    body_err(&a.body, &b.body)
        + (a.aim - b.aim).abs()
        + (a.aim_pitch - b.aim_pitch).abs()
}

/// Prediction error metric: the shared body term plus the scalars.
pub fn err_metric(a: &Truck, b: &Truck) -> f32 {
    body_err(&a.body, &b.body)
        + (a.steer - b.steer).abs()
        + (a.aim - b.aim).abs()
        + (a.aim_pitch - b.aim_pitch).abs()
        + (a.heat - b.heat).abs()
}

// --- geometry ---------------------------------------------------------------

// TODO(refactor): the five hand-written lerps and two err metrics below
// are trust-based coverage ("remember to add the new field") — the one
// gap the steps' exhaustive destructure can't close. Engine candidate:
// derive lerp/err from #[pm::pod] fields (angle/identity fields tagged
// the way #[wire] tags quantization) — the pm_params! spirit applied
// here; a new pod field then costs zero lerp code.

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
        aim_pitch: lerp_angle(a.aim_pitch, b.aim_pitch, t),
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

/// Interpolate two heli samples — the shared body lerp (nlerp
/// attitude) plus the gimbal angles.
pub fn heli_lerp(a: &Heli, b: &Heli, t: f32) -> Heli {
    Heli {
        body: body_lerp(&a.body, &b.body, t),
        aim: lerp_angle(a.aim, b.aim, t),
        aim_pitch: lerp_angle(a.aim_pitch, b.aim_pitch, t),
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

/// Closest point on segment `a`-`b` to `p`.
pub fn seg_closest(a: (f32, f32), b: (f32, f32), p: (f32, f32)) -> (f32, f32) {
    let (abx, abz) = (b.0 - a.0, b.1 - a.1);
    let (apx, apz) = (p.0 - a.0, p.1 - a.1);
    let len2 = abx * abx + abz * abz;
    let t = if len2 > 1e-8 {
        ((apx * abx + apz * abz) / len2).clamp(0.0, 1.0)
    } else {
        0.0
    };
    (a.0 + abx * t, a.1 + abz * t)
}

/// Distance from point `p` to segment `a`-`b`.
pub fn seg_point_dist(a: (f32, f32), b: (f32, f32), p: (f32, f32)) -> f32 {
    let (cx, cz) = seg_closest(a, b, p);
    let (dx, dz) = (p.0 - cx, p.1 - cz);
    (dx * dx + dz * dz).sqrt()
}

/// A collision shape in WORLD space, decoupled from which pool its
/// owner lives in: a ground-plane capsule (equal endpoints = a
/// cylinder) plus an altitude band. Every "does a shot touch this"
/// question — the server's sweep, the bots' hold-fire gate — goes
/// through [`ray_hits_hull`]. The shapes themselves are authored as
/// `collide.*` marker boxes in each MODEL (models.rs derives a
/// [`crate::models::Proto`] per box; the owner poses it every tick) —
/// a NEW VEHICLE ships its hitbox inside its .glb, and the sweep code
/// never changes.
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

    /// The hull's bounding box — what [`WorldIndex`] files it under.
    pub fn aabb(self) -> pm::Aabb {
        pm::Aabb::new(
            vec3(self.a.0.min(self.b.0) - self.r, self.y.0, self.a.1.min(self.b.1) - self.r),
            vec3(self.a.0.max(self.b.0) + self.r, self.y.1, self.a.1.max(self.b.1) + self.r),
        )
    }
}

/// Where a gunner points to hit `(tx, ty, tz)` from its muzzle:
/// `(heading, climb)`. No lead — hogs shoot at where you ARE, which
/// is most of why they're bad at it (`HOGGUN_SPREAD` is the rest).
pub fn hog_aim(x: f32, y: f32, z: f32, tx: f32, ty: f32, tz: f32) -> (f32, f32) {
    let (dx, dy, dz) = (tx - x, ty - y, tz - z);
    (dx.atan2(dz), dy.atan2((dx * dx + dz * dz).sqrt()))
}

// TODO(refactor): (x, z, y, heading, reach, dy) rides as six positional
// f32s through ray_hits_hull / sweep_colliders / the muzzles /
// line_clear — a small ShotRay struct would align those signatures and
// end the argument-order juggling.
/// A shot's travel — `reach` along `heading` on the ground plane, `dy`
/// total altitude change over it — against one hull, in PRESENT time
/// (vehicles aren't in the history ring; they're slow enough that
/// rewind buys little). SAMPLED, not solved: the step size rides the
/// hull radius (≤ 80% of it), so nothing tunnels whether `reach` is a
/// bullet's per-tick travel or a bot's whole line of fire. Returns the
/// hit point.
///
/// TODO(roadmap): if bullet speeds ever make the sampled sweep leaky,
/// an EXACT capsule cast drops in behind this signature — same pod,
/// solved instead of stepped. Not before: the ≤ 0.8 × radius step has
/// never missed at current speeds.
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

// --- the collider pool -------------------------------------------------------
// The rustdoc on Collider/Contact/Parts/WorldIndex below IS the
// collisions design record (the former docs/collisions.md, folded onto
// the types 2026-07-20). The lag-comp half lives on the server's
// `bullets` task; the broadphase half on pm::DynamicTree.

/// One collidable PART, registered into the collider pool by its
/// owner. THE collision architecture in one sentence: *a vehicle
/// registers each of its parts into a pool; the collisions sweep
/// iterates that pool; the hit is reported back to the vehicle to
/// handle* — detection is DATA the simulation iterates, never
/// functions that know what a helicopter is (decided 2026-07-16,
/// reviewing three generations of coexisting hit tests, each one a
/// type-branch wearing a trenchcoat).
///
/// What that buys: registration is data (spawn writes entries;
/// nothing enumerates kinds), multi-part vehicles and per-part damage
/// fall out for free (the heli's cabin/tail/rotor was pure data
/// entry), and the same contact stream feeds damage, knockback, and
/// sfx without the sweep growing branches. Gunner hogs proved it the
/// day it landed: an NPC shooter is just a task writing into pools
/// the pipeline already understands.
///
/// A five-engine survey (2026-07-17: Box2D v3/Box3D, Quake/Source,
/// Unreal, Unity classic+DOTS, Jolt/Rapier) held the design up
/// against primary sources; it survived wholesale. Everyone
/// converged on exactly this shape: contacts as transient data
/// drained after the sweep (see [`Contact`]), response owned by the
/// struck entity's code, part identity as a small data key (Unity
/// `ColliderKey`, Jolt `SubShapeID`, UE `BoneName`), parts attached
/// flat to one owner with hierarchy kept game-level, and collision
/// worlds never replicated — only poses go on the wire.
///
/// Keying: the entry is keyed by the part's OWN id — a vehicle's
/// parts are child entities ([`parts_add`]); a single-part swarm
/// entity is its own part, keyed by its owner id, so death cleans its
/// entry with the entity and the id space isn't doubled at horde
/// scale. Owners re-pose `hull` every tick (the heli-rotor-matrix
/// habit applied to shapes); the sweep never looks up a pose — and
/// since 2026-07-18 the shapes themselves are authored as `collide.*`
/// marker boxes in each kind's `.glb` (models.rs), so a new vehicle
/// ships its hitbox inside its model.
///
/// The shape vocabulary is deliberately capsule + altitude band and
/// nothing else. It becomes debt only when gameplay demands walkable
/// bridges or stacking — which is exactly the Box3D layer-3 trigger
/// parked on the physics plan (see the BUILDINGS TODO).
#[derive(Clone, Copy)]
pub struct Collider {
    /// The entity this part belongs to — response is ITS business.
    pub owner: Id,
    /// Owner-private part tag (`PART_*`); the sweep carries it through
    /// to the contact untouched and never interprets it.
    pub part: u8,
    /// Category bits — what this entry IS. Sweeps bring their own mask
    /// of what they TEST (the query-carries-its-filter pattern —
    /// Quake's `MASK_SHOT`, Box2D's `b2QueryFilter`; the world never
    /// knows who's asking). Categories are what retired the
    /// friendly-fire ownership walk: one sweep tests `VEHICLE|HOG`
    /// and skips the shooter by `owner`.
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

/// A vehicle's registered part ids, in registration order — `ids[0]`
/// is the body part. Fixed capacity; filler slots repeat the owner id
/// and sit past `n`. Lives on BOTH ends: the server's `vehicle.part`
/// pool and the client's local mirror use the same link shape.
///
/// This link is pm's first real entity RELATIONSHIP, and it breaks an
/// invariant the engine leans on: `id_remove(vehicle)` cleans pools
/// keyed by THAT id, but parts have their own ids — kill the heli and
/// its rotor collider would survive as a ghost. The answer is
/// CONVENTION, not engine feature: a janitor culls entries whose
/// `owner` fails `pm.id_alive` (the O(1) generational check — the
/// same index+generation validate-at-use every surveyed engine ships:
/// `b2Body_IsValid`, Jolt sequence numbers, Unity `Entity.Version`),
/// and response tasks re-check the owner at drain so a mid-tick death
/// never dangles. Engine-level child ids (`id_add_child` + cascading
/// remove) stay PARKED until more parent→child users appear — every
/// surveyed engine cascades shape lifetime inside its physics module
/// and none generalizes it into an entity feature; building it for
/// one caller is the kernel-decomposition mistake again.
#[derive(Clone, Copy)]
pub struct Parts {
    pub ids: [Id; 4],
    pub n: u8,
}

/// Register a vehicle's parts: child entities in the collider pool
/// plus the parent→child link in `parts` (keyed by the VEHICLE, so
/// entity removal cleans the link and the janitor reaps the orphaned
/// parts — see [`Parts`] for why that split exists). Hulls are
/// re-posed by the owner every tick; registration just needs them
/// sane. A truck is one BODY entry; the heli is three. Same call on
/// both ends: the server registers from spawn poses, the client from
/// draw-pool poses.
pub fn parts_add(
    pm: &mut pm::Pm,
    collider: &pm::PoolHandle<Collider>,
    parts: &pm::PoolHandle<Parts>,
    owner: Id,
    cat: u8,
    hulls: &[(u8, Hull)],
) {
    let mut p = Parts { ids: [owner; 4], n: 0 };
    for &(part, hull) in hulls {
        let pid = pm.id_add();
        collider.get_mut().add(pid, Collider { owner, part, cat, hull });
        p.ids[p.n as usize] = pid;
        p.n += 1;
    }
    parts.get_mut().add(owner, p);
}

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

/// One shot-vs-one-entry judgment — the shared core of the history
/// sweep and [`WorldIndex::sweep`], so the two paths cannot disagree.
fn sweep_one(
    x: f32,
    z: f32,
    y: f32,
    heading: f32,
    reach: f32,
    dy: f32,
    pad: f32,
    c: &Collider,
) -> Option<SweepHit> {
    let (hx, hz) = ray_hits_hull(x, z, y, heading, reach, dy, &c.hull.grow(pad))?;
    let (dx, dz) = (hx - x, hz - z);
    let frac = (dx * dx + dz * dz).sqrt() / reach.max(1e-6);
    Some(SweepHit { owner: c.owner, part: c.part, frac, x: hx, y: y + dy * frac, z: hz })
}

/// THE collisions sweep: one shot's travel against every collider
/// entry matching `mask`, nearest hit along the path winning (not
/// registry order — a hog can shield a teammate, and before this
/// ordering a teammate could eat a round a hog between the muzzles
/// should have caught). `skip` drops the shooter's own vehicle
/// (bullets are born at its muzzle); `pad` grows each tested hull
/// QUERY-side — the pad is the shot's forgiveness, and expansion is
/// query-side everywhere in the genre (Quake 3 expands brush planes
/// per-trace by that trace's extents): a collider doesn't know who's
/// shooting at it.
///
/// This SLICE form is the history path: rewound shots run against
/// ring frames, which the tree does not index. Present-time sweeps go
/// through [`WorldIndex::sweep`] — same judgment, pruned.
///
/// Linear over the frame is fine to ~1–2k hogs (~25k capsule tests
/// per tick at 200 hogs + 60 bullets). If ring sweeps ever show in
/// profiles, the answer is revisiting present-only lag comp — NOT
/// `pm::SpatialGrid`, which stays as the simple teaching structure.
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
        let Some(hit) = sweep_one(x, z, y, heading, reach, dy, pad, c) else {
            continue;
        };
        if best.is_none_or(|b| hit.frac < b.frac) {
            best = Some(hit);
        }
    }
    best
}

/// A detected touch — written by the sweep on a fresh id, drained the
/// SAME tick by the struck entity's response task (bites at prio 28,
/// sweep at 31, responses at 32; a runtime guard purges last-tick
/// leftovers loudly): transient facts as pool entries with a
/// lifetime, the contact-points rule — never callbacks, never events.
/// The sweep applies nothing; whoever owns `owner` owns every
/// consequence, so detection and response never meet in the same
/// function.
///
/// The industry arrived here the hard way, which is why this contract
/// is non-negotiable: Box2D v2 had mid-step gameplay callbacks
/// (`BeginContact`/`PreSolve`) and v3 DELETED them for event arrays
/// drained after the step ("callbacks in multithreading are
/// problematic... race conditions in user code, user code becomes
/// non-deterministic" — Catto's migration guide); Unity retrofitted
/// the same batched shape onto PhysX and got ~30% back; Jolt's
/// documented pattern is "buffer them yourself, process after
/// update". Drained-same-tick is that contract with pm's task
/// priorities as the guarantee.
///
/// No normal, no impulse: engines store those for their SOLVER, and
/// ours has none in the loop — knockback derives from `heading`.
/// Fields joined with their consumers, not before (`y` because a
/// rotor strike at altitude is not a ground splash; `source_peer`
/// because peer attribution is all any response ever wanted).
///
/// Small levers, noted not built: opt-in contact REPORTING per
/// collider if contact volume ever matters (Box2D v3.1 ships events
/// off-by-default for perf — a report-bit, not a redesign), and
/// enter/exit EDGES reconstructed owner-side via `pm::Adds` on this
/// pool if a sustained contact ever appears (heal pads).
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

// --- the query seam ---------------------------------------------------------

/// A proximity answer: who's near, where to aim/steer, and how far.
/// The point `(x, y, z)` is the closest point on the hull's capsule
/// AXIS (y clamped into the band) — a chase heading, an aim point, a
/// bite anchor, depending on the caller. `dist` is measured to that
/// point, not the hull's skin, so it lives on the same scale the old
/// center-distance target chains used (`hog_aggro` etc. keep meaning).
#[derive(Clone, Copy)]
pub struct Near {
    pub owner: Id,
    pub part: u8,
    pub x: f32,
    pub y: f32,
    pub z: f32,
    /// The hull's altitude band — bite anchors read `band.0`.
    pub band: (f32, f32),
    pub dist: f32,
}

impl Near {
    fn of(qx: f32, qy: f32, qz: f32, c: &Collider) -> Near {
        let (px, pz) = seg_closest(c.hull.a, c.hull.b, (qx, qz));
        let py = qy.clamp(c.hull.y.0, c.hull.y.1);
        let (dx, dy, dz) = (px - qx, py - qy, pz - qz);
        Near {
            owner: c.owner,
            part: c.part,
            x: px,
            y: py,
            z: pz,
            band: c.hull.y,
            dist: (dx * dx + dy * dy + dz * dz).sqrt(),
        }
    }
}

/// THE world-query index: the collider pool mirrored into a
/// [`pm::DynamicTree`] once per tick, asked three ways —
/// [`nearest`](WorldIndex::nearest) (targeting),
/// [`touch`](WorldIndex::touch) (bites), and
/// [`sweep`](WorldIndex::sweep) (present-time shots; rewound shots
/// stay on [`sweep_colliders`] over HISTORY frames — the tree only
/// indexes the present). Every "what's around me" question goes
/// through here instead of a hand-built per-pool chain: a new vehicle
/// kind registers collider parts and is chased, bitten, and aimed at
/// with zero AI edits — and aggro judges reach by the same
/// band-overlap criterion the bite does.
///
/// BOTH ends keep one: the server's `index` task mirrors the sim's
/// collider pool; the client's `colliders` task poses its own local
/// pool from the smoothed draw pools and mirrors that — so the
/// client's index describes the world this client believes it sees,
/// which is the view the server's lag comp honors for its shots.
/// The boundary that holds on both ends: colliders answer WHERE
/// (geometry questions come here); the pods answer WHAT — bot lead
/// math reads velocity, cage meshes are per kind, and neither belongs
/// in kind-erased shape data.
///
/// Design choices, with their costs:
/// - **Sync, not hooks**: a task refreshes the mirror at a declared
///   priority; pools stay dumb data. Cost: queries see last tick's
///   poses — exactly the staleness the old direct pool reads had
///   (AI at 28 always read what the pose tasks wrote the tick
///   before), made explicit.
/// - **A HashMap rides beside the tree** (`Id → (proxy, Collider)`):
///   tree leaves carry `Id` only, keeping game data out of the
///   engine structure. Cost: one map lookup per candidate.
/// - **False positives by contract**: the tree over-reports
///   (fat boxes); every verb re-runs the exact hull test. Never a
///   false negative.
/// - **The tree buys shape, not milliseconds — today.** ~10 vehicle
///   parts + a few hundred hogs is linear-scan territory; what was
///   bought is the verb API every caller speaks, so bigger worlds
///   (buildings-as-pool, Box3D tier-2) arrive behind the seam with
///   zero caller edits.
pub struct WorldIndex {
    tree: pm::DynamicTree,
    entries: std::collections::HashMap<Id, (u32, Collider)>,
}

impl Default for WorldIndex {
    fn default() -> Self {
        // Margin ≈ a fast hog's travel over ~8 ticks: an entry re-files
        // roughly 8x/second instead of 60x, and idle ones never. A
        // single constant to tune if the tradeoff (lazier updates vs
        // fatter false-positive rings) ever measures wrong.
        WorldIndex { tree: pm::DynamicTree::new(1.0), entries: Default::default() }
    }
}

impl WorldIndex {
    /// Mirror the collider pool: new entries insert, moved ones update
    /// (a no-op inside the fat margin), dead ones leave the tree.
    pub fn sync(&mut self, pool: &pm::Pool<Collider>) {
        let WorldIndex { tree, entries } = self;
        entries.retain(|&id, (proxy, _)| {
            let live = pool.contains(id);
            if !live {
                tree.remove(*proxy);
            }
            live
        });
        for (id, c) in pool.iter() {
            match entries.entry(id) {
                std::collections::hash_map::Entry::Occupied(mut e) => {
                    let (proxy, stored) = e.get_mut();
                    *stored = *c;
                    tree.update(*proxy, c.hull.aabb());
                }
                std::collections::hash_map::Entry::Vacant(v) => {
                    v.insert((tree.insert(c.hull.aabb(), c.cat as u64, id), *c));
                }
            }
        }
    }

    /// Nearest collider in `mask` whose altitude band overlaps
    /// `[band.0, band.1]`, within `within` of `(qx, qy, qz)` — the
    /// targeting verb. The band IS the reach criterion: a hog passes
    /// `(0, hog_leap)` and a climbing heli simply stops existing for
    /// it, exactly as it does for the bite.
    pub fn nearest(
        &self,
        qx: f32,
        qy: f32,
        qz: f32,
        within: f32,
        band: (f32, f32),
        mask: u8,
    ) -> Option<Near> {
        let (ylo, yhi) = (band.0.max(qy - within), band.1.min(qy + within));
        if ylo > yhi {
            return None;
        }
        let q = pm::Aabb::new(vec3(qx - within, ylo, qz - within), vec3(qx + within, yhi, qz + within));
        let mut best: Option<Near> = None;
        self.tree.query(q, mask as u64, |id| {
            let (_, c) = &self.entries[&id];
            if c.hull.y.0 > band.1 || c.hull.y.1 < band.0 {
                return;
            }
            let n = Near::of(qx, qy, qz, c);
            if n.dist <= within && best.is_none_or(|b| n.dist < b.dist) {
                best = Some(n);
            }
        });
        best
    }

    /// Nearest collider in `mask` overlapping the vertical circle
    /// (`r` around `(cx, cz)`, altitude band `[band.0, band.1]`) — the
    /// bite verb, [`hull_hits_circle`] behind the tree. Multi-part
    /// owners are touchable on ANY part: a hog flanking a heli's tail
    /// bites the tail, and the contact carries that part tag.
    pub fn touch(&self, cx: f32, cz: f32, r: f32, band: (f32, f32), mask: u8) -> Option<Near> {
        let q = pm::Aabb::new(vec3(cx - r, band.0, cz - r), vec3(cx + r, band.1, cz + r));
        let qy = (band.0 + band.1) * 0.5;
        let mut best: Option<Near> = None;
        self.tree.query(q, mask as u64, |id| {
            let (_, c) = &self.entries[&id];
            if !hull_hits_circle(&c.hull, cx, cz, r, band.0, band.1) {
                return;
            }
            let n = Near::of(cx, qy, cz, c);
            if best.is_none_or(|b| n.dist < b.dist) {
                best = Some(n);
            }
        });
        best
    }

    /// PRESENT-time shot sweep behind the tree — the third verb, same
    /// per-entry judgment as [`sweep_colliders`] (shared [`sweep_one`]
    /// core: pad grows the hull query-side, nearest `frac` wins,
    /// `skip` drops the shooter's own vehicle). For callers that live
    /// in the present: the bots' hold-fire gate. Rewound server shots
    /// keep the slice walk over history frames — the tree does not
    /// index the past.
    pub fn sweep(
        &self,
        x: f32,
        z: f32,
        y: f32,
        heading: f32,
        reach: f32,
        dy: f32,
        pad: f32,
        mask: u8,
        skip: Option<Id>,
    ) -> Option<SweepHit> {
        let p1 = vec3(x, y, z);
        let p2 = vec3(x + heading.sin() * reach, y + dy, z + heading.cos() * reach);
        let mut best: Option<SweepHit> = None;
        self.tree.cast(p1, p2, pad, mask as u64, |id| {
            let (_, c) = &self.entries[&id];
            if skip != Some(c.owner) {
                if let Some(hit) = sweep_one(x, z, y, heading, reach, dy, pad, c) {
                    if best.is_none_or(|b| hit.frac < b.frac) {
                        best = Some(hit);
                    }
                }
            }
            // Clip the remaining traversal to the best hit so far.
            best.map_or(1.0, |b| b.frac)
        });
        best
    }
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
        ..Heli::default()
    }
}

// --- physics sanity ----------------------------------------------------------

/// The force model's invariants, pinned so a tuning pass can't silently
/// break them: grip actually bleeds lateral momentum, the FBW trim
/// actually hovers, tilt actually goes places (and not past the cap).
#[cfg(test)]
mod hull_tests {
    use super::*;
    use crate::models::{Proto, collide_protos, posed};
    use std::f32::consts::FRAC_PI_2;

    // Hulls come from the models now: these tests derive protos from
    // the code-defined models (the same path the registry runs on a
    // real .glb) and pose them where each scenario wants — the sweep
    // itself never knows the difference.

    fn protos(kind: &str) -> Vec<Proto> {
        collide_protos(&match kind {
            "truck" => crate::models::truck(),
            "heli" => crate::models::heli(),
            "hog" => crate::models::hog(),
            _ => crate::models::flyer(),
        })
    }

    /// Truck hull at `(x, 0)` facing +z: capsule (x,∓0.8) r 0.9, band
    /// 0..1.6. Shots travel +x (heading = π/2).
    fn truck_at(x: f32) -> Hull {
        protos("truck")[0].pose(x, 0.0, 0.0, 0.0)
    }

    /// Heli cabin ball posed at altitude (the courtesy shape too).
    fn cabin_at(x: f32, y: f32) -> Hull {
        protos("heli")[0].pose(x, y, 0.0, 0.0)
    }

    #[test]
    fn sweep_hits_a_crossing_truck() {
        let hit = ray_hits_hull(-5.0, 0.0, 1.0, FRAC_PI_2, 10.0, 0.0, &truck_at(0.0));
        assert!(hit.is_some(), "flat shot through the hull must connect");
    }

    #[test]
    fn altitude_band_rejects_overflight() {
        let hit = ray_hits_hull(-5.0, 0.0, 5.0, FRAC_PI_2, 10.0, 0.0, &truck_at(0.0));
        assert!(hit.is_none(), "a shot 5u up overflies a 1.6u hull");
        let under = ray_hits_hull(-5.0, 0.0, 1.0, FRAC_PI_2, 10.0, 0.0, &cabin_at(0.0, 10.0));
        assert!(under.is_none(), "a flat shot passes under a heli at 10u");
        let level = ray_hits_hull(-5.0, 0.0, 10.0, FRAC_PI_2, 10.0, 0.0, &cabin_at(0.0, 10.0));
        assert!(level.is_some(), "a shot at its altitude hits it");
    }

    #[test]
    fn long_reach_cannot_tunnel() {
        // A bot's whole line of fire (GUN_RANGE), heli mid-way: the
        // sampling must scale with reach or this skips right over it.
        let pp = Params::default();
        let dy = 8.0 / 30.0 * pp.gun_range; // climb that crosses its altitude there
        let hit =
            ray_hits_hull(0.0, 0.0, 0.0, FRAC_PI_2, pp.gun_range, dy, &cabin_at(30.0, 8.0));
        assert!(hit.is_some(), "45u sweep must still sample densely enough");
    }

    #[test]
    fn grow_pads_the_hold_fire_gate() {
        let graze = |hull: &Hull| ray_hits_hull(-5.0, 2.0, 1.0, FRAC_PI_2, 10.0, 0.0, hull);
        assert!(graze(&truck_at(0.0)).is_none(), "1.2u off the capsule misses");
        assert!(
            graze(&truck_at(0.0).grow(0.5)).is_some(),
            "the grown gate holds fire on the same pass"
        );
    }

    /// Two trucks on the same firing line, entered far-first: the sweep
    /// must order by travel, honor the category mask, and drop the
    /// shooter's own vehicle.
    #[test]
    fn sweep_orders_masks_and_skips() {
        let (nid, fid) = (Id::new(0, 0, 1), Id::new(0, 0, 2));
        let part = |owner, x| Collider {
            owner,
            part: PART_BODY,
            cat: CAT_VEHICLE,
            hull: truck_at(x),
        };
        let cols = vec![(fid, part(fid, 6.0)), (nid, part(nid, 0.0))];
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

    /// The pad grows the QUERY, not the collider — and the hit
    /// altitude rides the climb.
    #[test]
    fn sweep_pad_is_query_side() {
        let id = Id::new(0, 0, 1);
        let cols = vec![(
            id,
            Collider {
                owner: id,
                part: PART_BODY,
                cat: CAT_VEHICLE,
                hull: truck_at(0.0),
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
        let id = Id::new(0, 0, 3);
        let cols = vec![(
            id,
            Collider {
                owner: id,
                part: PART_BODY,
                cat: CAT_HOG,
                hull: protos("hog")[0].pose(0.0, 0.0, 0.0, 0.0),
            },
        )];
        let shot = |y, pad| {
            sweep_colliders(-5.0, 0.0, y, FRAC_PI_2, 10.0, 0.0, pad, CAT_HOG, None, &cols)
        };
        assert!(shot(1.0, 0.0).is_some(), "flat shot through the body connects");
        assert!(shot(HOG_H + 0.5, 0.0).is_none(), "overflight misses the band");
        assert!(
            shot(HOG_H + 0.5, Params::default().hit_pad_heli).is_some(),
            "the heli pad forgives a vertical near-miss"
        );
    }

    /// Stage-4 heli parts: cabin first (the bite convention), boom
    /// behind, rotor above — and part-level nearest-along-ray means
    /// the boom shields the cabin from astern and the disc catches
    /// overflying fire.
    #[test]
    fn heli_parts_pose_and_shield() {
        // Facing +z at 10u up.
        let parts = posed(&protos("heli"), 0.0, 10.0, 0.0, 0.0);
        assert_eq!(parts[0].0, PART_BODY, "ids[0] convention: cabin first");
        let (_, tail) = parts[1];
        let far = tail.a.1.min(tail.b.1);
        assert!(
            tail.a.1 < 0.0 && tail.b.1 < 0.0 && (far + 2.8).abs() < 1e-4,
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
            flank(10.0 + 1.05), // just over the r=1.0 cabin ball
            Some(PART_ROTOR),
            "just over the cabin, into the disc"
        );
        let astern =
            sweep_colliders(0.0, -6.0, 10.0, 0.0, 12.0, 0.0, 0.0, CAT_VEHICLE, None, &cols)
                .map(|hit| hit.part);
        assert_eq!(astern, Some(PART_TAIL), "from astern the boom eats it first");
    }

    /// A flyer's hull rides its altitude: level shots at its height
    /// connect, deck-level fire passes under the flock.
    #[test]
    fn flyer_hull_rides_its_altitude() {
        let hull = protos("flyer")[0].pose(0.0, 8.0, 0.0, 0.0);
        let shot = |y| ray_hits_hull(-5.0, 0.0, y, FRAC_PI_2, 10.0, 0.0, &hull);
        assert!(shot(8.0).is_some(), "a shot at its altitude connects");
        assert!(shot(1.0).is_none(), "a deck-level shot passes under it");
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
        let leap = Params::default().hog_leap;
        let touch = |z| hull_hits_circle(&truck_at(0.0), 0.0, z, HOG_R, 0.0, leap);
        // Nose contact: capsule half-length 0.8 + its radius + the hog's.
        assert!(touch(0.8 + TRUCK_R + HOG_R - 0.05), "nose contact bites");
        assert!(!touch(0.8 + TRUCK_R + HOG_R + 0.05), "clear of the capsule");
        assert!(
            !hull_hits_circle(&cabin_at(0.0, 10.0), 0.0, 0.0, HOG_R, 0.0, leap),
            "a heli at 10u is out of leaping reach however close in plan"
        );
        assert!(
            hull_hits_circle(&cabin_at(0.0, 2.0), 0.0, 0.0, HOG_R, 0.0, leap),
            "hovering low over the horde gets nipped"
        );
        // The flyers' band rides THEIR altitude instead of the ground:
        // a heli out of leaping range is squarely in winged range.
        let cabin = cabin_at(0.0, 10.0);
        assert!(
            hull_hits_circle(&cabin, 0.0, 0.0, FLYER_R, 10.0 - FLYER_REACH, 10.0 + FLYER_REACH),
            "a flyer at your altitude bites at any altitude"
        );
        assert!(
            !hull_hits_circle(&cabin, 0.0, 0.0, FLYER_R, 3.0 - FLYER_REACH, 3.0 + FLYER_REACH),
            "a flyer seven units below is still climbing, not biting"
        );
    }

    // --- the query seam over the same posed hulls ---------------------------

    /// A collider pool with a truck body and a heli cabin, plus the
    /// synced index — the seam tests' little world.
    fn world(heli_y: f32) -> (pm::Pool<Collider>, WorldIndex, Id, Id) {
        let mut pool = pm::Pool::new();
        let (truck_id, heli_id) = (Id::new(0, 0, 1), Id::new(0, 0, 2));
        pool.add(
            Id::new(0, 0, 11),
            Collider { owner: truck_id, part: PART_BODY, cat: CAT_VEHICLE, hull: truck_at(0.0) },
        );
        pool.add(
            Id::new(0, 0, 12),
            Collider {
                owner: heli_id,
                part: PART_BODY,
                cat: CAT_VEHICLE,
                hull: cabin_at(6.0, heli_y),
            },
        );
        let mut idx = WorldIndex::default();
        idx.sync(&pool);
        (pool, idx, truck_id, heli_id)
    }

    #[test]
    fn nearest_band_is_the_aggro_criterion() {
        // A hog at (10, 0): the heli at 6 is CLOSER in plan, but at 10u
        // its band misses the leap window — the truck is the answer.
        let (_, idx, truck_id, heli_id) = world(10.0);
        let leap = Params::default().hog_leap;
        let n = idx.nearest(10.0, 0.0, 0.0, 4.0 * ARENA, (0.0, leap), CAT_VEHICLE).unwrap();
        assert_eq!(n.owner, truck_id, "the climbing heli must not exist for the horde");
        // Hovering low, the same heli is the nearer legal target.
        let (_, idx, _, heli_id2) = world(2.0);
        let n = idx.nearest(10.0, 0.0, 0.0, 4.0 * ARENA, (0.0, leap), CAT_VEHICLE).unwrap();
        assert_eq!(n.owner, heli_id2, "a low hover is fair game");
        assert_eq!(heli_id, heli_id2);
        // And `within` is a hard gate.
        assert!(idx.nearest(60.0, 0.0, 0.0, 10.0, (0.0, leap), CAT_VEHICLE).is_none());
    }

    #[test]
    fn nearest_aim_point_rides_the_band() {
        // A gunner muzzle at HOGGUN_Y: level shot at a truck (muzzle
        // height is inside the band), belly shot at a heli overhead
        // (clamped to the band floor).
        let (_, idx, truck_id, heli_id) = world(10.0);
        let any = (f32::NEG_INFINITY, f32::INFINITY);
        let n = idx.nearest(4.0, HOGGUN_Y, 0.0, 100.0, any, CAT_VEHICLE).unwrap();
        assert_eq!(n.owner, truck_id, "3D distance: the truck wins up close");
        assert_eq!(n.y, HOGGUN_Y, "truck band contains the muzzle height: level shot");
        // Band-filter the ground floor away: only the heli remains, and
        // the aim point clamps up to its band floor.
        let n = idx.nearest(6.0, HOGGUN_Y, 0.0, 100.0, (5.0, f32::INFINITY), CAT_VEHICLE).unwrap();
        assert_eq!(n.owner, heli_id);
        let cabin = cabin_at(6.0, 10.0);
        assert_eq!(n.y, cabin.y.0, "heli overhead: aim clamps to the band floor (belly)");
    }

    #[test]
    fn touch_is_the_bite_with_a_part_tag() {
        let (_, idx, truck_id, _) = world(10.0);
        let leap = Params::default().hog_leap;
        let hull = truck_at(0.0);
        // Nose-to-hull contact, same numbers the raw predicate test
        // uses above — the seam must agree with hull_hits_circle.
        let z = 0.8 + hull.r + HOG_R - 0.05;
        let n = idx.touch(0.0, z, HOG_R, (0.0, leap), CAT_VEHICLE).unwrap();
        assert_eq!((n.owner, n.part), (truck_id, PART_BODY));
        assert_eq!(n.band, hull.y, "bite anchors read the hull band back");
        assert!(idx.touch(0.0, z + 0.1, HOG_R, (0.0, leap), CAT_VEHICLE).is_none());
    }

    #[test]
    fn index_sweep_agrees_with_the_slice_sweep() {
        // Same shot, both paths: the tree-pruned present-time sweep and
        // the history-frame slice walk share sweep_one, so hit, owner,
        // part, and frac must match exactly — including skip and pad.
        let (pool, idx, truck_id, heli_id) = world(1.5);
        let slice: Vec<(Id, Collider)> = pool.iter().map(|(i, c)| (i, *c)).collect();
        use std::f32::consts::FRAC_PI_2;
        let shots = [
            (-5.0, 0.0, 1.0, FRAC_PI_2, 20.0, 0.0, 0.0, None), // down the row
            (-5.0, 0.0, 1.0, FRAC_PI_2, 20.0, 0.0, 0.0, Some(truck_id)), // skip the near one
            (-5.0, 0.0, 5.0, FRAC_PI_2, 20.0, 0.0, 0.0, None), // overflies both
            (-5.0, 0.0, 3.5, FRAC_PI_2, 20.0, 0.0, 1.2, None), // pad rescues a graze
        ];
        for (x, z, y, heading, reach, dy, pad, skip) in shots {
            let a = idx.sweep(x, z, y, heading, reach, dy, pad, CAT_VEHICLE, skip);
            let b = sweep_colliders(x, z, y, heading, reach, dy, pad, CAT_VEHICLE, skip, &slice);
            assert_eq!(a.is_some(), b.is_some(), "hit/miss must agree (skip={skip:?})");
            if let (Some(a), Some(b)) = (a, b) {
                assert_eq!((a.owner, a.part), (b.owner, b.part));
                assert_eq!(a.frac, b.frac, "same sweep_one core, same frac");
            }
        }
        // And the skip case actually reaches the far hull.
        let hit = idx
            .sweep(-5.0, 0.0, 1.5, FRAC_PI_2, 20.0, 0.0, 0.0, CAT_VEHICLE, Some(truck_id))
            .expect("heli at 1.5u is on this line");
        assert_eq!(hit.owner, heli_id);
    }

    #[test]
    fn sync_tracks_moves_and_deaths() {
        let (mut pool, mut idx, truck_id, heli_id) = world(2.0);
        // The heli relocates far: update must re-file it (well past the
        // fat margin), and nearest must follow.
        if let Some(mut c) = pool.get_mut(Id::new(0, 0, 12)) {
            c.hull = cabin_at(50.0, 2.0);
        }
        idx.sync(&pool);
        let n = idx.nearest(48.0, 0.0, 0.0, 20.0, (0.0, 4.0), CAT_VEHICLE).unwrap();
        assert_eq!(n.owner, heli_id);
        // The truck dies: its entry leaves the index on the next sync.
        pool.remove(Id::new(0, 0, 11));
        idx.sync(&pool);
        let n = idx.nearest(0.0, 0.0, 0.0, 4.0 * ARENA, (0.0, 4.0), CAT_VEHICLE).unwrap();
        assert_eq!(n.owner, heli_id, "only the heli remains");
    }
}

#[cfg(test)]
mod params_tests {
    use super::*;

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
        assert_eq!(params_load("does/not/exist.params"), Params::default());
    }
}

#[cfg(test)]
mod physics_tests {
    use super::*;
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
        assert_eq!(t.aim_pitch, TRUCK_AIM_UP, "elevation clamps at the stop");
        let (_, my, _, dir, climb) = truck_muzzle(&t);
        assert_eq!(climb, TRUCK_AIM_UP, "the shot flies the aimed line");
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
        assert_eq!(t.aim_pitch, -TRUCK_AIM_DOWN, "depression stop is asymmetric");
    }

    #[test]
    fn heli_chin_gun_gimbals_and_clamps() {
        let mut h = spawn_heli(1);
        h.body.pos.y = 10.0;
        let cmd = Drive {
            aim: 0.8,
            aim_pitch: -2.0, // past the gimbal stop
            ..Default::default()
        };
        heli_step(&mut h, cmd, DT, &pp());
        assert_eq!(h.aim, 0.8, "azimuth is a crisp copy");
        assert_eq!(h.aim_pitch, -HELI_AIM_PITCH, "elevation clamps at the stop");
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
