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
//! security boundary. A mod exports two symbols
//! (`C-unwind` so a panic inside init unwinds back to the loader's
//! catch instead of aborting the process):
//!
//! ```ignore
//! #[unsafe(no_mangle)]
//! pub extern "C-unwind" fn pm_mod_abi() -> u64 { pm::mod_abi() }
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

type AbiFn = unsafe extern "C-unwind" fn() -> u64;
type InitFn = unsafe extern "C-unwind" fn(&mut Pm) -> bool;

struct ModEntry {
    path: PathBuf,
    /// Module name: file stem without the `lib` prefix.
    name: String,
    lib: Option<Library>,
    loaded_mtime: Option<SystemTime>,
}

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
                eprintln!(
                    "pm: mod '{}' ABI {abi:#x} != host {host_abi:#x} — it was built against \
                     a different pm (toolchain, profile, and the compiled pm crate must all \
                     match the host; rebuild the mod from this workspace with the host's \
                     profile).",
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
