Drop WAVs here to replace the synthesized placeholder sounds — picked up
at launch, no code change (see src/sfx.rs):

  shot.wav   a bullet is fired (every truck's, attenuated by distance)
  hit.wav    a bullet connected (hog flesh or building wall)
  kill.wav   a hog went down
  bite.wav   a hog rammed a truck
  boom.wav   a truck exploded (overheat or hp 0)

Any sample rate / channel count; u8, i16, i32, or f32 little-endian PCM.
