//! pm — data-oriented game framework kernel (Rust restart).
//!
//! Flat task scheduler + sparse-set ECS. See `src_rust/README.md` for the
//! mapping from the C++ framework and the design decisions behind it.

mod id;
mod kernel;
mod math;
mod net;
mod paged;
pub mod probe;
mod pool;
mod predict;
pub mod modload;
mod smooth;
mod spatial;
mod transport;
mod util;

pub use id::Id;
pub use kernel::{AccessError, Handle, IntoTaskResult, Pm, Single, TaskError, TaskFault, TaskStat};
pub use math::{Mat4, Rng, Vec2, Vec3, vec2, vec3};
pub use net::{Applied, NetClient, NetError, NetServer, Outbox};
pub use pool::{Mut, Pool};
pub use predict::Predictor;
pub use modload::{MOD_ABI, ModLoader, mod_abi};
pub use smooth::{coast_blend, pool_mirror};
pub use spatial::SpatialGrid;
pub use transport::{EVENT_USER_BASE, QuicClient, QuicServer};
pub use util::{Cooldown, Counter, DelayTimer, FallingEdge, Hysteresis, Latch, RisingEdge};
