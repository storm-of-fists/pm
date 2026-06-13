//! pm — data-oriented game framework kernel.
//!
//! Flat task scheduler + sparse-set ECS with networking built into the
//! core. See the repo README for the API tour and design decisions.

mod camera;
mod id;
mod kernel;
mod math;
mod net;
mod netmod;
mod paged;
pub mod probe;
mod pool;
mod predict;
pub mod modload;
mod smooth;
mod spatial;
mod transport;
mod util;

pub use camera::{
    CAMERA_PRIO, CamAnchor, CamRig, CamView, camera_attach, camera_follow, camera_install,
    camera_use,
};
pub use id::Id;
pub use kernel::{AccessError, Handle, IntoTaskResult, Pm, Single, TaskError, TaskFault, TaskStat};
pub use math::{Mat4, Rng, Vec2, Vec3, vec2, vec3};
pub use net::{Applied, NetClient, NetError, NetServer, Outbox};
pub use netmod::{
    AppliedLog, ClientEvents, Commands, NET_PRIO, NetInput, NetStatus, PeerEvents, SentLog,
    ServerEvents, ServerOutbox,
};
pub use pool::{Mut, Pool};
pub use predict::Predictor;
pub use modload::{MOD_ABI, ModLoader, mod_abi};
pub use smooth::{coast_blend, pool_mirror};
pub use spatial::SpatialGrid;
pub use transport::{EVENT_USER_BASE, QuicClient, QuicServer};
pub use util::{Cooldown, Counter, DelayTimer, FallingEdge, Hysteresis, Latch, RisingEdge};
