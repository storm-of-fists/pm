# hellfire mod template

A minimal, working starter for a pm dylib mod. Copy this directory,
rename the crate, and edit `pm_mod_init` in `src/lib.rs`.

## The one rule

A mod is loaded **into the running host process** and resolves host types
(`Monster`, the `Pm` kernel, …) by `TypeId`. `TypeId` is only stable
within a single compilation, so the mod must be built with the **same
compiler, profile, and `pm`/`core` versions** as the host. Get any of
them wrong and the loader refuses the mod (it does not crash) and prints a
diff showing which one is off.

Three things keep you on the rails:

1. **`rust-toolchain.toml`** at the workspace root pins the compiler. Use
   the same one the engine ships, and rustc matches automatically.
2. The server prints its **build manifest** on startup
   (`[server] mods: build mods against:` …). Those are the exact values
   to match.
3. This template pins the **profile** (`[profile.release]`) and the
   `pm` / `hellfire_core` dependency versions.

## Build

In-tree (joint selection so cargo resolves one `pm` unit; matching
profile):

```bash
cargo build --release -p mod_template -p hellfire
```

The `.so` (`.dylib` / `.dll`) lands in `target/release/`, where the
server watches for it, and hot-reloads on each rebuild.

## Shipping mods for a released engine

When the engine is published, an out-of-tree mod author:

- copies this template,
- swaps the `path` deps in `Cargo.toml` for the published `version` /
  `git` deps,
- uses the engine's `rust-toolchain.toml`,
- and builds against the manifest the server prints.

The ABI handshake (`pm_mod_abi` + `pm_mod_manifest`) does the rest: a
mismatched build is refused with an actionable message instead of
undefined behavior.
