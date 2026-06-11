//! Dylib mod loading with mtime hot-reload, built on the module system:
//! a mod IS a module whose init lives in a shared library.
//!
//! Contract (pinned-toolchain, like game-version-pinned mods): the mod
//! must be compiled by the same rustc, against the same `pm` (and any
//! shared game crate) as the host — in practice, built from the same
//! workspace. `MOD_ABI` is a coarse tripwire for stale binaries, not a
//! security boundary. A mod exports two symbols:
//!
//! ```ignore
//! #[unsafe(no_mangle)]
//! pub extern "C" fn pm_mod_abi() -> u64 { pm::MOD_ABI }
//!
//! #[unsafe(no_mangle)]
//! pub extern "C" fn pm_mod_init(pm: &mut Pm) -> bool {
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

/// Bump when kernel/module ABI-relevant surfaces change. Mods built
/// against another value are refused with a clear error.
pub const MOD_ABI: u64 = 0x706d_0003;

type AbiFn = unsafe extern "C" fn() -> u64;
type InitFn = unsafe extern "C" fn(&mut Pm) -> bool;

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
            if abi != MOD_ABI {
                eprintln!(
                    "pm: mod '{}' ABI {abi:#x} != host {MOD_ABI:#x} — rebuild it against this pm",
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
            let result = pm.module_add(&entry.name, |pm| -> Result<(), crate::TaskError> {
                if unsafe { init(pm) } { Ok(()) } else { Err("pm_mod_init returned false".into()) }
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
