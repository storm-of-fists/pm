//! Thin 3D renderer over the SDL3 GPU API — the boilerplate every 3D
//! example was going to repeat: device + standard flat-shaded pipeline
//! (cull/no-cull pair), depth texture, static mesh upload, and a
//! begin/draw/end frame flow. Deliberately NOT an engine: one vertex
//! format, one shader pair (see shaders/basic3d.wgsl), per-draw
//! uniforms. A game that outgrows it brings its own pipeline and keeps
//! using the device.
//!
//! ```ignore
//! let mut r3d = Renderer3d::new(&window)?;
//! let mesh = r3d.upload_mesh(&bake(&tris, (0.4, 0.6, 0.9)))?;
//! // render task:
//! if let Some(mut frame) = r3d.frame(&window, view, light_world) {
//!     frame.draw(&mesh, model, Tint::WHITE, true);
//! } // drop submits
//! ```
//!
//! Conventions: +y up, +z forward, depth 0..1. The projection bakes the
//! y-flip the Vulkan backend needs, which mirrors screen winding — the
//! culling pipeline therefore treats CLOCKWISE as front. Author meshes
//! CCW-from-outside and it all works out.
//!
//! Projection is (General) Panini by default — pm's house look: wide
//! FOV with a rectilinear center, straight verticals, compressed
//! periphery (`panini_for_fov` couples the distance to the FOV). Done
//! the right way: the scene renders RECTILINEAR (wider source FOV)
//! into an offscreen texture, then a fullscreen pass inverts the
//! panini mapping per pixel (post3d.wgsl) — exact for all geometry.
//! Set `panini = 0.0` to skip the pass entirely and render rectilinear
//! straight to the swapchain.

use std::collections::HashMap;

use pm::{Mat4, Vec3, vec3};
use sdl3::gpu::{
    BlitInfo, Buffer, BufferBinding, BufferRegion, BufferUsageFlags, ColorTargetDescription,
    ColorTargetInfo, CommandBuffer, CompareOp, ComputePipeline, CullMode, DepthStencilState,
    DepthStencilTargetInfo, Device, FillMode, Filter, FrontFace, GraphicsPipeline,
    GraphicsPipelineTargetInfo, LoadOp, PrimitiveType, RasterizerState, RenderPass, SampleCount,
    ShaderFormat, ShaderStage, StorageTextureReadWriteBinding, StoreOp, Texture, TextureCreateInfo,
    TextureFormat, TextureRegion, TextureTransferInfo, TextureType, TextureUsage,
    TransferBufferLocation, TransferBufferUsage, VertexAttribute, VertexBufferDescription,
    VertexElementFormat, VertexInputRate, VertexInputState,
};
use sdl3::pixels::Color;
use sdl3::video::Window;

use crate::font::Font;

/// Vertex of the standard pipeline: position, per-face normal, color.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Vertex3 {
    pub pos: [f32; 3],
    pub normal: [f32; 3],
    pub color: [f32; 3],
}

/// Per-draw uniform block — must match `Uniforms` in basic3d.wgsl.
#[repr(C)]
#[derive(Clone, Copy)]
struct Uniforms {
    mv: [f32; 16],
    proj: [f32; 4],  // sx, sy (negative: y-flip), depth A, depth B
    light: [f32; 4], // xyz dir (view space), w = fog distance
    tint: [f32; 4],
    fog_color: [f32; 4],
}

/// Post-pass uniform block — must match `PostU` in post3d.wgsl.
#[repr(C)]
#[derive(Clone, Copy)]
struct PostU {
    out_scale: [f32; 4], // panini x/y at the screen edges, d, squeeze s
    src_scale: [f32; 4], // source half-tans, output pixel dims
}

/// Text-pass uniform block — must match `TextU` in text.wgsl.
#[repr(C)]
#[derive(Clone, Copy)]
struct TextU {
    rect: [f32; 4],  // dest top-left x, y in pixels (zw unused)
    color: [f32; 4], // rgb tint, a = coverage scale
}

/// Offscreen scene target for the panini path: the scene renders into
/// `color` (oversized, rectilinear), the compute pass warps it into the
/// screen-sized `present`. Rebuilt when fov/panini change.
struct SceneTarget {
    color: Texture<'static>,
    depth: Texture<'static>,
    /// Rectilinear half-tangents the source covers (h, v).
    src_tan: (f32, f32),
    /// Panini coords at the output screen edges (h, v).
    out_edge: (f32, f32),
    /// (fov, panini) this target was built for.
    key: (u32, u32),
}

/// An uploaded static mesh: GPU buffer + vertex count.
pub struct Mesh3 {
    buffer: Buffer,
    count: u32,
}

/// A cached glyph: its GPU coverage texture (None for whitespace) and
/// the metrics needed to place it. Mirrors `font::Raster`, with the
/// coverage living on the GPU.
struct GpuGlyph {
    tex: Option<Texture<'static>>,
    w: u32,
    h: u32,
    xmin: f32,
    top: f32,
    advance: f32,
}

/// A glyph texture awaiting its copy-pass upload (deferred so all uploads
/// ride the frame's command buffer, before the compute passes).
struct PendingGlyph {
    tex: Texture<'static>,
    rgba: Vec<u8>,
    w: u32,
    h: u32,
}

/// The GPU text subsystem: a fontdue face plus a per-(glyph, px) texture
/// cache. Glyphs rasterize on demand (`ensure_glyph`); new ones queue an
/// upload that the next frame's `drop` flushes.
struct TextCache {
    font: Font,
    glyphs: HashMap<(char, u32), GpuGlyph>,
    pending: Vec<PendingGlyph>,
}

/// One laid-out glyph to composite this frame: its texture and where.
struct TextCmd {
    tex: Texture<'static>,
    rect: [f32; 4],
    color: [f32; 4],
    w: u32,
    h: u32,
}

impl TextCache {
    /// Rasterize + cache `ch` at `px` if needed; return its metrics. New
    /// non-empty glyphs create a GPU texture and queue its upload.
    fn ensure_glyph(&mut self, device: &Device, ch: char, px: f32) -> &GpuGlyph {
        let key = (ch, px.round() as u32);
        if !self.glyphs.contains_key(&key) {
            let r = self.font.raster(ch, key.1 as f32);
            let tex = if r.w == 0 || r.h == 0 {
                None
            } else {
                // Coverage replicated across rgba; the shader reads .r.
                let mut rgba = Vec::with_capacity(r.coverage.len() * 4);
                for a in &r.coverage {
                    rgba.extend_from_slice(&[*a, *a, *a, *a]);
                }
                device
                    .create_texture(
                        TextureCreateInfo::new()
                            .with_type(TextureType::_2D)
                            .with_width(r.w)
                            .with_height(r.h)
                            .with_layer_count_or_depth(1)
                            .with_num_levels(1)
                            .with_sample_count(SampleCount::NoMultiSampling)
                            .with_format(TextureFormat::R8g8b8a8Unorm)
                            .with_usage(TextureUsage::COMPUTE_STORAGE_READ),
                    )
                    .ok()
                    .inspect(|tex| {
                        self.pending.push(PendingGlyph {
                            tex: (*tex).clone(),
                            rgba,
                            w: r.w,
                            h: r.h,
                        });
                    })
            };
            self.glyphs.insert(
                key,
                GpuGlyph {
                    tex,
                    w: r.w,
                    h: r.h,
                    xmin: r.xmin,
                    top: r.top,
                    advance: r.advance,
                },
            );
        }
        self.glyphs.get(&key).unwrap()
    }
}

pub struct Renderer3d {
    device: Device,
    pipe_cull: GraphicsPipeline,
    pipe_nocull: GraphicsPipeline,
    pipe_post: ComputePipeline,
    pipe_text: ComputePipeline,
    depth: Texture<'static>,
    scene: Option<SceneTarget>,
    /// Screen-sized final image: every frame resolves here (the panini
    /// warp's target, or the scene's direct target when panini is off),
    /// the text pass composites onto it, then it blits to the swapchain.
    present: Option<Texture<'static>>,
    text: TextCache,
    width: u32,
    height: u32,
    /// HORIZONTAL field of view, degrees. Set via `set_fov` to keep the
    /// panini distance coupled, or write directly to decouple them.
    pub fov_deg: f32,
    /// Panini distance `d`: 0 = rectilinear, ~0.3 mild at 60° FOV up to
    /// ~0.9 at 125°. `set_fov` keeps it on that curve — pm's house
    /// look. Wide FOV without peripheral smearing; verticals stay
    /// straight; helps motion/rotation comfort in vehicle games.
    pub panini: f32,
    /// Vertical squeeze `s` (1 = full panini vertical, 0 = rectilinear
    /// vertical). Leave at 1 unless you know why.
    pub panini_squeeze: f32,
    /// Fog cutoff distance in world units; 0 disables. Default 80.
    pub fog_distance: f32,
    /// Clear / fog color (rgb 0..1).
    pub clear_color: (f32, f32, f32),
}

/// The house fov→panini coupling: d = 0.3 at 60° FOV rising to 0.9 at
/// 125° (clamped outside). Wider view, more cylinder.
pub fn panini_for_fov(fov_deg: f32) -> f32 {
    (0.3 + (fov_deg - 60.0) / 65.0 * 0.6).clamp(0.0, 1.0)
}

/// Panini-x at azimuth `phi` for distance `d`.
fn panini_x(phi: f32, d: f32) -> f32 {
    phi.sin() * (d + 1.0) / (d + phi.cos())
}

/// Inverse panini: panini coords -> rectilinear tangents (the same math
/// the post shader runs per pixel; used here to plan source coverage).
fn panini_inverse(xp: f32, yp: f32, d: f32, s: f32) -> (f32, f32) {
    let k = xp / (d + 1.0);
    let phi = k.atan() + (d * k / (1.0 + k * k).sqrt()).asin();
    let c = phi.cos().max(1e-3);
    let m = (d + 1.0) / (d + c);
    let vert = (1.0 / c) * (1.0 - s) + m * s;
    (phi.tan(), yp / (vert * c))
}

impl Renderer3d {
    /// Device + standard pipelines + depth buffer for `window`.
    pub fn new(window: &Window) -> Result<Renderer3d, String> {
        let (width, height) = window.size();
        let device = Device::new(ShaderFormat::SPIRV, cfg!(debug_assertions))
            .map_err(|e| e.to_string())?
            .with_window(window)
            .map_err(|e| e.to_string())?;

        let vert = device
            .create_shader()
            .with_code(
                ShaderFormat::SPIRV,
                include_bytes!(concat!(env!("OUT_DIR"), "/vs_main.spv")),
                ShaderStage::Vertex,
            )
            .with_entrypoint(c"vs_main")
            .with_uniform_buffers(1)
            .build()
            .map_err(|e| e.to_string())?;
        let frag = device
            .create_shader()
            .with_code(
                ShaderFormat::SPIRV,
                include_bytes!(concat!(env!("OUT_DIR"), "/fs_main.spv")),
                ShaderStage::Fragment,
            )
            .with_entrypoint(c"fs_main")
            .build()
            .map_err(|e| e.to_string())?;

        let build = |cull: bool| {
            device
                .create_graphics_pipeline()
                .with_primitive_type(PrimitiveType::TriangleList)
                .with_vertex_shader(&vert)
                .with_fragment_shader(&frag)
                .with_vertex_input_state(
                    VertexInputState::new()
                        .with_vertex_buffer_descriptions(&[VertexBufferDescription::new()
                            .with_slot(0)
                            .with_pitch(size_of::<Vertex3>() as u32)
                            .with_input_rate(VertexInputRate::Vertex)])
                        .with_vertex_attributes(&[
                            VertexAttribute::new()
                                .with_location(0)
                                .with_buffer_slot(0)
                                .with_format(VertexElementFormat::Float3)
                                .with_offset(0),
                            VertexAttribute::new()
                                .with_location(1)
                                .with_buffer_slot(0)
                                .with_format(VertexElementFormat::Float3)
                                .with_offset(12),
                            VertexAttribute::new()
                                .with_location(2)
                                .with_buffer_slot(0)
                                .with_format(VertexElementFormat::Float3)
                                .with_offset(24),
                        ]),
                )
                .with_rasterizer_state(
                    RasterizerState::new()
                        .with_fill_mode(FillMode::Fill)
                        .with_cull_mode(if cull { CullMode::Back } else { CullMode::None })
                        // Projection y-flip mirrors winding: world-CCW
                        // arrives clockwise.
                        .with_front_face(FrontFace::Clockwise),
                )
                .with_depth_stencil_state(
                    DepthStencilState::new()
                        .with_enable_depth_test(true)
                        .with_enable_depth_write(true)
                        .with_compare_op(CompareOp::Less),
                )
                .with_target_info(
                    GraphicsPipelineTargetInfo::new()
                        .with_color_target_descriptions(&[ColorTargetDescription::new()
                            .with_format(device.get_swapchain_texture_format(window))])
                        .with_has_depth_stencil_target(true)
                        .with_depth_stencil_format(TextureFormat::D16Unorm),
                )
                .build()
                .map_err(|e| e.to_string())
        };
        let pipe_cull = build(true)?;
        let pipe_nocull = build(false)?;
        drop(vert);
        drop(frag);

        // The panini post pass is a COMPUTE pipeline: read the scene
        // texture, write the warped one, then blit to the swapchain.
        // (A fragment-shader version needs either sampled textures —
        // SDL_gpu wants combined image-samplers naga can't emit — or
        // fragment storage reads, an optional Vulkan feature that
        // segfaults WSLg's Dozen driver. Compute storage is core.)
        let pipe_post = device
            .create_compute_pipeline()
            .with_code(
                ShaderFormat::SPIRV,
                include_bytes!(concat!(env!("OUT_DIR"), "/cs_post.spv")),
            )
            .with_entrypoint(c"cs_post")
            .with_readonly_storage_textures(1)
            .with_readwrite_storage_textures(1)
            .with_uniform_buffers(1)
            .with_thread_count(8, 8, 1)
            .build()
            .map_err(|e| e.to_string())?;

        // The HUD/text pass: same compute shape as the panini pass — one
        // read-only glyph texture in, the screen image read-write out.
        let pipe_text = device
            .create_compute_pipeline()
            .with_code(
                ShaderFormat::SPIRV,
                include_bytes!(concat!(env!("OUT_DIR"), "/cs_text.spv")),
            )
            .with_entrypoint(c"cs_text")
            .with_readonly_storage_textures(1)
            .with_readwrite_storage_textures(1)
            .with_uniform_buffers(1)
            .with_thread_count(8, 8, 1)
            .build()
            .map_err(|e| e.to_string())?;

        let depth = device
            .create_texture(
                TextureCreateInfo::new()
                    .with_type(TextureType::_2D)
                    .with_width(width)
                    .with_height(height)
                    .with_layer_count_or_depth(1)
                    .with_num_levels(1)
                    .with_sample_count(SampleCount::NoMultiSampling)
                    .with_format(TextureFormat::D16Unorm)
                    .with_usage(TextureUsage::DEPTH_STENCIL_TARGET),
            )
            .map_err(|e| e.to_string())?;

        // A missing font isn't fatal — the renderer still draws, text is
        // simply skipped (each draw checks `text.font`-less paths via the
        // empty cache producing no glyphs). We try the system fonts.
        let font = Font::load_default()?;

        let fov_deg = 100.0;
        Ok(Renderer3d {
            device,
            pipe_cull,
            pipe_nocull,
            pipe_post,
            pipe_text,
            depth,
            scene: None,
            present: None,
            text: TextCache {
                font,
                glyphs: HashMap::new(),
                pending: Vec::new(),
            },
            width,
            height,
            fov_deg,
            panini: panini_for_fov(fov_deg),
            panini_squeeze: 1.0,
            fog_distance: 80.0,
            clear_color: (0.051, 0.059, 0.078),
        })
    }

    /// Allocate the screen-sized `present` image once. It is a color
    /// target (the panini-off scene renders straight into it), a
    /// simultaneous read/write compute storage image (the warp writes it,
    /// the text pass blends onto it), and a sampler source (blit reads
    /// it).
    fn ensure_present(&mut self) -> Option<()> {
        if self.present.is_some() {
            return Some(());
        }
        let tex = self
            .device
            .create_texture(
                TextureCreateInfo::new()
                    .with_type(TextureType::_2D)
                    .with_width(self.width)
                    .with_height(self.height)
                    .with_layer_count_or_depth(1)
                    .with_num_levels(1)
                    .with_sample_count(SampleCount::NoMultiSampling)
                    .with_format(TextureFormat::R8g8b8a8Unorm)
                    .with_usage(
                        TextureUsage::COLOR_TARGET
                            | TextureUsage::COMPUTE_STORAGE_SIMULTANEOUS_READ_WRITE
                            | TextureUsage::SAMPLER,
                    ),
            )
            .ok()?;
        self.present = Some(tex);
        Some(())
    }

    /// (Re)build the offscreen scene target for the current fov/panini.
    /// The source is rectilinear and must cover every angle the panini
    /// output shows (corners need the most); it is sized so its CENTER
    /// pixel density matches the output — panini compresses the
    /// periphery, so edges come out supersampled for free.
    fn ensure_scene(&mut self) -> Option<()> {
        let key = ((self.fov_deg * 16.0) as u32, (self.panini * 1024.0) as u32);
        if self.scene.as_ref().is_some_and(|sc| sc.key == key) {
            return Some(());
        }
        let (d, s) = (self.panini, self.panini_squeeze);
        let half = (self.fov_deg.to_radians() / 2.0).clamp(0.1, 1.45);
        let x_edge = panini_x(half, d);
        let y_edge = x_edge * self.height as f32 / self.width as f32;
        let (mut th, mut tv) = (0.0f32, 0.0f32);
        for i in 0..=32 {
            let t = i as f32 / 32.0;
            for (xp, yp) in [(x_edge, y_edge * t), (x_edge * t, y_edge)] {
                let (xr, yr) = panini_inverse(xp, yp, d, s);
                th = th.max(xr.abs());
                tv = tv.max(yr.abs());
            }
        }
        let (th, tv) = (th * 1.02, tv * 1.02);
        let w = ((self.width as f32 * th / x_edge).ceil() as u32).clamp(self.width, self.width * 2);
        let h =
            ((self.height as f32 * tv / y_edge).ceil() as u32).clamp(self.height, self.height * 2);
        let tex = |w, h, format, usage| {
            self.device
                .create_texture(
                    TextureCreateInfo::new()
                        .with_type(TextureType::_2D)
                        .with_width(w)
                        .with_height(h)
                        .with_layer_count_or_depth(1)
                        .with_num_levels(1)
                        .with_sample_count(SampleCount::NoMultiSampling)
                        .with_format(format)
                        .with_usage(usage),
                )
                .ok()
        };
        let color = tex(
            w,
            h,
            TextureFormat::R8g8b8a8Unorm,
            TextureUsage::COLOR_TARGET | TextureUsage::COMPUTE_STORAGE_READ,
        )?;
        let depth = tex(
            w,
            h,
            TextureFormat::D16Unorm,
            TextureUsage::DEPTH_STENCIL_TARGET,
        )?;
        self.scene = Some(SceneTarget {
            color,
            depth,
            src_tan: (th, tv),
            out_edge: (x_edge, y_edge),
            key,
        });
        Some(())
    }

    /// Set the horizontal FOV and ride the house fov→panini curve
    /// (`panini_for_fov`). Write the fields directly to decouple.
    pub fn set_fov(&mut self, fov_deg: f32) {
        self.fov_deg = fov_deg;
        self.panini = panini_for_fov(fov_deg);
    }

    /// Projection params for the shader: screen scales chosen so the
    /// horizontal screen edge lands exactly at fov/2 THROUGH the panini
    /// mapping (center pixels stay square: sy = sx * aspect), plus the
    /// 0..1 depth mapping. sy is negated — the Vulkan backend renders
    /// y-down otherwise; this is also why the cull pipeline's front
    /// face is clockwise.
    fn proj_params(&self) -> [f32; 4] {
        let d = self.panini;
        let half = self.fov_deg.to_radians() / 2.0;
        let edge = half.sin() * (d + 1.0) / (d + half.cos());
        let sx = 1.0 / edge;
        let sy = -(sx * self.width as f32 / self.height as f32);
        let (near, far) = (0.1_f32, 300.0_f32);
        [sx, sy, far / (far - near), -near * far / (far - near)]
    }

    /// Upload a static mesh (transfer buffer + copy pass).
    pub fn upload_mesh(&self, vertices: &[Vertex3]) -> Result<Mesh3, String> {
        let bytes = std::mem::size_of_val(vertices);
        let buffer = self
            .device
            .create_buffer()
            .with_size(bytes as u32)
            .with_usage(BufferUsageFlags::VERTEX)
            .build()
            .map_err(|e| e.to_string())?;
        let transfer = self
            .device
            .create_transfer_buffer()
            .with_size(bytes as u32)
            .with_usage(TransferBufferUsage::UPLOAD)
            .build()
            .map_err(|e| e.to_string())?;
        let mut map = transfer.map::<Vertex3>(&self.device, true);
        map.mem_mut()[..vertices.len()].copy_from_slice(vertices);
        map.unmap();
        let commands = self
            .device
            .acquire_command_buffer()
            .map_err(|e| e.to_string())?;
        let copy = self
            .device
            .begin_copy_pass(&commands)
            .map_err(|e| e.to_string())?;
        copy.upload_to_gpu_buffer(
            TransferBufferLocation::new()
                .with_offset(0)
                .with_transfer_buffer(&transfer),
            BufferRegion::new()
                .with_offset(0)
                .with_size(bytes as u32)
                .with_buffer(&buffer),
            true,
        );
        self.device.end_copy_pass(copy);
        commands.submit().map_err(|e| e.to_string())?;
        Ok(Mesh3 {
            buffer,
            count: vertices.len() as u32,
        })
    }

    /// Begin a frame: clear color + depth, ready to draw. Returns None
    /// when a target isn't available (minimized etc). Every frame resolves
    /// into the screen-sized `present` image — the scene renders there
    /// directly (panini off) or into the oversized scene target that the
    /// warp resolves there (panini on) — then the text pass composites the
    /// HUD and the result blits to the swapchain. The frame submits on
    /// drop (the swapchain is acquired only then, after all work is
    /// recorded).
    pub fn frame<'a>(
        &'a mut self,
        window: &'a Window,
        view: Mat4,
        light_world: Vec3,
    ) -> Option<Frame3<'a>> {
        let use_post = self.panini > 1e-3;
        self.ensure_present()?;
        if use_post {
            self.ensure_scene()?;
        }
        let commands = self.device.acquire_command_buffer().ok()?;
        let (r, g, b) = self.clear_color;
        let clear = Color::RGB((r * 255.0) as u8, (g * 255.0) as u8, (b * 255.0) as u8);
        let depth_part = |t: DepthStencilTargetInfo| {
            t.with_cycle(true)
                .with_clear_depth(1.0)
                .with_load_op(LoadOp::CLEAR)
                .with_store_op(StoreOp::STORE)
                .with_stencil_load_op(LoadOp::DONT_CARE)
                .with_stencil_store_op(StoreOp::DONT_CARE)
        };
        let (pass, proj) = if use_post {
            let scene = self.scene.as_mut().unwrap();
            let color_targets = [ColorTargetInfo::default()
                .with_texture(&scene.color)
                .with_load_op(LoadOp::CLEAR)
                .with_store_op(StoreOp::STORE)
                .with_clear_color(clear)];
            let depth_target =
                depth_part(DepthStencilTargetInfo::new()).with_texture(&mut scene.depth);
            let pass = self
                .device
                .begin_render_pass(&commands, &color_targets, Some(&depth_target))
                .ok()?;
            // Rectilinear source projection covering the panini output.
            let (near, far) = (0.1_f32, 300.0_f32);
            let proj = [
                1.0 / scene.src_tan.0,
                -(1.0 / scene.src_tan.1),
                far / (far - near),
                -near * far / (far - near),
            ];
            (pass, proj)
        } else {
            // No panini: render straight into `present` (screen-sized).
            let present = self.present.as_ref().unwrap();
            let color_targets = [ColorTargetInfo::default()
                .with_texture(present)
                .with_load_op(LoadOp::CLEAR)
                .with_store_op(StoreOp::STORE)
                .with_clear_color(clear)];
            let depth_target =
                depth_part(DepthStencilTargetInfo::new()).with_texture(&mut self.depth);
            let pass = self
                .device
                .begin_render_pass(&commands, &color_targets, Some(&depth_target))
                .ok()?;
            (pass, self.proj_params())
        };
        let warp = if use_post {
            let scene = self.scene.as_ref().unwrap();
            Some(WarpInfo {
                pipe: &self.pipe_post,
                scene_color: &scene.color,
                u: PostU {
                    out_scale: [
                        scene.out_edge.0,
                        scene.out_edge.1,
                        self.panini,
                        self.panini_squeeze,
                    ],
                    src_scale: [
                        scene.src_tan.0,
                        scene.src_tan.1,
                        self.width as f32,
                        self.height as f32,
                    ],
                },
            })
        } else {
            None
        };
        let light_view = view.transform_dir(light_world.norm());
        Some(Frame3 {
            device: &self.device,
            pipe_cull: &self.pipe_cull,
            pipe_nocull: &self.pipe_nocull,
            commands: Some(commands),
            pass: Some(pass),
            view,
            proj,
            light: [light_view.x, light_view.y, light_view.z, self.fog_distance],
            fog_color: [r, g, b, 1.0],
            bound_cull: None,
            present: self.present.as_ref().unwrap(),
            warp,
            text_pipe: &self.pipe_text,
            text: &mut self.text,
            window,
            out_w: self.width,
            out_h: self.height,
            cmds: Vec::new(),
        })
    }
}

/// One frame in flight: draw meshes and HUD text, then drop to submit.
pub struct Frame3<'a> {
    device: &'a Device,
    pipe_cull: &'a GraphicsPipeline,
    pipe_nocull: &'a GraphicsPipeline,
    commands: Option<CommandBuffer>,
    pass: Option<RenderPass>,
    view: Mat4,
    proj: [f32; 4],
    light: [f32; 4],
    fog_color: [f32; 4],
    bound_cull: Option<bool>,
    /// The screen-sized image everything resolves into before the blit.
    present: &'a Texture<'static>,
    /// The panini warp inputs, present only when panini is on.
    warp: Option<WarpInfo<'a>>,
    text_pipe: &'a ComputePipeline,
    text: &'a mut TextCache,
    window: &'a Window,
    out_w: u32,
    out_h: u32,
    /// HUD glyphs queued this frame, composited at drop.
    cmds: Vec<TextCmd>,
}

/// The drop-time panini warp inputs (`scene_color` -> `present`).
struct WarpInfo<'a> {
    pipe: &'a ComputePipeline,
    scene_color: &'a Texture<'static>,
    u: PostU,
}

impl Frame3<'_> {
    /// Draw `mesh` with `model`, vertex colors multiplied by `tint`
    /// (rgb; pass white for none). `cull = false` for open surfaces
    /// that must be visible from both sides.
    pub fn draw(&mut self, mesh: &Mesh3, model: Mat4, tint: (f32, f32, f32), cull: bool) {
        let (Some(commands), Some(pass)) = (&self.commands, &self.pass) else {
            return;
        };
        if self.bound_cull != Some(cull) {
            pass.bind_graphics_pipeline(if cull {
                self.pipe_cull
            } else {
                self.pipe_nocull
            });
            self.bound_cull = Some(cull);
        }
        let mv = self.view * model;
        let u = Uniforms {
            mv: mv.0,
            proj: self.proj,
            light: self.light,
            tint: [tint.0, tint.1, tint.2, 1.0],
            fog_color: self.fog_color,
        };
        commands.push_vertex_uniform_data(0, &u);
        pass.bind_vertex_buffers(
            0,
            &[BufferBinding::new()
                .with_buffer(&mesh.buffer)
                .with_offset(0)],
        );
        pass.draw_primitives(mesh.count as usize, 1, 0, 0);
    }

    /// Queue a line of screen-space HUD text with its top-left at (x, y),
    /// `px` tall, in `color`. Composited over the final image at drop.
    /// Returns the advance width (so callers can lay out following text).
    /// Glyphs rasterize once and cache; whitespace costs nothing.
    pub fn text(&mut self, s: &str, x: f32, y: f32, px: f32, color: (u8, u8, u8)) -> f32 {
        let col = [
            color.0 as f32 / 255.0,
            color.1 as f32 / 255.0,
            color.2 as f32 / 255.0,
            1.0,
        ];
        let baseline = y + self.text.font.ascent(px);
        let mut pen = x;
        for ch in s.chars() {
            let g = self.text.ensure_glyph(self.device, ch, px);
            if let Some(tex) = &g.tex {
                self.cmds.push(TextCmd {
                    tex: tex.clone(),
                    rect: [pen + g.xmin, baseline + g.top, 0.0, 0.0],
                    color: col,
                    w: g.w,
                    h: g.h,
                });
            }
            pen += g.advance;
        }
        pen - x
    }

    /// Width `s` would occupy at `px` without drawing — for centering and
    /// right-alignment (e.g. the score number at top-middle).
    pub fn text_width(&self, s: &str, px: f32) -> f32 {
        self.text.font.measure(s, px)
    }
}

impl Drop for Frame3<'_> {
    fn drop(&mut self) {
        let (Some(pass), Some(mut commands)) = (self.pass.take(), self.commands.take()) else {
            return;
        };
        self.device.end_render_pass(pass);

        // 1. Upload glyphs rasterized this frame, before the compute
        // passes that sample them. The transfer buffers must outlive the
        // submit, so they live in `transfers` until the end.
        let pending = std::mem::take(&mut self.text.pending);
        let mut transfers = Vec::new();
        let copy = if pending.is_empty() {
            None
        } else {
            self.device.begin_copy_pass(&commands).ok()
        };
        if let Some(copy) = copy {
            for gph in &pending {
                let Ok(tb) = self
                    .device
                    .create_transfer_buffer()
                    .with_size(gph.rgba.len() as u32)
                    .with_usage(TransferBufferUsage::UPLOAD)
                    .build()
                else {
                    continue;
                };
                let mut map = tb.map::<u8>(self.device, false);
                map.mem_mut()[..gph.rgba.len()].copy_from_slice(&gph.rgba);
                map.unmap();
                copy.upload_to_gpu_texture(
                    TextureTransferInfo::new()
                        .with_transfer_buffer(&tb)
                        .with_offset(0)
                        .with_pixels_per_row(gph.w)
                        .with_rows_per_layer(gph.h),
                    TextureRegion::new()
                        .with_texture(&gph.tex)
                        .with_width(gph.w)
                        .with_height(gph.h)
                        .with_depth(1),
                    false,
                );
                transfers.push(tb);
            }
            self.device.end_copy_pass(copy);
        }

        // 2. Panini warp: scene.color -> present (skipped when off).
        if let Some(warp) = self.warp.take() {
            let rw = [StorageTextureReadWriteBinding::new().with_texture(self.present)];
            if let Ok(cp) = self.device.begin_compute_pass(&commands, &rw, &[]) {
                cp.bind_compute_pipeline(warp.pipe);
                cp.bind_compute_storage_textures(0, std::slice::from_ref(warp.scene_color));
                commands.push_compute_uniform_data(0, &warp.u);
                cp.dispatch(self.out_w.div_ceil(8), self.out_h.div_ceil(8), 1);
                self.device.end_compute_pass(cp);
            }
        }

        // 3. HUD: alpha-blend each queued glyph onto present. One compute
        // pass, one dispatch per glyph (rebinding the glyph texture).
        if !self.cmds.is_empty() {
            let rw = [StorageTextureReadWriteBinding::new().with_texture(self.present)];
            if let Ok(cp) = self.device.begin_compute_pass(&commands, &rw, &[]) {
                cp.bind_compute_pipeline(self.text_pipe);
                for cmd in &self.cmds {
                    cp.bind_compute_storage_textures(0, std::slice::from_ref(&cmd.tex));
                    commands.push_compute_uniform_data(
                        0,
                        &TextU {
                            rect: cmd.rect,
                            color: cmd.color,
                        },
                    );
                    cp.dispatch(cmd.w.div_ceil(8), cmd.h.div_ceil(8), 1);
                }
                self.device.end_compute_pass(cp);
            }
        }

        // 4. Blit present -> swapchain, acquired only now that the frame's
        // work is recorded (the one point that can block on the
        // compositor; if it fails — minimized — drop the frame).
        let blit = match commands.wait_and_acquire_swapchain_texture(self.window) {
            Ok(swapchain) => Some(
                BlitInfo::default()
                    .with_source_texture(self.present)
                    .with_source_region(0, 0, 0, self.out_w, self.out_h)
                    .with_destination_texture(&swapchain)
                    .with_destination_region(0, 0, 0, swapchain.width(), swapchain.height())
                    .with_load_op(LoadOp::DONT_CARE)
                    .with_filter(Filter::Nearest),
            ),
            Err(_) => None,
        };
        if let Some(blit) = blit {
            commands.blit_texture(blit);
        }
        let _ = commands.submit();
        drop(transfers);
    }
}

/// Bake world-space triangles (CCW from outside) into standard
/// vertices: per-face normal, one color.
pub fn bake(tris: &[[Vec3; 3]], color: (f32, f32, f32)) -> Vec<Vertex3> {
    let mut out = Vec::with_capacity(tris.len() * 3);
    for t in tris {
        let n = (t[1] - t[0]).cross(t[2] - t[0]).norm();
        for p in t {
            out.push(Vertex3 {
                pos: [p.x, p.y, p.z],
                normal: [n.x, n.y, n.z],
                color: [color.0, color.1, color.2],
            });
        }
    }
    out
}

/// Axis-aligned box from `min` to `max` as 12 CCW triangles — the
/// workhorse primitive (cars, crates, walls...).
pub fn box_tris(min: Vec3, max: Vec3) -> Vec<[Vec3; 3]> {
    let v = |x, y, z| {
        vec3(
            if x == 0 { min.x } else { max.x },
            if y == 0 { min.y } else { max.y },
            if z == 0 { min.z } else { max.z },
        )
    };
    let quads: [[Vec3; 4]; 6] = [
        [v(0, 0, 0), v(0, 1, 0), v(1, 1, 0), v(1, 0, 0)], // -z
        [v(1, 0, 1), v(1, 1, 1), v(0, 1, 1), v(0, 0, 1)], // +z
        [v(0, 0, 1), v(0, 1, 1), v(0, 1, 0), v(0, 0, 0)], // -x
        [v(1, 0, 0), v(1, 1, 0), v(1, 1, 1), v(1, 0, 1)], // +x
        [v(0, 0, 1), v(0, 0, 0), v(1, 0, 0), v(1, 0, 1)], // -y
        [v(0, 1, 0), v(0, 1, 1), v(1, 1, 1), v(1, 1, 0)], // +y
    ];
    quads
        .iter()
        .flat_map(|q| [[q[0], q[1], q[2]], [q[0], q[2], q[3]]])
        .collect()
}

/// Checkerboard ground on y=0, `half` cells in each direction from the
/// origin, `cell` units per cell, alternating the two colors.
pub fn checker_ground(
    half: i32,
    cell: f32,
    a: (f32, f32, f32),
    b: (f32, f32, f32),
) -> Vec<Vertex3> {
    let mut out = Vec::new();
    for gx in -half..half {
        for gz in -half..half {
            let (x, z) = (gx as f32 * cell, gz as f32 * cell);
            let c = if (gx + gz).rem_euclid(2) == 0 { a } else { b };
            let (p0, p1, p2, p3) = (
                vec3(x, 0.0, z),
                vec3(x + cell, 0.0, z),
                vec3(x + cell, 0.0, z + cell),
                vec3(x, 0.0, z + cell),
            );
            out.extend(bake(&[[p0, p3, p2], [p0, p2, p1]], c));
        }
    }
    out
}
