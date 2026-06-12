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
//! Conventions: +y up, +z forward, depth 0..1. `projection()` bakes the
//! y-flip the Vulkan backend needs, which mirrors screen winding — the
//! culling pipeline therefore treats CLOCKWISE as front. Author meshes
//! CCW-from-outside and it all works out.

use pm::{Mat4, Vec3, vec3};
use sdl3::gpu::{
    Buffer, BufferBinding, BufferRegion, BufferUsageFlags, ColorTargetDescription,
    ColorTargetInfo, CommandBuffer, CompareOp, CullMode, DepthStencilState,
    DepthStencilTargetInfo, Device, FillMode, FrontFace, GraphicsPipeline,
    GraphicsPipelineTargetInfo, LoadOp, PrimitiveType, RasterizerState, RenderPass, SampleCount,
    ShaderFormat, ShaderStage, StoreOp, Texture, TextureCreateInfo, TextureFormat, TextureType,
    TextureUsage, TransferBufferLocation, TransferBufferUsage, VertexAttribute,
    VertexBufferDescription, VertexElementFormat, VertexInputRate, VertexInputState,
};
use sdl3::pixels::Color;
use sdl3::video::Window;

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
    mvp: [f32; 16],
    mv: [f32; 16],
    light: [f32; 4], // xyz dir (view space), w = fog distance
    tint: [f32; 4],
    fog_color: [f32; 4],
}

/// An uploaded static mesh: GPU buffer + vertex count.
pub struct Mesh3 {
    buffer: Buffer,
    count: u32,
}

pub struct Renderer3d {
    device: Device,
    pipe_cull: GraphicsPipeline,
    pipe_nocull: GraphicsPipeline,
    depth: Texture<'static>,
    width: u32,
    height: u32,
    /// Projection used by `frame` (rebuild with `set_projection`).
    proj: Mat4,
    /// Fog cutoff distance in world units; 0 disables. Default 80.
    pub fog_distance: f32,
    /// Clear / fog color (rgb 0..1).
    pub clear_color: (f32, f32, f32),
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

        let proj = Self::projection(70.0_f32.to_radians(), width as f32 / height as f32);
        Ok(Renderer3d {
            device,
            pipe_cull,
            pipe_nocull,
            depth,
            width,
            height,
            proj,
            fog_distance: 80.0,
            clear_color: (0.051, 0.059, 0.078),
        })
    }

    /// Perspective for SDL GPU's clip space: +z forward, depth 0..1,
    /// y NEGATED (the Vulkan backend renders y-down otherwise — this is
    /// also why the cull pipeline's front face is clockwise).
    pub fn projection(fov_y: f32, aspect: f32) -> Mat4 {
        let (near, far) = (0.1, 300.0);
        let f = 1.0 / (fov_y / 2.0).tan();
        let mut m = Mat4([0.0; 16]);
        m.0[0] = f / aspect;
        m.0[5] = -f;
        m.0[10] = far / (far - near);
        m.0[11] = 1.0;
        m.0[14] = -near * far / (far - near);
        m
    }

    pub fn set_fov(&mut self, fov_y: f32) {
        self.proj = Self::projection(fov_y, self.width as f32 / self.height as f32);
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
        let commands = self.device.acquire_command_buffer().map_err(|e| e.to_string())?;
        let copy = self.device.begin_copy_pass(&commands).map_err(|e| e.to_string())?;
        copy.upload_to_gpu_buffer(
            TransferBufferLocation::new().with_offset(0).with_transfer_buffer(&transfer),
            BufferRegion::new().with_offset(0).with_size(bytes as u32).with_buffer(&buffer),
            true,
        );
        self.device.end_copy_pass(copy);
        commands.submit().map_err(|e| e.to_string())?;
        Ok(Mesh3 { buffer, count: vertices.len() as u32 })
    }

    /// Begin a frame: acquire swapchain (blocks on vsync — let it pace
    /// the loop: `pm.loop_rate = 0`), clear color + depth. Returns None
    /// when the swapchain isn't available (minimized etc). The frame
    /// submits on drop.
    pub fn frame(&mut self, window: &Window, view: Mat4, light_world: Vec3) -> Option<Frame3<'_>> {
        let mut commands = self.device.acquire_command_buffer().ok()?;
        let Ok(swapchain) = commands.wait_and_acquire_swapchain_texture(window) else {
            commands.cancel();
            return None;
        };
        let (r, g, b) = self.clear_color;
        let color_targets = [ColorTargetInfo::default()
            .with_texture(&swapchain)
            .with_load_op(LoadOp::CLEAR)
            .with_store_op(StoreOp::STORE)
            .with_clear_color(Color::RGB(
                (r * 255.0) as u8,
                (g * 255.0) as u8,
                (b * 255.0) as u8,
            ))];
        let depth_target = DepthStencilTargetInfo::new()
            .with_texture(&mut self.depth)
            .with_cycle(true)
            .with_clear_depth(1.0)
            .with_load_op(LoadOp::CLEAR)
            .with_store_op(StoreOp::STORE)
            .with_stencil_load_op(LoadOp::DONT_CARE)
            .with_stencil_store_op(StoreOp::DONT_CARE);
        let pass = self
            .device
            .begin_render_pass(&commands, &color_targets, Some(&depth_target))
            .ok()?;
        let light_view = view.transform_dir(light_world.norm());
        Some(Frame3 {
            device: &self.device,
            pipe_cull: &self.pipe_cull,
            pipe_nocull: &self.pipe_nocull,
            commands: Some(commands),
            pass: Some(pass),
            view,
            proj: self.proj,
            light: [light_view.x, light_view.y, light_view.z, self.fog_distance],
            fog_color: [r, g, b, 1.0],
            bound_cull: None,
        })
    }
}

/// One frame in flight: draw meshes, then drop to submit.
pub struct Frame3<'a> {
    device: &'a Device,
    pipe_cull: &'a GraphicsPipeline,
    pipe_nocull: &'a GraphicsPipeline,
    commands: Option<CommandBuffer>,
    pass: Option<RenderPass>,
    view: Mat4,
    proj: Mat4,
    light: [f32; 4],
    fog_color: [f32; 4],
    bound_cull: Option<bool>,
}

impl Frame3<'_> {
    /// Draw `mesh` with `model`, vertex colors multiplied by `tint`
    /// (rgb; pass white for none). `cull = false` for open surfaces
    /// that must be visible from both sides.
    pub fn draw(&mut self, mesh: &Mesh3, model: Mat4, tint: (f32, f32, f32), cull: bool) {
        let (Some(commands), Some(pass)) = (&self.commands, &self.pass) else { return };
        if self.bound_cull != Some(cull) {
            pass.bind_graphics_pipeline(if cull { self.pipe_cull } else { self.pipe_nocull });
            self.bound_cull = Some(cull);
        }
        let mv = self.view * model;
        let u = Uniforms {
            mvp: (self.proj * mv).0,
            mv: mv.0,
            light: self.light,
            tint: [tint.0, tint.1, tint.2, 1.0],
            fog_color: self.fog_color,
        };
        commands.push_vertex_uniform_data(0, &u);
        pass.bind_vertex_buffers(
            0,
            &[BufferBinding::new().with_buffer(&mesh.buffer).with_offset(0)],
        );
        pass.draw_primitives(mesh.count as usize, 1, 0, 0);
    }
}

impl Drop for Frame3<'_> {
    fn drop(&mut self) {
        if let (Some(pass), Some(commands)) = (self.pass.take(), self.commands.take()) {
            self.device.end_render_pass(pass);
            let _ = commands.submit();
        }
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
    quads.iter().flat_map(|q| [[q[0], q[1], q[2]], [q[0], q[2], q[3]]]).collect()
}

/// Checkerboard ground on y=0, `half` cells in each direction from the
/// origin, `cell` units per cell, alternating the two colors.
pub fn checker_ground(half: i32, cell: f32, a: (f32, f32, f32), b: (f32, f32, f32)) -> Vec<Vertex3> {
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
