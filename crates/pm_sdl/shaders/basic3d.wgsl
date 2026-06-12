// basic3d.wgsl — pm_sdl::gpu3d's standard shader: vertex {pos, normal,
// color}, per-draw uniform {mvp, mv, light, tint}. Flat shading via
// per-face normals baked per vertex, distance fog toward the clear
// color. Games needing more bring their own pipeline.
//
// SDL_gpu SPIR-V convention: vertex-stage uniform buffers = set 1,
// binding = the slot passed to push_vertex_uniform_data.

struct Uniforms {
    mvp: mat4x4<f32>,    // proj * view * model
    mv: mat4x4<f32>,     // view * model
    light: vec4<f32>,    // view-space light dir (xyz); w = fog distance (0 = no fog)
    tint: vec4<f32>,     // multiplied with vertex color (rgb) ; w = fog rgb packed? no: unused
    fog_color: vec4<f32>,
}

@group(1) @binding(0)
var<uniform> u: Uniforms;

struct VsIn {
    @location(0) pos: vec3<f32>,
    @location(1) normal: vec3<f32>,
    @location(2) color: vec3<f32>,
}

struct VsOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) color: vec3<f32>,
}

@vertex
fn vs_main(in: VsIn) -> VsOut {
    var out: VsOut;
    out.clip = u.mvp * vec4<f32>(in.pos, 1.0);

    let n = normalize((u.mv * vec4<f32>(in.normal, 0.0)).xyz);
    let diffuse = 0.35 + 0.65 * max(dot(n, normalize(u.light.xyz)), 0.0);

    var fog = 1.0;
    if (u.light.w > 0.0) {
        let view_pos = (u.mv * vec4<f32>(in.pos, 1.0)).xyz;
        fog = clamp(1.0 - length(view_pos) / u.light.w, 0.25, 1.0);
    }

    out.color = mix(u.fog_color.rgb, in.color * u.tint.rgb * diffuse, fog);
    return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    return vec4<f32>(in.color, 1.0);
}
