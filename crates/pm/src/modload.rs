//! Dylib mod loading with mtime hot-reload. A mod is just a function
//! (`pm_mod_init`) living in a shared library — there is no "module"
//! concept in the kernel anymore; a mod registers tasks and pools
//! exactly like `main` does.
//!
//! Contract (pinned-toolchain, like game-version-pinned mods): the mod
//! must link the exact `pm` compilation the host links — same rustc,
//! same profile, and built in the SAME cargo invocation/selection as
//! the host (e.g. `cargo build -p <mod> -p <game>`): cargo resolves
//! features per selected-package graph, so even a featureless pm can
//! split into different units across separate `-p` invocations
//! (transitive metadata cascades), and different units mean different
//! `TypeId`s — the mod's `Monster` would not be the host's `Monster`.
//! `mod_abi()` hashes this build's `Pm` TypeId so a mismatched mod is
//! refused with a message instead of exploding in `pool()`. Not a
//! security boundary. To make that refusal *actionable*, a mod also
//! exports its build manifest (`pm_mod_manifest`), which the loader diffs
//! against the host's on mismatch so the author sees which knob is off
//! (rustc, profile, target) rather than two opaque hashes. The surest
//! way to match all of them is the workspace `rust-toolchain.toml` plus
//! the `mod-template/` starter, which pins the deps and profile.
//!
//! A mod exports three symbols (`C-unwind` so a panic inside init unwinds
//! back to the loader's catch instead of aborting the process):
//!
//! ```ignore
//! #[unsafe(no_mangle)]
//! pub extern "C-unwind" fn pm_mod_abi() -> u64 { pm::mod_abi() }
//!
//! #[unsafe(no_mangle)]
//! pub extern "C-unwind" fn pm_mod_manifest() -> *const std::os::raw::c_char {
//!     pm::mod_manifest_ptr()
//! }
//!
//! #[unsafe(no_mangle)]
//! pub extern "C-unwind" fn pm_mod_init(pm: &mut Pm) -> bool {
//!     // Register with task_add_or_replace so a reload swaps each task's
//!     // body in place instead of stacking duplicates. Pools persist
//!     // across reloads, so entity state survives a code change.
//!     pm.task_add_or_replace("my_mod", 30.0, 0.0, move |pm| { /* ... */ });
//!     true
//! }
//! ```
//!
//! Hot-reload model: on file change the loader loads the NEW library and
//! runs its init (which re-registers tasks via `task_add_or_replace`),
//! then **leaks the previous library** — keeps its code mapped rather
//! than `dlclose`-ing it. That's deliberate: a task the new init didn't
//! replace (renamed, removed) still holds a closure pointing into the
//! old code, and unmapping code a live task calls is UB. Leaking a few
//! MB per reload is the right trade for a dev-only feature; restart to
//! reclaim. Pools are never dropped, so state carries across reloads.

use std::path::PathBuf;
use std::time::SystemTime;

use libloading::Library;

use crate::kernel::Pm;

/// Bump when kernel/module ABI-relevant surfaces change.
pub const MOD_ABI: u64 = 0x706d_0004;

/// The full ABI word a mod must echo: the version constant mixed with a
/// hash of this build's `Pm` TypeId. TypeIds change whenever the pm
/// compilation changes — toolchain, profile, or feature set — which are
/// exactly the cases where the mod's types would silently not be the
/// host's types. Both sides compute this with their own compiled-in pm;
/// equality means the dylib really shares the host's crates.
pub fn mod_abi() -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::hash::DefaultHasher::new();
    std::any::TypeId::of::<Pm>().hash(&mut h);
    MOD_ABI ^ h.finish()
}

/// Human-readable record of the toolchain/profile/target that compiled
/// this `pm`, newline-joined `key=value` lines. Baked by `build.rs`. The
/// ABI hash is the gate; this is the diagnosis printed when it fails —
/// see [`build_manifest`] for the parsed form and the `pm_mod_manifest`
/// export below for how a mod surfaces its own.
pub const BUILD_MANIFEST: &str = concat!(
    "rustc=", env!("PM_RUSTC_VERSION"), "\n",
    "profile=", env!("PM_PROFILE"), "\n",
    "target=", env!("PM_TARGET"), "\n",
    "pm=", env!("CARGO_PKG_VERSION"),
);

/// [`BUILD_MANIFEST`] with a trailing NUL so its pointer is a valid C
/// string — what crosses the dylib boundary (a `*const c_char` touches
/// no Rust-layout type, so it's safe to call even from a mismatched mod).
const BUILD_MANIFEST_NUL: &str = concat!(
    "rustc=", env!("PM_RUSTC_VERSION"), "\n",
    "profile=", env!("PM_PROFILE"), "\n",
    "target=", env!("PM_TARGET"), "\n",
    "pm=", env!("CARGO_PKG_VERSION"), "\0",
);

/// Pointer a mod re-exports as `pm_mod_manifest` (mirroring how it
/// re-exports [`mod_abi`] as `pm_mod_abi`). Points into the caller's own
/// `pm`, so host and mod each report their own build.
pub fn mod_manifest_ptr() -> *const std::os::raw::c_char {
    BUILD_MANIFEST_NUL.as_ptr() as *const std::os::raw::c_char
}

/// This build's manifest as `(key, value)` pairs, in declared order.
pub fn build_manifest() -> Vec<(&'static str, &'static str)> {
    parse_manifest(BUILD_MANIFEST)
}

fn parse_manifest(s: &str) -> Vec<(&str, &str)> {
    s.lines().filter_map(|l| l.split_once('=')).collect()
}

/// Render a host-vs-mod manifest diff, marking lines that disagree. Used
/// only on an ABI mismatch, to turn "0xA != 0xB" into something a mod
/// author can act on.
fn manifest_diff(host: &str, mod_: &str) -> String {
    use std::fmt::Write;
    let (host, mod_) = (parse_manifest(host), parse_manifest(mod_));
    let mut out = String::new();
    for (key, hv) in &host {
        let mv = mod_.iter().find(|(k, _)| k == key).map(|(_, v)| *v);
        let mv = mv.unwrap_or("<absent>");
        let mark = if mv == *hv { "" } else { "   <- mismatch" };
        let _ = writeln!(out, "    {key:<8} host {hv:<28} mod {mv}{mark}");
    }
    out
}

type AbiFn = unsafe extern "C-unwind" fn() -> u64;
type InitFn = unsafe extern "C-unwind" fn(&mut Pm) -> bool;
type ManifestFn = unsafe extern "C-unwind" fn() -> *const std::os::raw::c_char;

struct ModEntry {
    path: PathBuf,
    /// Module name: file stem without the `lib` prefix.
    name: String,
    lib: Option<Library>,
    loaded_mtime: Option<SystemTime>,
}

// Today's dlopen'd cdylib is the Tier-0 "sharp knife": full `&mut Pm`,
// native speed, but the author must be a Rust dev on the host's exact
// toolchain (that's what mod_abi + the manifest enforce/diagnose). The
// roadmap below is about loosening BOTH constraints — and every tier past
// Tier 0 hits the same wall: nothing else can share `Pm`/pools by
// TypeId, so they all need the SAME prerequisite first.
//
// TODO(roadmap): host-API binding layer — the shared prerequisite. A
// stable, non-TypeId surface for "do gameplay" — pool access by name with
// typed get/set, id_add/id_del, task registration, time/tick — that any
// non-native mod calls. This is the real work; the language tiers below
// are thin backends over it.
//
// TODO(roadmap): store mods (Tier 1) — a mod as its own `Pm` + thread,
// handed only an `Arc<Store>` (see the threaded-stores note in kernel):
// crash isolation and safe unload, because nothing of the mod's is
// mapped into the host's address space the way today's cdylibs are. Still
// Rust, but isolated.
//
// TODO(roadmap): Lua tier (Tier 2, mlua) — scripted mods over the binding
// layer. Pragmatic "let players mod without Rust": tiny runtime, hot
// reload is just re-reading source, accessible. Dynamic typing + an FFI
// marshaling cost on hot loops are the tradeoffs.
//
// TODO(roadmap): wasm tier (Tier 3, wasmtime) — portable + sandboxed.
// Language-agnostic (Rust/C/AssemblyScript compile to it), real
// capability security for UNTRUSTED authors, near-native speed. Heaviest
// integration (data marshaling across the boundary, no shared pools).
// This is what a released engine with untrusted mods ultimately wants.
//
// TODO(roadmap): Python (pyo3) — only if a specific audience needs it.
// Heavy runtime, GIL, hard to sandbox; fine for editor/tooling scripting,
// poor for a per-tick gameplay loop. Lowest priority.
#[derive(Default)]
pub struct ModLoader {
    mods: Vec<ModEntry>,
}

fn mtime(path: &std::path::Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

impl ModLoader {
    pub fn new() -> Self {
        Self::default()
    }

    /// Watch a dylib path. Missing files are fine — the mod loads when
    /// the file appears (first `cargo build -p <mod>`).
    pub fn watch(&mut self, path: impl Into<PathBuf>) {
        let path = path.into();
        let name = path
            .file_stem()
            .map(|s| s.to_string_lossy().trim_start_matches("lib").to_string())
            .unwrap_or_else(|| "mod".into());
        self.mods.push(ModEntry {
            path,
            name,
            lib: None,
            loaded_mtime: None,
        });
    }

    /// Names of currently loaded mods.
    pub fn loaded(&self) -> impl Iterator<Item = &str> {
        self.mods
            .iter()
            .filter(|m| m.lib.is_some())
            .map(|m| m.name.as_str())
    }

    /// Check every watched path; (re)load the ones whose file changed.
    /// Call from a ~1 Hz task.
    pub fn poll(&mut self, pm: &mut Pm) {
        for entry in &mut self.mods {
            let now = mtime(&entry.path);
            if now.is_none() || now == entry.loaded_mtime {
                continue;
            }
            entry.loaded_mtime = now;

            let lib = match unsafe { Library::new(&entry.path) } {
                Ok(l) => l,
                Err(e) => {
                    eprintln!("pm: mod '{}' failed to open: {e}", entry.name);
                    continue;
                }
            };
            let abi = match unsafe { lib.get::<AbiFn>(b"pm_mod_abi") } {
                Ok(f) => unsafe { f() },
                Err(e) => {
                    eprintln!("pm: mod '{}' has no pm_mod_abi: {e}", entry.name);
                    continue;
                }
            };
            let host_abi = mod_abi();
            if abi != host_abi {
                // The hash already proves a mismatch; pull the mod's
                // build manifest (if it exports one) and diff it against
                // ours so the author sees which knob is off, not just two
                // hashes. A pre-manifest mod just gets the short form.
                let diff = unsafe { lib.get::<ManifestFn>(b"pm_mod_manifest") }
                    .ok()
                    .map(|f| unsafe { std::ffi::CStr::from_ptr(f()) })
                    .and_then(|c| c.to_str().ok())
                    .map(|mod_manifest| manifest_diff(BUILD_MANIFEST, mod_manifest))
                    .unwrap_or_default();
                eprintln!(
                    "pm: mod '{}' ABI {abi:#x} != host {host_abi:#x} — built against a \
                     different pm. Rebuild it with the host's exact toolchain and profile \
                     (use the workspace rust-toolchain.toml).\n{diff}",
                    entry.name
                );
                continue;
            }
            let init = match unsafe { lib.get::<InitFn>(b"pm_mod_init") } {
                Ok(f) => *f,
                Err(e) => {
                    eprintln!("pm: mod '{}' has no pm_mod_init: {e}", entry.name);
                    continue;
                }
            };
            // catch_unwind at the mod boundary (and only here — tasks
            // stay panic-loud): foreign init code must not take the host
            // down. There's no rollback now, so a failed init may leave
            // partial registration; the dev sees the message and fixes
            // the mod. Either way the just-loaded library is leaked, not
            // dropped — on failure it may already hold closures the host
            // now runs (UB to unmap); on success it replaces the old one.
            let ok = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe { init(pm) }));
            match ok {
                Ok(true) => {
                    // New code is live; keep the old library mapped for
                    // any task the new init didn't replace.
                    if let Some(old) = entry.lib.replace(lib) {
                        std::mem::forget(old);
                    }
                    eprintln!("pm: mod '{}' loaded", entry.name);
                }
                Ok(false) => {
                    std::mem::forget(lib);
                    eprintln!("pm: mod '{}' init returned false", entry.name);
                }
                Err(_) => {
                    std::mem::forget(lib);
                    eprintln!("pm: mod '{}' init panicked (see message above)", entry.name);
                }
            }
        }
    }
}
