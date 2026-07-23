//! pm — a data-oriented game framework in Rust: a flat task scheduler,
//! sparse-set component pools, and networking as a first-class core
//! concern — server-authoritative replication, client prediction, and
//! dylib hot-reload mods are built in, not bolted on.
//!
//! # API at a glance
//!
//! Fetch pool/singleton handles during init, clone them into task
//! closures, access inside the task. A task *is* a closure — its "state"
//! is its captures. Handles hide the `Rc<RefCell<..>>` plumbing; `get`
//! locks for read, `get_mut` for write (and stamps the entry's
//! changed-tick, which is what change-detection replication runs on).
//!
//! ```
//! use pm::{Pm, Vec2, task, vec2};
//!
//! struct Body {
//!     pos: Vec2,
//!     vel: Vec2,
//! }
//!
//! // pm is multiplayer-only: a peer is a server or a client, chosen at
//! // construction. Nothing binds until `run`, so the kernel tour below
//! // reads the same on either role.
//! let mut pm = Pm::server("127.0.0.1:0");
//! let body = pm.pool::<Body>("body"); // PoolHandle<Body>: named, typed pool
//!
//! let id = pm.id_add(); // generational id [peer|gen|index]
//! body.get_mut().add(
//!     id,
//!     Body {
//!         pos: Vec2::ZERO,
//!         vel: vec2(1.0, 0.0),
//!     },
//! );
//!
//! // Register a task: priority (lowest runs first), then the closure.
//! // Handles in the [..] list are cloned into the closure (`task!` is
//! // sugar for `task_add` + the clone block; an interval in seconds
//! // goes before the list, 0 = every tick when omitted).
//! task!(pm, "integrate", 30.0, [body], move |pm| {
//!     let dt = pm.loop_dt();
//!     for (_id, mut b) in body.get_mut().iter_mut() {
//!         // The Mut guard can't split-borrow (`b.pos += b.vel * dt`
//!         // won't compile): read locals first, then write.
//!         let step = b.vel * dt;
//!         b.pos = b.pos + step;
//!     }
//! });
//!
//! // `loop_run()` paces to `pm.loop_rate` and blocks until `loop_quit`;
//! // here we drive a few fixed-dt ticks by hand instead.
//! for _ in 0..60 {
//!     pm.loop_once(1.0 / 60.0);
//! }
//! assert!(body.get_id(id).unwrap().pos.x > 0.9);
//! ```
//!
//! Singletons are just single-entity pools ([`Pm::single`]) — there is no
//! separate "state" concept, so a singleton replicates and tears down
//! like any other pool. Networking is built in and not replaceable: pick a
//! role with [`Pm::server`] / [`Pm::client`], [`Pm::sync_pool`] each
//! replicated pool, register channels ([`input`](PmClient::input) /
//! [`event`](PmClient::event)), then [`run`](PmClient::run). Gameplay holds
//! typed handles from the role wrapper ([`ClientNet`]/[`ServerNet`],
//! [`InputTx`]/[`InputRx`], [`EventTx`]/[`EventRx`], [`SingleRx`]) and
//! never touches the socket. The doctrine: the client only ever sends
//! channels; the server only ever replicates pools.
//!
//! # Design decisions
//!
//! Why the pools/singles/tasks shape? Three reasons that compound:
//! **cache honesty** (a pool is a dense array — stepping 300 NPCs
//! touches 300 consecutive pods, not 300 heap objects behind
//! pointers), **replication falls out** (a pool of `#[pm::pod]`
//! structs is bytes with names — "send the world" becomes "diff some
//! arrays", no serialization framework), and **determinism is
//! auditable** (state lives in enumerable places; when prediction
//! replays inputs you can name everything it must restore).
//!
//! - **`Rc<RefCell<..>>` behind handles, not raw pointers.** Borrow
//!   checks are runtime but per-task-per-tick (one counter check), not
//!   per-entity — invisible in the hot loop.
//! - **Single-threaded kernel.** Parallelism will be an explicit door
//!   (threaded stores), not ambient scheduler magic.
//! - **One erased store.** Pools live in a single
//!   `HashMap<String, Rc<RefCell<dyn ErasedPool>>>`; supertrait upcasting
//!   recovers the typed pool. No parallel registries, and no separate
//!   "state" concept — a singleton is a single-entity pool.
//! - **The replicated pool is the wire format.** Synced components are
//!   `Pod`; if bandwidth pinches, make the component compact (i16
//!   positions) rather than inventing a serializer.
//!
//! # Guided tour
//!
//! New here and want the whole system explained start to finish? Read
//! the module docs in the order the machine does things, with
//! `examples/hogs` open as the worked example (its module docs carry
//! the game-side half of each lesson):
//!
//! 1. **One world**: [`Pm`] and the kernel docs — pools, singles,
//!    tasks, ids (`kernel.rs`, `pool.rs`).
//! 2. **The doctrine**: the netmod module doc (`netmod.rs`) — clients
//!    send channels, the server replicates pools; then `net.rs` for
//!    snapshots, acks, flights, and the byte budget.
//! 3. **Feel**: [`Predictor`] (`predict.rs`) for your own vehicle,
//!    [`PmClient::interp_pool`] + `duration.rs` for everyone else and
//!    the lag-comp rewind; hogs' `player_client.rs` cosmetic-gun
//!    comments for the 0 ms layer.
//! 4. **Physics**: the [`Body`] rustdoc — three tiers, library
//!    functions, no engine.
//! 5. **The lag lab**: `transport.rs` (`LagSocket`, the net doctor) —
//!    you cannot feel-test netcode on localhost; hogs defaults to
//!    lag=80 loss=0.03 because honest conditions are the shipped
//!    experience.
//!
//! Reading about netcode is like reading about swimming — most module
//! docs and the hogs sources carry "try this" experiments; do them.
//!
//! # Module map
//!
//! Each area's design notes live on its types — follow the links:
//!
//! - **kernel**: [`Pm`], [`PoolHandle`], [`SingleHandle`] — tasks, ids,
//!   the loop.
//! - **pool**: the sparse-set [`Pool`] and its [`Mut`] write guard.
//! - **net**: server-authoritative snapshot-delta replication behind the
//!   role wrappers ([`PmServer`]/[`PmClient`]) and their typed channel
//!   handles.
//! - **predict / smooth**: the front doors are
//!   [`PmClient::predict_pool`] (local avatar) and
//!   [`PmClient::interp_pool`] (remote entities); [`Predictor`] is the
//!   replay core beneath.
//! - **journal**: the server's tick-addressed past —
//!   [`PmServer::journal_pool`] returns the [`JournalHandle`] rewinds
//!   read (lag comp, recordings); [`PmServer::ttl_pool`] is the other
//!   server time modifier (transient entries expire).
//! - **camera**: cameras as entities attached to other entities
//!   ([`camera_track`], [`CameraRack`], [`CamManager`]).
//! - [`modload`]: dylib hot-reload mods.
//! - [`probe`]: drop-in scoped profiling; see also [`Pm::task_stats`].
//! - **math / spatial / util**: [`Vec2`]/[`Vec3`]/[`Mat4`]/[`Rng`], the
//!   angle helpers [`wrap_angle`]/[`lerp_angle`] (use `lerp_angle` for
//!   every angular field in a pool lerp), the [`SpatialGrid`], and
//!   PLC-style logic helpers ([`Hysteresis`], [`Cooldown`],
//!   [`RisingEdge`], …).

// TODO(v2): THE ENGINE-V2 LIST (2026-07-22, after the mission-system
// sessions) — what we'd make FOUNDATIONAL if pm were rewritten with
// the same goals (multiplayer-first, network perf above all). NOT a
// plan to rewrite: the load-bearing decisions (channels-in/pools-out
// doctrine, shared-step prediction, collider-pool collisions, QUIC,
// the single-threaded kernel) are stress-validated and STAY. These
// are the lessons that would change what sits at the CENTER — most
// are retrofittable one at a time, and several existing
// TODO(refactor)/TODO(roadmap) items are partial steps toward them.
// Ranked by how much we'd pay for them.
//
// GREENLIT 2026-07-22 (Connor): no longer a record — being adopted
// IN-PLACE, each item landing beside the old machinery and the old
// path torn out once the new one proves. Order: 3 (determinism
// boundary, the safety net) → 2 (tick journal, scoped to reconnect
// first — it IS the ship-list reconnect item) → 1 (pod compiler) →
// 4 when entity counts demand → 5 at the next serious shader push.
//
// TODO(v2): 1. THE POD COMPILER as the engine's spine. A replicated
// pod's semantics live smeared across the struct, #[wire] attrs, a
// hand lerp, a hand err metric, and the step's destructure guard —
// hogs' Truck::aim_pitch touched all five, and the lerp/err edits are
// trust-based. V2 shape: ONE schema declaration per pod — every field
// tagged with meaning (angle / position / quantization / lerp policy
// / predicted-vs-server-owned) — and codegen emits the wire repr,
// lerp, err metric, interp support, debug inspectors, and a SCHEMA
// HASH. The hash is the sleeper: real schema identity unlocks
// versioned handshakes → reconnect-after-patch and rolling server
// upgrades (today's strict-equality handshake is the right cheap
// call and a dead end). pm_params! already proved the one-line-
// declaration style; this is that idea applied to every synced pod.
// STAGE 2 LANDED 2026-07-23: the hash rides the handshake (ALPN pm/4)
// — every wire registration demands `PodSchema` (macro-emitted; the
// empty impl is the unhashed fallback), schema rows carry it, and a
// mismatch now names the drifted channel ("'hog': schema hash
// differs") instead of shrugging. Same-size field drift — reordering,
// a quantization scale, a lerp tag — fails the connect. The pod also
// SUPPLIES its own blend now: `interp_pool`/`predict_pool` read
// pod_lerp/pod_err by trait, no closures at the call sites (hand
// lerps in demo/drive deleted too). CLOSED 2026-07-23 with two
// DECISIONS, not code: (a) accept-compatible version NEGOTIATION is
// consciously deferred — WireReg's strict-equality doctrine holds
// until version-skewed fleets are real, and the concrete thing
// "reconnect-after-patch" needed was never negotiation: it's a
// PERSISTED session token (hogs writes it to disk, mtime-refreshed
// while playing) + the hash keeping same-schema relaunches
// compatible. (b) auto-installing interp/predict stays explicit-one-
// line-per-pool: the derivable half (lerp/err) is generated, the step
// can't be, and the remaining line IS the record of which pools get
// draw siblings — hiding it buys nothing and costs surprise.
//
// TODO(v2): 2. ONE TICK-ADDRESSED STATE HISTORY under everything —
// the only genuinely rewrite-shaped item. Four mechanisms are
// secretly the same thing, each with bespoke storage of "state at
// tick T": snapshot unacked/resend tracking, interp_pool's sample
// buffer, the journal's lag-comp ring, the predictor's replay
// window. V2 shape: every synced pool is backed by a single ring of
// tick-stamped frames, and snapshots, interp, rewind, and prediction
// replay all DERIVE from it — as do the features that keep being
// hard because it doesn't exist: recordings, replays, kill-cams.
// STAGE 1 LANDED 2026-07-22: `PmServer::journal_pool` is the named
// journal (one shared tick-stamped ring per pool — history_pool's
// wrapper has since been folded away), and RECONNECT shipped with it — though the recon
// found reconnect never needed the journal: a fresh peer's delta
// cursors already reconverge from zero, so reconnect is the pm/3
// session-token handshake (transport.rs: token reclaims the peer id
// inside a grace window; hogs parks the vehicle) plus
// `ClientNet::lost` for the redial loop. Join-in-progress was free
// all along. STAGE 2 LANDED 2026-07-23, both halves. Server: the
// packer's dirty scan derives from a per-tick change capture
// (`NetServer::refresh` → per-peer CANDIDATE LISTS; pack walks are
// O(dirty), the old O(entities × pools × peers × datagrams) scan is
// gone — the M2 16×1000 prerequisite). Client: interp samples live on
// the TICK AXIS — stamped with applied snapshot LABEL times, sampled
// by a soft-slewed estimate of the newest label, so arrival jitter
// and flight bursts stop wobbling the spacing (the predictor's window
// already rode input seqs — that axis by construction).
// STAGE 3 (RECORDINGS) LANDED 2026-07-23, exactly as the net.rs note
// predicted: the recorder is a virtual peer (RECORDER_PEER=255, id
// reserved at admission) with an unbounded budget and instant
// self-ack — first frame is a free keyframe, every later frame a pure
// delta, removal recycling never waits. `PmServer::record_to(path)`
// writes PMREC (header carries the encoded handshake schema);
// `PmClient::replay_from(path)` plays it through the NORMAL apply
// path on the tick clock — interp/draw/HUD can't tell the difference,
// and a schema-drifted viewer is rejected with the same named diff as
// a live connect. hogs: `server record=FILE` / `hogs replay=FILE`.
// Kill-cam is now a GAME feature (play the tail of a recording, or a
// journal window, back through the same seam) — pull when wanted.
//
// TODO(v2): 3. AN EXPLICIT DETERMINISM BOUNDARY. Shared steps must
// replay byte-exact and today that's convention (const-vs-param
// rules, "same compiled math" comments). V2 shape: the sim is its
// own crate of pure versioned functions with golden-replay tests —
// CI fails if a step's output changes. Makes cross-version
// prediction, replay files, and "did this refactor change the
// physics" machine-answerable instead of soak-answerable.
// STAGE 1 LANDED 2026-07-22: hogs' boundary extracted verbatim to
// the `hogs-sim` crate (examples/hogs/sim — steps, predicted pods,
// Drive, Params, shared geometry, muzzles, spawns), re-exported
// through common.rs so call sites didn't move; golden replays
// (sim/tests/golden.rs, scripted LCG command streams → FNV over
// every tick's pod bytes) + SIM_VERSION pin the math. Engine-side
// golden helpers can graduate into pm-world when a second game
// wants them.
//
// TODO(v2): 4. INTEREST MANAGEMENT inside the snapshot packer.
// Smallest-dirty-first fairness is fairness, not RELEVANCE — at 300+
// entities every peer still eventually gets everything. V2 shape:
// per-peer interest scoring (distance, recency, on-screen-ness — the
// parked foveal-as-sort-key idea) decides what fills the flight.
// LANDED 2026-07-22 exactly as the old foveal note predicted — a SORT
// KEY, not a scheduler: PmServer::interest_pool(pool, score) makes
// pack_dirty visit dirty entries in importance × staleness order
// (priority accumulator — staleness guarantees nothing starves), the
// budget keeps doing all the throttling, cross-pool fairness
// unchanged. hogs scores hog/flyer/bullet by distance to the peer's
// vehicle. STAGE 2 LANDED 2026-07-23: the view-pose report — a
// newest-wins DGRAM_VIEW (eye + forward, `ClientNet::view_set` →
// `ServerNet::view_pose`), deliberately NOT the input channel (pure
// presentation metadata, never sim input, bots simply don't send
// one). hogs' scorer now measures from the camera EYE with a
// forward-cone boost: on-screen hogs stream freshest, the swarm
// behind you rides the staleness floor. Play-feel at lag=80/loss=3%
// still unverified — watch for pop-in when whipping the camera.
//
// TODO(v2): 5. (pm-sdl, noted here so the list is one grep) THE
// RENDERER BACKEND. SDL_gpu + naga's combined-sampler limitation —
// no fragment-shader texture sampling, so text/HUD/decals ride
// compute passes — is exactly the wall a serious shader/lighting
// push keeps hitting. A wgpu backend swap is a pm-sdl leaf-crate
// project, not an engine rewrite, and buys back normal materials,
// samplers, and shadow maps. Sequence it BEFORE heavy shader
// investment on the current backend.

// TODO(roadmap): THE COMPONENT TURN (2026-07-23 — Connor: pull shared
// aspects OUT of the per-vehicle pods; "position should be predicted
// the same for almost everything"; forces applied to part assemblies,
// realistic integration with fictional/arcade forces, Unreal-style).
// The insight is right and half-true already: every avatar pod EMBEDS
// `Body` as its kinematic chunk — composition exists, it's just not
// REGISTERED. The hard constraint nobody may forget: prediction's
// atomicity. A predicted pod reconciles as ONE unit against ONE
// input-seq echo; split Body into its own synced pool and a budget
// boundary can deliver Body from tick T with vehicle-extras from T-1
// — the predictor corrects against a state that never existed.
// Component-split PREDICTED state therefore needs pack-atomic
// component GROUPS ("this peer's avatar components ride the same
// datagram") before the split is sound. Staged accordingly:
// STAGE 0 (now, no wire change): physics as a LIBRARY over Body —
//   formalize the parts/attachment layer (parts pool already exists):
//   per-part mass/offset → assembly aggregation (total mass, COM,
//   inertia scalar), forces/impulses accumulate per part → resolve to
//   ONE Body integration → part poses derive from attachment
//   transforms. Vehicles stop hand-rolling "how forces move me";
//   steps become force lists + the shared integrator (the landed
//   truck-grip/heli-thrust model, generalized). Arcade knob = the
//   forces, realism = the integrator — exactly the Unreal recipe.
// STAGE 1 (the M3 avatar decision): predictor over component TUPLES
//   ((Body, TruckCtl) etc), pack-atomic groups in the packer, THEN
//   Body graduates to its own registered pool and interp/predict/
//   collider/interest all key on it once instead of per-vehicle.
// Anti-goal either stage: a solver object owning state — pools stay
// the state, physics stays functions (destruction stance: authored
// states + cosmetic debris; see pm-physics memory).
// TODO(roadmap): THE SCALE LADDER (2026-07-23 — Connor restated the
// end goal, then SHARPENED it same day: "hog hunting is THE game for
// this engine... bf6 level [netcode]... 16 people at a time, maybe
// more... hogs and humans [playable]... destruction... boss battles
// ... a coop story mode. i want it all."). hogs is not a prototype
// for some later game — every rung below is a hogs feature. pm grows
// by making each rung MEASURABLE with bot soaks: scale is testable
// without players, so every rung names its gate and the engine work
// it forces. DESTRUCTION IS NOW A DECLARED PILLAR — that's the
// recorded trigger for the server-side Box3D FFI path when M4 opens
// (never a solver rewrite; prediction stays pm's).
// M1 SHIP HOGS — the helldivers shape (4-8 co-op vs hordes): the
//    TODO(ship) list as-is. Finishing its loop proves the whole stack
//    end to end; the co-op STORY mode grows from the mission system +
//    TODO(story) lore (playing AS hogs is already a story device).
// M2 16×1000 — the mid-size battle: 16 bot clients, 1000 live
//    entities, lag=80/loss=3%. GATE: server tick < 8 ms, ≤ 2 Mbps per
//    peer, clean feel. Forces: journal-derived dirty scan (v2 item 2
//    next stage), view-pose interest (item 4 next stage), tick-budget
//    profiling — and the wgpu swap (item 5) lands HERE, before the
//    content push that follows it.
// M3 INFANTRY — a character shared-step (walk/sprint/jump/vault),
//    skinning (its recorded trigger fires: organic art), input-map
//    key contexts, first-person camera. Playable hogs (TODO(ship)) is
//    the stepping stone: the first avatar that isn't a vehicle.
// M4 THE WORLD FIGHTS BACK — buildings→synced world pool (the
//    recorded destructibility prerequisite on BUILDINGS), terrain
//    heightfield in the collider story, Box3D FFI spike ONLY if
//    destruction becomes a design pillar.
// M5 64 AND BEYOND — the 64-bot soak; interest cells, delta
//    baselines, threaded stores: each lands ONLY when its measurement
//    demands it, never speculatively.
// Recordings/killcam (journal playback) slot in wherever the journal
// stages land — a battlefield-style killcam IS a journal replay.

mod blend;
mod bvh;
mod camera;
mod journal;
mod id;
mod kernel;
mod math;
pub mod modload;
mod net;
mod netmod;
mod paged;
mod pool;
mod predict;
pub mod probe;
mod smooth;
mod spatial;
mod transport;
mod util;

pub use bvh::{Aabb, DynamicTree};
pub use camera::{
    CAMERA_PRIO, CamAnchor, CamManager, CamRig, CamView, CameraRack, camera_install,
    camera_manager, camera_track,
};
pub use blend::{PodErr, PodLerp, PodSchema, schema_hash_str};
pub use id::Id;
pub use kernel::{
    EntryMut, IntoTaskResult, Pm, PoolHandle, SingleHandle, TaskError, TaskFault, TaskStat,
};
pub use math::{Body, Mat4, Quat, Rng, Vec2, Vec3, lerp_angle, vec2, vec3, wrap_angle};
pub use modload::{
    BUILD_ID, BUILD_MANIFEST, MOD_ABI, ModLoader, build_manifest, mod_abi, mod_manifest_ptr,
};
pub use net::{Applied, RECORDER_PEER, Wire};
/// Derive [`Wire`]: generates the compact `<Name>Wire` repr pod from
/// per-field `#[wire(i16, scale = 64.0)]` quantization attributes, for
/// pools registered via [`wire_pool`](Pm::wire_pool). Generated
/// code references `::pm` and `::bytemuck`, so both must be direct
/// dependencies of the deriving crate.
pub use pm_derive::Wire;
/// `#[pm::pod]` — one line instead of the seven-derive pod boilerplate:
/// expands to `#[repr(C)]` + `Clone, Copy, PartialEq, Debug, Default,
/// Pod, Zeroable`, and adds [`Wire`] automatically when any field has a
/// `#[wire(..)]` attribute. See `pm_derive::pod` for the fine print.
pub use pm_derive::pod;
pub use netmod::{
    netdbg_enable,
    ClientNet, EventRx, EventTx, InputRx, InputTx, PmClient, PmServer, JournalHandle,
    ServerNet, SingleRx,
};
pub use pool::{Mut, Pool};
pub use predict::Predictor;
pub use transport::token_random as session_token_random;
pub use spatial::SpatialGrid;
pub use util::{Adds, Cooldown, Counter, DelayTimer, FallingEdge, Hysteresis, Latch, Removes, RisingEdge};

// The sync layer, transport, and raw event plumbing are deliberately not
// public: networking is core, not a pluggable suite. Their tests live
// in-crate below.
#[cfg(test)]
mod events_tests;
#[cfg(test)]
mod net_tests;
#[cfg(test)]
mod netmod_tests;
#[cfg(test)]
mod quic_tests;
