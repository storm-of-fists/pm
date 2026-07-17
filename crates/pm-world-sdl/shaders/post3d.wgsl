// post3d.wgsl — the panini pass, as a COMPUTE shader. The scene is
// rendered RECTILINEAR (wider source FOV) into an offscreen texture;
// this pass inverts the General Panini mapping per output pixel and
// resamples into `warped`, which then blits to the swapchain.
//
// SDL_gpu binding subtlety that cost a day of segfaults: a "read-only
// storage texture" slot is a SAMPLED IMAGE descriptor in SDL's Vulkan
// backend (SDL_gpu_vulkan.c: "because shaders are stupid") — declare
// it `texture_2d<f32>` and textureLoad it, NOT `texture_storage_2d<..,
// read>` (a real storage-image declaration mismatches the descriptor
// type and crashes the driver at dispatch). Read-WRITE slots are real
// storage images.
//
// Forward mapping (what the output represents):
//   phi    = azimuth in the x-z plane,  M = (d + 1) / (d + cos phi)
//   x_p    = M * sin phi
//   y_p    = tan_theta * mix(1 / cos phi, M, s)
// Inverse (this shader): given (x_p, y_p), with k = x_p / (d + 1):
//   phi    = atan(k) + asin(d * k / sqrt(1 + k^2))
//   x_rect = tan phi
//   y_rect = y_p / (mix(1 / cos phi, M, s) * cos phi)
//
// SDL_gpu SPIR-V convention for compute: set 0 = sampled textures then
// READ-ONLY storage textures; set 1 = READ-WRITE storage textures;
// set 2 = uniform buffers.

struct PostU {
    // x: panini-x at the horizontal screen edge, y: panini-y at the
    // vertical edge, z: panini distance d, w: vertical squeeze s
    out_scale: vec4<f32>,
    // x/y: rectilinear half-tangents the source texture covers,
    // z/w: output pixel dimensions
    src_scale: vec4<f32>,
}

@group(0) @binding(0)
var scene: texture_2d<f32>;

@group(1) @binding(0)
var warped: texture_storage_2d<rgba8unorm, write>;

@group(2) @binding(0)
var<uniform> u: PostU;

@compute @workgroup_size(8, 8, 1)
fn cs_post(@builtin(global_invocation_id) gid: vec3<u32>) {
    let out_dims = u.src_scale.zw;
    if (f32(gid.x) >= out_dims.x || f32(gid.y) >= out_dims.y) {
        return;
    }
    let dims = vec2<f32>(textureDimensions(scene));
    // Pixel center -> output NDC. Both passes share the same flipped-y
    // convention, so the sign flips cancel; positive scales throughout.
    let ndc = (vec2<f32>(gid.xy) + 0.5) / out_dims * 2.0 - 1.0;
    let xp = ndc.x * u.out_scale.x;
    let yp = ndc.y * u.out_scale.y;
    let d = u.out_scale.z;
    let s = u.out_scale.w;

    let k = xp / (d + 1.0);
    let phi = atan(k) + asin(d * k / sqrt(1.0 + k * k));
    let cphi = max(cos(phi), 1e-3);
    let m = (d + 1.0) / (d + cphi);
    let vert = mix(1.0 / cphi, m, s);
    let xr = tan(phi);
    let yr = yp / (vert * cphi);

    let suv = vec2<f32>(xr / u.src_scale.x, yr / u.src_scale.y) * 0.5 + 0.5;

    // Manual bilinear (storage textures have no sampler).
    let p = suv * dims - 0.5;
    let i0 = vec2<i32>(floor(p));
    let f = fract(p);
    let mx = vec2<i32>(dims) - 1;
    let zero = vec2<i32>(0);
    let c00 = textureLoad(scene, clamp(i0, zero, mx), 0);
    let c10 = textureLoad(scene, clamp(i0 + vec2<i32>(1, 0), zero, mx), 0);
    let c01 = textureLoad(scene, clamp(i0 + vec2<i32>(0, 1), zero, mx), 0);
    let c11 = textureLoad(scene, clamp(i0 + vec2<i32>(1, 1), zero, mx), 0);
    let color = mix(mix(c00, c10, f.x), mix(c01, c11, f.x), f.y);
    textureStore(warped, vec2<i32>(gid.xy), color);
}
