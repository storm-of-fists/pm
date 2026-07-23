Game assets, picked up at launch — everything here has a built-in
fallback, so deleting any file just reverts to the synthesized/coded
placeholder.

MODELS (src/models.rs): truck.glb, heli.glb, hog.glb, flyer.glb — the
entity models, seeded from the code definitions by
`cargo run -p hogs -- genassets` and editable in Blender from then on.
Rules (see pm_sdl::model rustdoc): single .glb, vertex colors or solid
materials (NO textures), face +Z, 1 unit = 1 m, keep the part names
(body/cabin/barrel, body/tail/skid/rotor/gun, body/snout,
body/wing.l/wing.r) — a missing part rejects the file loudly and the
built-in wins. Relative shading is baked in vertex colors; peer/hp
tint multiplies on top at draw.

The magenta `collide.*` boxes are the HITBOXES — never drawn, judged
by the server: each box becomes a capsule (long footprint axis) +
altitude band (y extent). Reshape them and combat follows; delete one
and the file is rejected. Keep them axis-aligned in entity space, and
export with hidden objects INCLUDED if you park them in a hidden
collection.

SOUNDS (src/sfx.rs): drop WAVs to replace the synthesized placeholders:

  shot.wav   a bullet is fired (every truck's, attenuated by distance)
  hit.wav    a bullet connected (hog flesh or building wall)
  kill.wav   a hog went down
  bite.wav   a hog rammed a truck
  boom.wav   a truck exploded (overheat or hp 0)

Any sample rate / channel count; u8, i16, i32, or f32 little-endian PCM.
