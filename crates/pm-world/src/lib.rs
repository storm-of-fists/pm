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
//!   [`PmClient::interp_pool`] (remote entities); [`Predictor`] and the
//!   manual helpers [`pool_mirror`], [`coast_blend`], [`pool_interp`]
//!   ([`InterpBuffer`]) are their seams.
//! - **duration**: the server-side counterparts —
//!   [`PmServer::ttl_pool`] (transient entries expire) and
//!   [`PmServer::history_pool`] (past-tick window + rewind, the lag-comp
//!   memory); [`pool_expire`] and [`HistoryRing`] are their seams.
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
//
// TODO(v2): 2. ONE TICK-ADDRESSED STATE HISTORY under everything —
// the only genuinely rewrite-shaped item. Four mechanisms are
// secretly the same thing, each with bespoke storage of "state at
// tick T": snapshot unacked/resend tracking, interp_pool's sample
// buffer, history_pool's lag-comp ring, the predictor's replay
// window. V2 shape: every synced pool is backed by a single ring of
// tick-stamped frames, and snapshots, interp, rewind, and prediction
// replay all DERIVE from it — as do the features that keep being
// hard because it doesn't exist: recordings, replays, kill-cams.
// STAGE 1 LANDED 2026-07-22: `PmServer::journal_pool` is the named
// journal (one shared tick-stamped ring per pool; `history_pool` now
// derives from it), and RECONNECT shipped with it — though the recon
// found reconnect never needed the journal: a fresh peer's delta
// cursors already reconverge from zero, so reconnect is the pm/3
// session-token handshake (transport.rs: token reclaims the peer id
// inside a grace window; hogs parks the vehicle) plus
// `ClientNet::lost` for the redial loop. Join-in-progress was free
// all along. STILL QUEUED here: the packer's dirty scan and the
// client stores (interp samples, predictor window) deriving from the
// journal, then recordings/replays/kill-cams on top.
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
// vehicle. Queued refinement: on-screen-ness via a client view-pose
// report (just another input channel) when a game wants it.
//
// TODO(v2): 5. (pm-sdl, noted here so the list is one grep) THE
// RENDERER BACKEND. SDL_gpu + naga's combined-sampler limitation —
// no fragment-shader texture sampling, so text/HUD/decals ride
// compute passes — is exactly the wall a serious shader/lighting
// push keeps hitting. A wgpu backend swap is a pm-sdl leaf-crate
// project, not an engine rewrite, and buys back normal materials,
// samplers, and shadow maps. Sequence it BEFORE heavy shader
// investment on the current backend.

mod blend;
mod bvh;
mod camera;
mod duration;
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
pub use blend::{PodErr, PodLerp, schema_hash_str};
pub use duration::{HistoryRing, pool_expire};
pub use id::Id;
pub use kernel::{
    EntryMut, IntoTaskResult, Pm, PoolHandle, SingleHandle, TaskError, TaskFault, TaskStat,
};
pub use math::{Body, Mat4, Quat, Rng, Vec2, Vec3, lerp_angle, vec2, vec3, wrap_angle};
pub use modload::{
    BUILD_ID, BUILD_MANIFEST, MOD_ABI, ModLoader, build_manifest, mod_abi, mod_manifest_ptr,
};
pub use net::{Applied, Wire};
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
    ClientNet, EventRx, EventTx, InputRx, InputTx, NET_PRIO, PmClient, PmServer, PoolHistory,
    PoolJournal,
    ServerNet, SingleRx,
};
pub use pool::{Mut, Pool};
pub use predict::Predictor;
pub use smooth::{InterpBuffer, coast_blend, pool_interp, pool_mirror};
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
