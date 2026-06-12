//! PNG sprite loading, mtime hot-reload, and centered drawing (port of
//! pm_sprite.hpp). Decoding is the pure-Rust `png` crate — no SDL3_image
//! build dependency.
//!
//! Usage:
//! ```ignore
//! let mut s = Sprite::load(&canvas, "resources/player.png");
//! // render task:
//! s.draw_centered(&mut canvas, x, y, 64.0);
//! // ~1 Hz hot-reload task:
//! if s.changed() { s.reload(&canvas); }
//! ```
//!
//! A failed (re)load keeps the old texture and warns — a half-written
//! PNG mid-save just means the next poll retries.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use sdl3::pixels::PixelFormat;
use sdl3::render::{Canvas, FRect, Texture};
use sdl3::sys::pixels::SDL_PIXELFORMAT_ABGR8888;
use sdl3::video::Window;

fn file_mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

fn decode_png(path: &Path) -> Result<(u32, u32, Vec<u8>), String> {
    let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut decoder = png::Decoder::new(file);
    // Normalize exotic PNGs (palette, 16-bit, no alpha) toward RGBA8.
    decoder.set_transformations(png::Transformations::normalize_to_color8() | png::Transformations::ALPHA);
    let mut reader = decoder.read_info().map_err(|e| e.to_string())?;
    let mut buf = vec![0u8; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).map_err(|e| e.to_string())?;
    buf.truncate(info.buffer_size());
    let rgba = match info.color_type {
        png::ColorType::Rgba => buf,
        png::ColorType::GrayscaleAlpha => {
            buf.chunks_exact(2).flat_map(|ga| [ga[0], ga[0], ga[0], ga[1]]).collect()
        }
        other => return Err(format!("unsupported decoded color type {other:?}")),
    };
    Ok((info.width, info.height, rgba))
}

/// A texture with its source path and load-time mtime, so on-disk edits
/// can be detected and hot-swapped.
pub struct Sprite {
    tex: Option<Texture>,
    pub w: f32,
    pub h: f32,
    path: PathBuf,
    mtime: Option<SystemTime>,
}

impl Sprite {
    /// Load from `path`. On failure the sprite is empty (draws nothing)
    /// and a later `reload` retries.
    pub fn load(canvas: &Canvas<Window>, path: impl Into<PathBuf>) -> Sprite {
        let mut s =
            Sprite { tex: None, w: 0.0, h: 0.0, path: path.into(), mtime: None };
        s.reload(canvas);
        s
    }

    /// Re-decode from the stored path and swap the texture in. If the
    /// load fails the old texture stays. Returns true on success.
    pub fn reload(&mut self, canvas: &Canvas<Window>) -> bool {
        // Record mtime first so a load that fails (or keeps failing)
        // isn't retried by `changed()` until the file changes again.
        self.mtime = file_mtime(&self.path);
        let (w, h, rgba) = match decode_png(&self.path) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("pm: sprite '{}' load failed: {e}", self.path.display());
                return false;
            }
        };
        let format = PixelFormat::try_from(SDL_PIXELFORMAT_ABGR8888).unwrap();
        let mut tex = match canvas.create_texture_static(format, w, h) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("pm: sprite '{}' texture failed: {e}", self.path.display());
                return false;
            }
        };
        if tex.update(None, &rgba, (w * 4) as usize).is_err() {
            eprintln!("pm: sprite '{}' texture upload failed", self.path.display());
            return false;
        }
        tex.set_blend_mode(sdl3::render::BlendMode::Blend);
        if let Some(old) = self.tex.take() {
            // Safety: the texture belongs to this canvas's renderer,
            // which outlives the swap (sprites live in states the tasks
            // of one Pm hold; the canvas is owned by the same Pm scope).
            unsafe { old.destroy() };
        }
        self.w = w as f32;
        self.h = h as f32;
        self.tex = Some(tex);
        true
    }

    /// True if the file on disk changed since the last (re)load attempt.
    pub fn changed(&self) -> bool {
        file_mtime(&self.path) != self.mtime
    }

    pub fn loaded(&self) -> bool {
        self.tex.is_some()
    }

    /// Draw centered at (cx, cy), scaled to `display_w` wide with
    /// proportional height.
    pub fn draw_centered(&self, canvas: &mut Canvas<Window>, cx: f32, cy: f32, display_w: f32) {
        let Some(tex) = &self.tex else { return };
        let display_h = if self.w > 0.0 { display_w * (self.h / self.w) } else { display_w };
        let dst = FRect::new(cx - display_w * 0.5, cy - display_h * 0.5, display_w, display_h);
        let _ = canvas.copy(tex, None, dst);
    }
}

#[cfg(test)]
mod tests {
    use super::decode_png;
    use std::path::Path;

    // Decode the real hellfire sprites (texture upload needs a display,
    // so only the decode path is covered headless).
    #[test]
    fn decodes_hellfire_sprites() {
        for (name, w, h) in
            [("connor-front.png", 148, 272), ("connor-back.png", 147, 270)]
        {
            let path = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../examples/hellfire/resources")
                .join(name);
            let (dw, dh, rgba) = decode_png(&path).expect(name);
            assert_eq!((dw, dh), (w, h));
            assert_eq!(rgba.len(), (w * h * 4) as usize);
        }
    }

    #[test]
    fn missing_file_is_an_error_not_a_panic() {
        assert!(decode_png(Path::new("/nonexistent.png")).is_err());
    }
}
