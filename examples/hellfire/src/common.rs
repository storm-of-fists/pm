//! Shared hellfire definitions now live in the `hellfire_core` crate so
//! dylib mods can link the exact same types (TypeId equality requires
//! literally the same compiled crate, not a copy of the source).
pub use hellfire_core::*;
