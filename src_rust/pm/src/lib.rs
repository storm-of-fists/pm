//! pm — data-oriented game framework kernel (Rust restart).
//!
//! Flat task scheduler + sparse-set ECS. See `src_rust/README.md` for the
//! mapping from the C++ framework and the design decisions behind it.

#[cfg(feature = "sdl")]
mod font;
mod id;
mod kernel;
mod math;
mod net;
mod paged;
pub mod probe;
mod pool;
pub mod modload;
mod spatial;
#[cfg(feature = "sdl")]
mod sprite;
mod transport;
mod util;

#[cfg(feature = "sdl")]
pub use font::Font;
pub use id::Id;
pub use kernel::{IntoTaskResult, Pm, Single, Task, TaskError, TaskFault, TaskStat};
pub use math::{Rng, Vec2, vec2};
pub use net::{Applied, NetClient, NetError, NetServer};
pub use pool::{Mut, Pool};
pub use modload::{MOD_ABI, ModLoader};
pub use spatial::SpatialGrid;
#[cfg(feature = "sdl")]
pub use sprite::Sprite;
pub use transport::{EVENT_USER_BASE, QuicClient, QuicServer};
pub use util::{Cooldown, Counter, DelayTimer, FallingEdge, Hysteresis, Latch, RisingEdge};
