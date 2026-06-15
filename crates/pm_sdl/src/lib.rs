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

pub use font::{Font, Raster};
pub use sprite::Sprite;

/// The windowed-client opening ceremony, in one call: SDL init, a
/// centered window, the event pump, and the display's measured refresh
/// rate. Set `pm.loop_rate = refresh` — the kernel loop is the
/// pacemaker, NOT vsync: WSLg creates the swapchain vsync but never
/// blocks on it, so an uncapped loop free-runs (~700 fps); on platforms
/// where vsync does block, the absolute-deadline loop absorbs the wait.
///
/// The pieces come back as a tuple so they can move into different task
/// closures (window → render, pump → input). For a 2D canvas client,
/// follow with `window.into_canvas()`.
pub fn window(title: &str, w: u32, h: u32) -> (sdl3::video::Window, sdl3::EventPump, u32) {
    let sdl = sdl3::init().expect("sdl init");
    let video = sdl.video().expect("sdl video");
    let window = video
        .window(title, w, h)
        .position_centered()
        .build()
        .expect("window");
    let pump = sdl.event_pump().expect("event pump");
    let refresh = window
        .get_display()
        .and_then(|d| d.get_mode())
        .map(|m| m.refresh_rate.round() as u32)
        .unwrap_or(60)
        .max(30);
    (window, pump, refresh)
}
