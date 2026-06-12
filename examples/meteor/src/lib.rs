//! Example hellfire mod: every few seconds, a meteor shower — a ring of
//! fast red monsters collapsing toward the middle of the arena.
//!
//! Built as a cdylib and loaded into the running server by
//! `pm::ModLoader` (see the server's "mods" task). Because this crate
//! links the same compiled `pm` and `hellfire_core` as the host, its
//! `Monster` IS the host's `Monster` — `pool("monster")` resolves to the
//! live replicated pool, and spawned meteors flow to every client
//! through the ordinary sync path. Edit, rebuild with the exact command
//! the server prints (`cargo build --release -p meteor -p hellfire` —
//! JOINT selection and matching profile, or cargo resolves a different
//! pm unit and the ABI check refuses the load), and watch it hot-swap.

use hellfire_core::{H, Monster, MonsterSrv, W};
use pm::{Pm, Rng, vec2};

#[unsafe(no_mangle)]
pub extern "C-unwind" fn pm_mod_abi() -> u64 {
    pm::mod_abi()
}

#[unsafe(no_mangle)]
pub extern "C-unwind" fn pm_mod_init(pm: &mut Pm) -> bool {
    let monster = pm.pool::<Monster>("monster");
    let monster_srv = pm.pool::<MonsterSrv>("monster_srv");
    let mut rng = Rng::new(0xE7E0);
    eprintln!("[meteor] shower armed: every 6 s");

    pm.task_add_every("meteor_shower", 32.0, 6.0, move |pm| {
        let center = vec2(W * 0.5, H * 0.5);
        let count = 22;
        for k in 0..count {
            let angle = k as f32 / count as f32 * std::f32::consts::TAU + rng.rfr(-0.1, 0.1);
            let radius = rng.rfr(380.0, 460.0);
            let pos = center + vec2(angle.cos(), angle.sin()) * radius;
            let speed = rng.rfr(130.0, 190.0);
            let id = pm.id_add();
            monster.borrow_mut().add(
                id,
                Monster {
                    pos,
                    vel: (center - pos).norm() * speed,
                    size: rng.rfr(10.0, 15.0),
                    color: [255, rng.rfr(40.0, 90.0) as u8, 30, 255],
                },
            );
            monster_srv.borrow_mut().add(id, MonsterSrv { shoot_timer: rng.rfr(1.0, 3.0) });
        }
        eprintln!("[meteor] shower of {count} at tick {}", pm.tick());
    });
    true
}
