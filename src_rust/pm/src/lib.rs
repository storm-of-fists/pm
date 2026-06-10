//! pm — data-oriented game framework kernel (Rust restart).
//!
//! Flat task scheduler + sparse-set ECS. See `src_rust/README.md` for the
//! mapping from the C++ framework and the design decisions behind it.

mod id;
mod kernel;
mod net;
mod paged;
pub mod probe;
mod pool;
mod transport;

pub use id::Id;
pub use kernel::{Pm, TaskStat};
pub use net::{Applied, NetClient, NetError, NetServer};
pub use pool::{Mut, Pool};
pub use transport::{EVENT_USER_BASE, QuicClient, QuicServer};
