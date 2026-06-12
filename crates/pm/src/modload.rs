//! Dylib mod loading with mtime hot-reload, built on the module system:
//! a mod IS a module whose init lives in a shared library.
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
//!     // register tasks/pools exactly like any module init; return false to fail
//!     true
//! }
//! ```
//!
//! On file change the loader does `module_remove` (stopping the mod's
//! tasks — running their `end` hooks — and dropping its pools, so no
//! code or vtable from the old library survives), unloads, reloads, and
//! re-runs init. Drop the loader only after `module_remove`-ing or at
//! process exit.

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
        self.mods.push(ModEntry { path, name, lib: None, loaded_mtime: None });
    }

    /// Names of currently loaded mods.
    pub fn loaded(&self) -> impl Iterator<Item = &str> {
        self.mods.iter().filter(|m| m.lib.is_some()).map(|m| m.name.as_str())
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

            // Tear down the previous incarnation completely before
            // dlclose: every closure, Task vtable, and pool the mod
            // registered must die while its code is still mapped.
            if entry.lib.take().is_some() {
                pm.module_remove(&entry.name);
                eprintln!("pm: mod '{}' unloaded for reload", entry.name);
            }

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
            // stay panic-loud): foreign init code must not take the
            // host down. module_add rolls back partial registration.
            let result = pm.module_add(&entry.name, |pm| -> Result<(), crate::TaskError> {
                match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
                    init(pm)
                })) {
                    Ok(true) => Ok(()),
                    Ok(false) => Err("pm_mod_init returned false".into()),
                    Err(_) => Err("pm_mod_init panicked (see message above)".into()),
                }
            });
            match result {
                Ok(()) => {
                    entry.lib = Some(lib);
                    eprintln!("pm: mod '{}' loaded", entry.name);
                }
                Err(e) => eprintln!("pm: mod '{}' init failed: {e}", entry.name),
            }
        }
    }
}
