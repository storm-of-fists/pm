//! TTF text rendering for SDL canvases: fontdue rasterization (pure
//! Rust, no SDL3_ttf build dependency) with a per-(glyph, size) texture
//! cache. Built for HUD/overlay/menu amounts of text, not novels.
//!
//! ```ignore
//! let mut font = Font::load_default()?;
//! font.draw(&mut canvas, "score 1230", 10.0, 10.0, 18.0, (255, 255, 255));
//! ```

use std::collections::HashMap;

use sdl3::pixels::PixelFormat;
use sdl3::render::{Canvas, FRect, Texture};
use sdl3::sys::pixels::SDL_PIXELFORMAT_ABGR8888;
use sdl3::video::Window;

/// System font candidates, most preferred first.
const DEFAULT_FONTS: [&str; 4] = [
    "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf",
    "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
    "/usr/share/fonts/truetype/liberation/LiberationMono-Regular.ttf",
    "/usr/share/fonts/TTF/DejaVuSansMono.ttf",
];

struct Glyph {
    tex: Option<Texture>, // None for whitespace / empty raster
    w: f32,
    h: f32,
    xmin: f32,
    top: f32, // offset from baseline down to glyph top (screen y-down)
    advance: f32,
}

pub struct Font {
    font: fontdue::Font,
    cache: HashMap<(char, u32), Glyph>,
}

impl Font {
    pub fn load(path: &str) -> Result<Font, String> {
        let bytes = std::fs::read(path).map_err(|e| format!("{path}: {e}"))?;
        let font = fontdue::Font::from_bytes(bytes, fontdue::FontSettings::default())
            .map_err(|e| format!("{path}: {e}"))?;
        Ok(Font { font, cache: HashMap::new() })
    }

    /// First system font that loads. Errors only if none exist.
    pub fn load_default() -> Result<Font, String> {
        for p in DEFAULT_FONTS {
            if let Ok(f) = Self::load(p) {
                return Ok(f);
            }
        }
        Err(format!("no usable font found (tried {DEFAULT_FONTS:?})"))
    }

    pub fn line_height(&self, px: f32) -> f32 {
        self.font
            .horizontal_line_metrics(px)
            .map(|m| m.new_line_size)
            .unwrap_or(px * 1.2)
    }

    fn glyph_cache(&mut self, canvas: &Canvas<Window>, ch: char, px: f32) -> (char, u32) {
        let key = (ch, px.round() as u32);
        if !self.cache.contains_key(&key) {
            let (m, coverage) = self.font.rasterize(ch, key.1 as f32);
            let tex = if m.width == 0 || m.height == 0 {
                None
            } else {
                let mut rgba = Vec::with_capacity(coverage.len() * 4);
                for a in &coverage {
                    rgba.extend_from_slice(&[255, 255, 255, *a]);
                }
                let format = PixelFormat::try_from(SDL_PIXELFORMAT_ABGR8888).unwrap();
                canvas
                    .create_texture_static(format, m.width as u32, m.height as u32)
                    .ok()
                    .and_then(|mut t| {
                        t.update(None, &rgba, m.width * 4).ok()?;
                        t.set_blend_mode(sdl3::render::BlendMode::Blend);
                        Some(t)
                    })
            };
            self.cache.insert(
                key,
                Glyph {
                    tex,
                    w: m.width as f32,
                    h: m.height as f32,
                    xmin: m.xmin as f32,
                    // fontdue metrics are y-up from the baseline; screen
                    // is y-down: glyph top = baseline - (ymin + height).
                    top: -(m.ymin as f32 + m.height as f32),
                    advance: m.advance_width,
                },
            );
        }
        key
    }

    /// Draw `text` with its top-left at (x, y), `px` tall, in `color`.
    /// Returns the advance width.
    pub fn draw(
        &mut self,
        canvas: &mut Canvas<Window>,
        text: &str,
        x: f32,
        y: f32,
        px: f32,
        color: (u8, u8, u8),
    ) -> f32 {
        let ascent = self.font.horizontal_line_metrics(px).map(|m| m.ascent).unwrap_or(px);
        let baseline = y + ascent;
        let mut pen = x;
        for ch in text.chars() {
            let key = self.glyph_cache(canvas, ch, px);
            let g = self.cache.get_mut(&key).unwrap();
            if let Some(tex) = &mut g.tex {
                tex.set_color_mod(color.0, color.1, color.2);
                let dst = FRect::new(pen + g.xmin, baseline + g.top, g.w, g.h);
                let _ = canvas.copy(tex, None, dst);
            }
            pen += g.advance;
        }
        pen - x
    }

    /// Width `text` would occupy at `px` (no draw).
    pub fn measure(&mut self, text: &str, px: f32) -> f32 {
        let size = px.round() as u32 as f32;
        text.chars().map(|ch| self.font.metrics(ch, size).advance_width).sum()
    }
}
