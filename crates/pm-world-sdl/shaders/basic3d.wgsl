// basic3d.wgsl — pm_sdl::gpu3d's standard scene shader: vertex {pos,
// normal, color}, per-draw vertex uniform {mv, proj, tint}, per-FRAME
// fragment uniform {sun, hemisphere ambient, point lights, fog}.
//
// Lighting is PER-PIXEL (the vertex stage only moves things): with
// per-face normals baked per vertex the flat-shaded look is identical
// to the old Gouraud path on small faces, but point lights now pool
// correctly on large surfaces (the arena ground is two triangles — a
// headlight between its vertices simply vanished per-vertex).
//
//   lit = hemisphere(n)  +  sun_color * max(n·sun_dir, 0)  +  Σ points
//   hemisphere(n) = mix(ground, sky, n·up * 0.5 + 0.5)
//   point i: atten = (1 - d/radius)^2, clamped — cheap, finite reach
//
// `tint.w` is the EMISSIVE flag (0 lit, 1 skips lighting entirely —
// tracers, muzzle flashes, blob shadows); fog still applies to both.
//
// The projection here is plain rectilinear (clip assembled from view
// space: w = view z, so GPU clipping behaves exactly as with a
// matrix). pm's house panini look happens AFTERWARD as a post pass —
// see post3d.wgsl; warping per vertex smears triangles that cross the
// camera plane.
//
// SDL_gpu SPIR-V convention: vertex-stage uniform buffers = set 1,
// fragment-stage = set 3; binding = the slot passed to
// push_vertex/fragment_uniform_data.

struct Uniforms {
    mv: mat4x4<f32>, // view * model
    proj: vec4<f32>, // x: sx, y: sy (y-flip baked: negative), z/w: depth A/B
    tint: vec4<f32>, // rgb multiplied with vertex color; w = emissive flag
}

// Everything the LIGHTS need, in VIEW space, pushed once per frame.
const MAX_LIGHTS: u32 = 8u;
struct LightU {
    sun_dir: vec4<f32>,   // xyz view-space (normalized); w = fog distance (0 = off)
    sun_color: vec4<f32>, // rgb
    sky: vec4<f32>,       // hemisphere ambient falling from above
    ground: vec4<f32>,    // hemisphere ambient bouncing from below
    up: vec4<f32>,        // world up in view space
    fog_color: vec4<f32>,
    // Point lights, two vec4 each: [pos.xyz (view), radius], [color.rgb, _].
    lights: array<vec4<f32>, 16>,
    counts: vec4<f32>,    // x: live light count
}

@group(1) @binding(0)
var<uniform> u: Uniforms;

@group(3) @binding(0)
var<uniform> lu: LightU;

struct VsIn {
    @location(0) pos: vec3<f32>,
    @location(1) normal: vec3<f32>,
    @location(2) color: vec3<f32>,
}

struct VsOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) color: vec3<f32>,
    @location(1) vpos: vec3<f32>,
    @location(2) normal: vec3<f32>,
    @location(3) emissive: f32,
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
    out.vpos = vpos;
    out.normal = (u.mv * vec4<f32>(in.normal, 0.0)).xyz;
    out.color = in.color * u.tint.rgb;
    out.emissive = u.tint.w;
    return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let n = normalize(in.normal);
    // Hemisphere ambient + one sun.
    let hemi = mix(
        lu.ground.rgb,
        lu.sky.rgb,
        dot(n, normalize(lu.up.xyz)) * 0.5 + 0.5,
    );
    var lit = hemi + lu.sun_color.rgb * max(dot(n, normalize(lu.sun_dir.xyz)), 0.0);
    // Point lights: quadratic-ish falloff with a hard finite radius.
    let count = u32(lu.counts.x);
    for (var i = 0u; i < count; i = i + 1u) {
        let p = lu.lights[i * 2u];
        let c = lu.lights[i * 2u + 1u];
        let l = p.xyz - in.vpos;
        let d = length(l);
        let atten = clamp(1.0 - d / max(p.w, 1e-3), 0.0, 1.0);
        lit = lit + c.rgb * (max(dot(n, l / max(d, 1e-4)), 0.0) * atten * atten);
    }
    var col = mix(in.color * lit, in.color, in.emissive);
    // Distance fog toward the horizon color (also applied to emissives:
    // a tracer at the fog wall should die into it like everything else).
    if (lu.sun_dir.w > 0.0) {
        let fog = clamp(1.0 - length(in.vpos) / lu.sun_dir.w, 0.25, 1.0);
        col = mix(lu.fog_color.rgb, col, fog);
    }
    return vec4<f32>(col, 1.0);
}
