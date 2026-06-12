//! SDL-side companions to the pm kernel: sprite loading with mtime
//! hot-reload and TTF text rendering. A separate crate (not a pm cargo
//! feature) so `pm` itself compiles exactly one way — mods link the
//! same pm as the host no matter how the game is built.
//!
//! Re-exports `sdl3` so downstream crates write `use pm_sdl::sdl3::...`
//! and inherit one pinned version + build configuration.

pub use sdl3;

mod font;
pub mod gpu3d;
mod sprite;

pub use font::Font;
pub use sprite::Sprite;
