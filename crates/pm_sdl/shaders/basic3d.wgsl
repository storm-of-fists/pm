// basic3d.wgsl — pm_sdl::gpu3d's standard scene shader: vertex {pos,
// normal, color}, per-draw uniform {mv, proj params, light, tint}.
// Flat shading via per-face normals baked per vertex, distance fog
// toward the clear color. Games needing more bring their own pipeline.
//
// The projection here is plain rectilinear (clip assembled from view
// space: w = view z, so GPU clipping behaves exactly as with a
// matrix). pm's house panini look happens AFTERWARD as a post pass —
// see post3d.wgsl; warping per vertex smears triangles that cross the
// camera plane.
//
// SDL_gpu SPIR-V convention: vertex-stage uniform buffers = set 1,
// binding = the slot passed to push_vertex_uniform_data.

struct Uniforms {
    mv: mat4x4<f32>,       // view * model
    proj: vec4<f32>,       // x: sx, y: sy (y-flip baked: negative), z/w: depth A/B
    light: vec4<f32>,      // view-space light dir (xyz); w = fog distance (0 = no fog)
    tint: vec4<f32>,       // multiplied with vertex color (rgb)
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
    let vpos = (u.mv * vec4<f32>(in.pos, 1.0)).xyz;
    out.clip = vec4<f32>(
        vpos.x * u.proj.x,
        vpos.y * u.proj.y,
        u.proj.z * vpos.z + u.proj.w,
        vpos.z,
    );

    let n = normalize((u.mv * vec4<f32>(in.normal, 0.0)).xyz);
    let diffuse = 0.35 + 0.65 * max(dot(n, normalize(u.light.xyz)), 0.0);

    var fog = 1.0;
    if (u.light.w > 0.0) {
        fog = clamp(1.0 - length(vpos) / u.light.w, 0.25, 1.0);
    }

    out.color = mix(u.fog_color.rgb, in.color * u.tint.rgb * diffuse, fog);
    return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    return vec4<f32>(in.color, 1.0);
}
