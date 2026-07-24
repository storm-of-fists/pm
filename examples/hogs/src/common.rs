//! Shared hogs definitions. The DETERMINISM BOUNDARY — the predicted
//! pods, THE shared steps, params, and the geometry both sides collide
//! against — moved to the `hogs-sim` crate (v2 item 3) and is
//! re-exported here, so game code keeps reaching it as `common::*`.
//! What remains is shared-but-not-replayed: server-owned pods, events,
//! hulls/sweeps, lerps/err metrics, bot helpers. Hogs are server-owned
//! NPCs: clients never step them, only interpolate — so `hog` state has
//! no client-side step function at all, just a lerp.

pub use hogs_sim::*;
// The generated blend methods' traits — in scope wherever pods go.

use pm::{Body, Id};

// (`addr=` + `password=` landed 2026-07-20 — menu Join field, CLI args,
// and the deploy/ box all dial anywhere now. Reconnect-in-place landed
// 2026-07-22 on the v2 session-token handshake: the server parks a
// dropped player's vehicle for p.reconnect_grace and the client
// redials with the same token — see the server roster task and the
// player client's redial loop. Join-in-progress was free all along:
// a fresh peer converges from zero by the delta-cursor design.)
pub const ADDR: &str = "127.0.0.1:48223";
/// What the menu's HOST verb binds: every interface, so friends can
/// dial the host's IP while the host itself joins over loopback.
pub const HOST_BIND: &str = "0.0.0.0:48223";

// (INTERP_DELAY + the PM_INTERP_MS env + interp= arg DISSOLVED
// 2026-07-23 into the `interp_ms` PARAM — one replicated number, both
// halves of the lag-comp contract, live-tunable like everything else.)
//
// TODO(simplify): THE PARAMS SWEEP (2026-07-23 — Connor: "one big text
// file... almost everything needs to live not in code constants, but
// in that text file"). The audit found FOUR mechanisms doing one job
// (consts, params, env vars, CLI args) and ~60 scalar consts across
// common.rs + hogs-sim that are TUNING wearing const clothing. The
// rule going forward (extends the params design record in sim
// lib.rs): **if a number tunes behavior, it's a param in the text
// file; `const` is reserved for (a) wire/enum discriminants (PHASE_*,
// CAT_*, KIND_* — schema, not tuning), (b) determinism anchors
// (FIXED_DT, SIM_VERSION), (c) authored content tables (BUILDINGS,
// LEVELS, RACE_LOOP — level data, a future level-file story, wrong
// shape for scalar params), and (d) invocation identity (ADDR,
// PARAMS_FILE).** The sweep, in batches so goldens stay green:
// 1. Sim scalars → Params: TRUCK_R/STEER_TAU/p.aim_max/aim stops,
//    HELI_* envelope (~10), HOG_*/FLYER_* AI + hp (~12), HOGGUN_*,
//    p.truck_hp/p.depot_hp/BOSS_* , p.brief_secs, p.impact_ttl,
//    p.reconnect_grace. Steps already take &Params — mostly
//    mechanical read-site swaps. Goldens: values identical ⇒ hashes
//    hold; any drift is a bug caught by the tripwire.
// 2. A CLIENT-FEEL pod (second pm_params! block, own file, NOT
//    synced): AIM_SENS, camera constants, extrap (kills the
//    PM_EXTRAP_MS env), day length (absorbs the `day=` arg + Tune
//    single). Same macro, same file format — one mechanism, two
//    scopes (shared truth vs personal feel).
// 3. Env knobs after the sweep: PM_NETDBG/PM_PROF/PM_CC stay (debug
//    tooling, not tuning); everything else dies.

/// Default params file path; a `params=PATH` arg overrides. Local tuning
/// state, gitignored — the shipped defaults live in the [`Params`]
/// declaration above.
pub const PARAMS_FILE: &str = "hogs.params";

/// Load the params file through the generated clamped codec. Missing
/// file = shipped defaults; unknown names and bad values are skipped
// (params_load/params_save/ParamSet/PARAM_SAVE moved INTO THE ENGINE
// 2026-07-23 — `pm::params_load`, `PmServer::params(path)`,
// `PmClient::params()`; params aren't just for hogs.)

/// Parsed CLI flags every client run cares about (see main.rs header
/// for the grammar). One struct so signatures stop growing a parameter
/// per knob.
#[derive(Clone)]
pub struct Flags {
    /// (one-way lag ms, loss fraction) — the simulated link.
    pub link: (f32, f32),
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
    /// Play back a recording (`replay=FILE`) instead of connecting —
    /// spectator view of a file written by the server's `record=FILE`.
    pub replay: Option<String>,
}

/// Live-tunable client knobs, bridged from the telemetry node's signals
/// into a pm single (`"hogs.tune"`) that game tasks read each frame.
#[derive(Clone, Copy)]
pub struct Tune {
    /// The DEBUG VIEW (tilde toggles): hitbox cages — every entity's
    /// SOLVER shape (the Box3D capsule/boxes the server contacts and
    /// casts) as emissive magenta wireframes, posed with the full
    /// replicated Body — plus the engine overlay: per-task timings,
    /// pool populations, and net counters off this client's `Pm`.
    /// Client-side only, nothing on the wire. The eventual live
    /// console opens here.
    pub show_debug: bool,
}

impl Default for Tune {
    fn default() -> Self {
        Tune { show_debug: false }
    }
}

// --- replicated pods -----------------------------------------------------

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
/// through `interp_pool` only.
///
/// THE REPLICATED PHYSICS-ENTITY FORMAT (Connor's call, 2026-07-23,
/// with the Box3D adoption): every physical entity carries the full
/// shared [`pm::Body`] on the wire — position, velocity, orientation
/// quaternion, angular velocity — plus its per-kind gameplay fields.
/// One format, one interp path (`Body`'s PodLerp nlerps attitude), one
/// packer story; what the solver knows, every client sees. Full
/// precision for now (spend-now-tighten-later stance at the ~150-hog
/// scale target): dirty-tracking plus solver SLEEP is the bandwidth
/// diet — a settled hog stops changing and falls off the wire.
/// `heading`/`speed` stopped being fields: yaw and speed are
/// DERIVATIONS of the body ([`Hog::heading`]); the AI's steering
/// scalars live server-side in the brain pods.
#[pm::pod]
pub struct Hog {
    pub body: Body,
    /// 0..p.hog_hp; clients tint by it. Dead hogs are REMOVED, not hp==0.
    pub hp: f32,
}

impl Hog {
    /// The 2D bearing gameplay reads (yaw of the body).
    pub fn heading(&self) -> f32 {
        self.body.yaw()
    }

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
    #[lerp(angle)]
    pub heading: f32,
    /// Climb angle: dy per unit of travel is `sin(pitch)`. 0 for trucks.
    #[wire(i16, scale = 10000.0)]
    #[lerp(angle)]
    pub pitch: f32,
    /// Which peer fired it. A client HIDES its own replicated bullets —
    /// it already drew a local [`Tracer`] at the click (the ~RTT-late
    /// twin would double-draw) — and skips their bang in sfx the same
    /// way. Whole small numbers, so the u8 roundtrip is exact.
    #[wire(u8)]
    #[lerp(snap)]
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

// --- channels --------------------------------------------------------------

// TODO(refactor): pool/channel NAMES ("truck", "hog", "param.set", …)
// are string literals duplicated across server.rs and client_setup — a
// typo is a runtime handshake rejection. Pin them as consts here next
// to their pods.

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


/// Hog body radius (they're round; the biomod part is the attitude).
pub const HOG_R: f32 = 0.7;
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
/// interp only.
///
/// Same replicated-body format as [`Hog`] — flyers aren't solver
/// bodies (yet; their flight is AI-scripted), but the WIRE speaks one
/// language: the AI fills `body` (pos, velocity, yaw quat) and clients
/// interp/render it exactly like everything else.
#[pm::pod]
pub struct Flyer {
    pub body: Body,
    /// 0..p.flyer_hp; clients tint by it. Dead flyers are REMOVED — the
    /// client's falling ragdoll is the death animation.
    pub hp: f32,
}

impl Flyer {
    pub fn heading(&self) -> f32 {
        self.body.yaw()
    }

}

/// Flyer body radius, and the collider's altitude half-band about the
/// body center (query pads still grow it shooter-side).
pub const FLYER_R: f32 = 0.7;
pub const FLYER_H: f32 = 0.8;

// --- muzzles + cosmetic tracers ----------------------------------------------

/// Advance a cosmetic [`Tracer`] one `dt`; `false` = expired. Dies on
/// exactly the walls the real bullet dies on (ground, buildings below
/// the roofline, arena, ceiling, range) so the visual never outlives
/// where the shot could truthfully be — hogs excepted, on purpose.
///
/// TODO(box3d-move): the roofline/arena gate below is a hand march
/// that ignores ramp wedges (tracers fly through them) — becomes a ray
/// cast on the client's local solver world (master note atop phys.rs).
pub fn tracer_step(tr: &mut Tracer, dt: f32, speed: f32, ceil: f32) -> bool {
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
        && tr.y <= ceil
}

// --- geometry ---------------------------------------------------------------

// The five hand-written lerps and two err metrics that used to live
// here are GENERATED now (v2 item 1, landed 2026-07-22): #[pm::pod]
// derives `pod_lerp`/`pod_err` fieldwise — angular fields carry
// #[lerp(angle)], identity-on-a-float fields #[lerp(snap)], and Body/
// Id/ints have their meaning by type (pm's PodLerp/PodErr impls). A
// new pod field costs ZERO lerp code, and a forgotten tag is a visible
// diff on the declaration, not a runtime smear. Call sites pass
// `Truck::pod_lerp` / `Truck::pod_err` — see client_setup.

/// Where a gunner points to hit `(tx, ty, tz)` from its muzzle:
/// `(heading, climb)`. No lead — hogs shoot at where you ARE, which
/// is most of why they're bad at it (`p.hoggun_spread` is the rest).
pub fn hog_aim(x: f32, y: f32, z: f32, tx: f32, ty: f32, tz: f32) -> (f32, f32) {
    let (dx, dy, dz) = (tx - x, ty - y, tz - z);
    (dx.atan2(dz), dy.atan2((dx * dx + dz * dz).sqrt()))
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

#[cfg(test)]
mod pod_blend_tests {
    use super::*;
    use pm::vec3;
    use pm::{Body, PodErr, PodLerp, Quat};

    #[test]
    fn truck_lerp_and_err_match_the_hand_versions() {
        let mut a = spawn_truck(1);
        a.body.vel = vec3(3.0, 0.0, 1.0);
        a.steer = 0.2;
        a.aim = -0.4;
        a.aim_pitch = 0.1;
        a.heat = 0.3;
        let mut b = a;
        b.body.pos = a.body.pos + vec3(2.0, 0.0, 4.0);
        b.body.rot = Quat::from_yaw(0.8);
        b.steer = 0.6;
        b.aim = 0.4;
        b.heat = 0.5;
        // The old truck_lerp: body_lerp + linear scalars + lerp_angle aim.
        let l = a.pod_lerp(&b, 0.5);
        assert_eq!(l.body.pos, vec3(a.body.pos.x + 1.0, 0.0, a.body.pos.z + 2.0));
        assert_eq!(l.body.rot, Quat::nlerp(a.body.rot, b.body.rot, 0.5));
        assert!((l.steer - 0.4).abs() < 1e-6);
        assert!((l.aim - lerp_angle(-0.4, 0.4, 0.5)).abs() < 1e-6);
        assert!((l.heat - 0.4).abs() < 1e-6);
        // The old err_metric: body term + scalar abs-diffs. (Angle
        // fields are wrap-aware now — identical for any diff < π, which
        // the gimbal stops guarantee in play.)
        let body = (a.body.pos.x - b.body.pos.x).abs()
            + (a.body.pos.z - b.body.pos.z).abs()
            + (1.0 - a.body.rot.dot(b.body.rot).abs()) * 8.0;
        let expect = body + 0.4 + 0.8 + 0.0 + 0.2;
        assert!((a.pod_err(&b) - expect).abs() < 1e-5, "{} vs {expect}", a.pod_err(&b));
    }

    #[test]
    fn hog_attitude_lerps_the_short_way() {
        // The Body-format hog: attitude is a quat now, and nlerp takes
        // the short arc across ±π by construction — the property the
        // old #[lerp(angle)] heading field hand-guaranteed.
        let a = Hog {
            body: Body { rot: Quat::from_yaw(3.0), ..Default::default() },
            hp: 1.0,
        };
        let b = Hog {
            body: Body {
                pos: pm::vec3(1.0, 0.0, 0.0),
                rot: Quat::from_yaw(-3.0),
                ..Default::default()
            },
            ..a
        };
        let l = a.pod_lerp(&b, 0.5);
        // 3.0 → -3.0 short way crosses ±π, never passes 0.
        assert!(
            l.heading().abs() > 3.0 || (l.heading() - std::f32::consts::PI).abs() < 0.3,
            "wrapped midpoint, got {}",
            l.heading()
        );
        assert_eq!(l.body.pos.x, 0.5);
    }

    #[test]
    fn bullet_owner_snaps_never_blends() {
        let a = Bullet { x: 0.0, y: 1.0, z: 0.0, heading: 0.0, pitch: 0.0, owner: 1.0 };
        let b = Bullet { owner: 2.0, x: 4.0, ..a };
        let l = a.pod_lerp(&b, 0.25);
        assert_eq!(l.owner, 2.0, "identity, not a quantity");
        assert_eq!(l.x, 1.0);
    }

    #[test]
    fn schema_hash_exists_and_differs_across_pods() {
        assert_ne!(Truck::SCHEMA_HASH, Heli::SCHEMA_HASH);
        assert_ne!(Hog::SCHEMA_HASH, Flyer::SCHEMA_HASH);
        let _ = Body::default(); // Body blends via engine impls, not the derive
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
        let p: Params = pm::params_load(path);
        assert_eq!(p.wave_base, 200.0); // loaded (metadata ignored)
        assert_eq!(p.gunner_frac, 1.0); // clamped to range
        assert_eq!(p.hog_fast, 11.0); // unparseable: default kept
        assert_eq!(p.wave_grow, 15.0); // absent: default kept

        // Save → load is identity for in-range sets.
        pm::params_save(path, &p).unwrap();
        assert_eq!(pm::params_load::<Params>(path), p);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn params_missing_file_is_the_shipped_defaults() {
        assert_eq!(pm::params_load::<Params>("does/not/exist.params"), Params::default());
    }
}

