//! Headless sim: 100k entities integrating position from velocity for 600
//! ticks. The perf sanity check — the README's ns-per-entity-update number
//! comes from here.
//!
//!     cargo run --release -p pm --example sim

use std::time::Instant;

use pm::Pm;

#[derive(Default, Clone, Copy)]
struct Pos {
    x: f32,
    y: f32,
}

#[derive(Default, Clone, Copy)]
struct Vel {
    x: f32,
    y: f32,
}

#[derive(Default)]
struct Sim {
    ticks_left: u32,
}

const ENTITIES: u32 = 100_000;
const TICKS: u32 = 600;

fn main() {
    let mut pm = Pm::new();
    pm.loop_rate = 0; // run flat out

    let pos = pm.pool::<Pos>("pos");
    let vel = pm.pool::<Vel>("vel");
    let sim = pm.single::<Sim>("sim");
    sim.get_mut().ticks_left = TICKS;

    for i in 0..ENTITIES {
        // 100k exceeds the 65k-per-peer index budget; spread across two.
        let id = pm.id_add_for((i / 60_000) as u8);
        pos.get_mut().add(id, Pos::default());
        vel.get_mut().add(
            id,
            Vel {
                x: (i % 7) as f32,
                y: (i % 3) as f32,
            },
        );
    }

    // The join: iterate one pool densely, look the other up by id —
    // each_with does exactly that (callback style; see pool.rs for why
    // a streaming two-Mut iterator can't exist).
    pm.task_add("physics", 30.0, 0.0, {
        let pos = pos.clone();
        let vel = vel.clone();
        move |pm| {
            let dt = pm.loop_dt();
            vel.get_mut().each_with(&mut pos.get_mut(), |_, v, mut p| {
                p.x += v.x * dt;
                p.y += v.y * dt;
            });
        }
    });

    pm.task_add("control", 100.0, 0.0, {
        let sim = sim.clone();
        move |pm| {
            let mut sim = sim.get_mut();
            sim.ticks_left -= 1;
            if sim.ticks_left == 0 {
                pm.loop_quit();
            }
        }
    });

    let start = Instant::now();
    pm.loop_run();
    let elapsed = start.elapsed();

    let ops = ENTITIES as u64 * TICKS as u64;
    println!(
        "{} entities x {} ticks in {:.1} ms — {:.2} ns per entity-update (join: each_with)",
        ENTITIES,
        TICKS,
        elapsed.as_secs_f64() * 1e3,
        elapsed.as_secs_f64() * 1e9 / ops as f64,
    );
    let p = pos.get();
    let sample = p.values()[1];
    println!("sample pos[1] = ({:.1}, {:.1})", sample.x, sample.y);
}
