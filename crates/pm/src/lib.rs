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
//! let mut pm = Pm::new();
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
//! like any other pool. Networking installs as a task over registered
//! pools: [`Pm::sync`] each replicated pool, then [`Pm::serve`] (server)
//! or [`Pm::connect`] (client); gameplay reads and writes the `"net.*"`
//! singletons ([`PeerEvents`], [`Commands`], [`NetStatus`], …) and never
//! touches the socket.
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
//! - **net**: server-authoritative snapshot-delta replication
//!   ([`NetServer`]/[`NetClient`]) and the installable net modules
//!   ([`Pm::serve`]/[`Pm::connect`]).
//! - **predict / smooth**: client [`Predictor`] and the presentation
//!   helpers [`pool_mirror`], [`coast_blend`], [`pool_interp`]
//!   ([`InterpBuffer`]).
//! - **camera**: cameras as entities attached to other entities
//!   ([`camera_track`], [`CameraRack`], [`CamManager`]).
//! - [`modload`]: dylib hot-reload mods.
//! - [`probe`]: drop-in scoped profiling; see also [`Pm::task_stats`].
//! - **math / spatial / util**: [`Vec2`]/[`Vec3`]/[`Mat4`]/[`Rng`], the
//!   [`SpatialGrid`], and PLC-style logic helpers ([`Hysteresis`],
//!   [`Cooldown`], [`RisingEdge`], …).

mod camera;
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
pub use id::Id;
pub use kernel::{
    IntoTaskResult, Pm, PoolHandle, SingleHandle, TaskError, TaskFault, TaskStat,
};
pub use math::{Mat4, Rng, Vec2, Vec3, vec2, vec3};
pub use modload::{MOD_ABI, ModLoader, mod_abi};
pub use net::{Applied, NetClient, NetError, NetServer, Outbox};
pub use netmod::{
    AppliedLog, ClientEvents, Commands, NET_PRIO, NetInput, NetStatus, PeerEvents, SentLog,
    ServerEvents, ServerOutbox,
};
pub use pool::{Mut, Pool};
pub use predict::Predictor;
pub use smooth::{InterpBuffer, coast_blend, pool_interp, pool_mirror};
pub use spatial::SpatialGrid;
pub use transport::{EVENT_USER_BASE, QuicClient, QuicServer};
pub use util::{Cooldown, Counter, DelayTimer, FallingEdge, Hysteresis, Latch, RisingEdge};
