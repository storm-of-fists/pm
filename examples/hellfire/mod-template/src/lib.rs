//! Starter pm mod. Copy this crate, rename it, and edit `pm_mod_init`.
//! It builds and hot-reloads into a running hellfire server exactly like
//! `meteor` does.
//!
//! The three exports below are the whole contract. The first two are
//! boilerplate you should not change — they are how the loader proves the
//! mod links the same `pm` the host does (a hard requirement: the mod
//! resolves host types like `Monster` by TypeId, which is only stable
//! within one compiler/profile). `pm_mod_init` is your code.
//!
//! Build (joint selection + matching profile, in-tree):
//!   cargo build --release -p mod_template -p hellfire
//! Out of tree, build against the values the server prints on startup;
//! the workspace `rust-toolchain.toml` pins the compiler for you.

use hellfire_core::{H, Monster, MonsterSrv, W};
use pm::{Pm, Rng, vec2};

/// ABI handshake — leave as-is. The loader compares this to its own; a
/// mismatch means the mod was built against a different pm and is
/// refused (with a manifest diff) rather than crashing in `pool()`.
#[unsafe(no_mangle)]
pub extern "C-unwind" fn pm_mod_abi() -> u64 {
    pm::mod_abi()
}

/// Build manifest — leave as-is. Lets the loader tell the author *which*
/// knob (rustc, profile, target) differs when the ABI check fails.
#[unsafe(no_mangle)]
pub extern "C-unwind" fn pm_mod_manifest() -> *const std::os::raw::c_char {
    pm::mod_manifest_ptr()
}

/// Your mod. Register tasks and touch pools just like the host's `main`.
/// Return `false` to signal init failure. Use `task_add_or_replace` so a
/// hot-reload swaps the task body in place instead of stacking copies;
/// pools persist across reloads, so entity state survives a rebuild.
#[unsafe(no_mangle)]
pub extern "C-unwind" fn pm_mod_init(pm: &mut Pm) -> bool {
    let monster = pm.pool::<Monster>("monster");
    let monster_srv = pm.pool::<MonsterSrv>("monster_srv");
    let mut rng = Rng::new(0x1234);
    eprintln!("[mod_template] loaded");

    // Example: spawn one wandering monster at the arena center every 5 s.
    pm.task_add_or_replace("mod_template_spawn", 32.0, 5.0, move |pm| {
        let id = pm.id_add();
        let pos = vec2(W * 0.5, H * 0.5);
        monster.get_mut().add(
            id,
            Monster {
                pos,
                vel: vec2(rng.rfr(-1.0, 1.0), rng.rfr(-1.0, 1.0)).norm() * 80.0,
                size: 12.0,
                color: [120, 200, 255, 255],
            },
        );
        monster_srv
            .get_mut()
            .add(id, MonsterSrv { shoot_timer: 2.0 });
    });
    true
}
