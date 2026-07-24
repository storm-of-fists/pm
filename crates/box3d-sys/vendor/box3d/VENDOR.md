# Vendored Box3D

- Upstream: https://github.com/erincatto/box3d
- Commit: `c52908c9a907714e4d3a8a30be5272a1761158e1` ("Fixes 08 (#98)")
- Vendored: 2026-07-23, license MIT (see LICENSE)
- Contents: `include/` + the library half of `src/` only — samples,
  benchmarks, tests, docs, and `extern/` (sokol, samples-only) are not
  vendored. `src/CMakeLists.txt` and `box3d.natvis` dropped; the build
  is `cc` over every `src/*.c` (see ../build.rs), C17, no defines —
  float precision (`BOX3D_DOUBLE_PRECISION` off), validation off.

To bump: clone upstream, check out the new pin, re-copy the same file
set, update this commit line, and re-run the crate tests — the
determinism re-run test and falling-box invariants are the smoke that
the new pin still behaves. pm code NEVER includes these headers
directly: everything goes through the `pmb3` shim (../src/pmb3.c), so
an upstream API change is a shim-sized diff, not a game-sized one.
