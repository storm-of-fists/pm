// text.wgsl — screen-space text/HUD compositing as a COMPUTE pass.
// fontdue rasterizes each glyph on the CPU; the coverage is uploaded as
// an R8G8B8A8 texture, and this shader alpha-blends it over the final
// screen image (one dispatch per glyph, workgroup covering its box).
//
// Why compute and not a textured fragment quad: SDL_gpu's Vulkan backend
// binds fragment samplers as COMBINED image-samplers, which naga's
// SPIR-V backend can't emit — the same wall that made the panini pass
// (post3d.wgsl) a compute shader.
//
// Same SDL_gpu compute binding rules as post3d.wgsl: set 0 = read-only
// storage (declared `texture_2d<f32>`, a sampled-image descriptor —
// NOT `texture_storage_2d<.., read>`); set 1 = read-write storage (a
// real storage image); set 2 = uniforms.

struct TextU {
    rect: vec4<f32>,  // xy: dest top-left in pixels; zw: dest size
    color: vec4<f32>, // rgb tint; a scales coverage (fade / alpha)
}

@group(0) @binding(0)
var glyph: texture_2d<f32>;

@group(1) @binding(0)
var screen: texture_storage_2d<rgba8unorm, read_write>;

@group(2) @binding(0)
var<uniform> u: TextU;

@compute @workgroup_size(8, 8, 1)
fn cs_text(@builtin(global_invocation_id) gid: vec3<u32>) {
    // Dest size comes from the uniform, source coords clamp into the
    // texture: for glyphs the two match (1:1 texels), while a 1x1 white
    // texture stretches into any solid rectangle (`Frame3::rect`) — the
    // whole HUD is this one shader.
    if (gid.x >= u32(u.rect.z) || gid.y >= u32(u.rect.w)) {
        return;
    }
    let gdims = textureDimensions(glyph);
    let src = min(gid.xy, gdims - vec2<u32>(1u, 1u));
    let cov = textureLoad(glyph, vec2<i32>(src), 0).r * u.color.a;
    if (cov <= 0.0) {
        return;
    }
    let dst = vec2<i32>(i32(u.rect.x) + i32(gid.x), i32(u.rect.y) + i32(gid.y));
    let sdims = vec2<i32>(textureDimensions(screen));
    if (dst.x < 0 || dst.y < 0 || dst.x >= sdims.x || dst.y >= sdims.y) {
        return;
    }
    let bg = textureLoad(screen, dst);
    let rgb = mix(bg.rgb, u.color.rgb, cov);
    textureStore(screen, dst, vec4<f32>(rgb, 1.0));
}
