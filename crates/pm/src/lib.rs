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
//! use pm::{Pm, Vec2, vec2};
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
//! // Register a task: priority (lowest runs first), interval (0 = every
//! // tick), then the closure. Clone the handle in at registration.
//! let integrate = body.clone();
//! pm.task_add("integrate", 30.0, 0.0, move |pm| {
//!     let dt = pm.loop_dt();
//!     for (_id, mut b) in integrate.get_mut().iter_mut() {
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
//!   [`SpatialGrid`], and PLC-style logic helpers ([`Hysteresis`],
//!   [`Cooldown`], [`RisingEdge`], …).

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

pub use camera::{
    CAMERA_PRIO, CamAnchor, CamManager, CamRig, CamView, CameraRack, camera_install,
    camera_manager, camera_track,
};
pub use duration::{HistoryRing, pool_expire};
pub use id::Id;
pub use kernel::{
    IntoTaskResult, Pm, PoolHandle, SingleHandle, SingleMut, TaskError, TaskFault, TaskStat,
};
pub use math::{Mat4, Rng, Vec2, Vec3, lerp_angle, vec2, vec3, wrap_angle};
pub use modload::{BUILD_MANIFEST, MOD_ABI, ModLoader, build_manifest, mod_abi, mod_manifest_ptr};
pub use net::Applied;
pub use netmod::{
    ClientNet, EventRx, EventTx, InputRx, InputTx, NET_PRIO, PmClient, PmServer, PoolHistory,
    ServerNet, SingleRx,
};
pub use pool::{Mut, Pool};
pub use predict::Predictor;
pub use smooth::{InterpBuffer, coast_blend, pool_interp, pool_mirror};
pub use spatial::SpatialGrid;
pub use util::{Cooldown, Counter, DelayTimer, FallingEdge, Hysteresis, Latch, RisingEdge};

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
