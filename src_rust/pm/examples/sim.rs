//! Headless sim: 100k entities integrating position from velocity for 600
//! ticks. Doubles as a perf sanity check against the C++ benchmark numbers
//! (`each()` trivial ~0.6 ns/op sequential on the reference machine).
//!
//!     cargo run --release --example sim

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

    let pos = pm.pool_get::<Pos>("pos");
    let vel = pm.pool_get::<Vel>("vel");
    let sim = pm.single::<Sim>("sim");
    sim.borrow_mut().ticks_left = TICKS;

    for i in 0..ENTITIES {
        // 100k exceeds the 65k-per-peer index budget; spread across two.
        let id = pm.id_add_for((i / 60_000) as u8);
        pos.borrow_mut().add(id, Pos::default());
        vel.borrow_mut().add(id, Vel { x: (i % 7) as f32, y: (i % 3) as f32 });
    }

    // Join pattern: iterate one pool, look the other up by id.
    pm.task_fn("physics", 30.0, {
        let pos = pos.clone();
        let vel = vel.clone();
        move |pm| {
            let dt = pm.loop_dt();
            let vel = vel.borrow();
            let mut pos = pos.borrow_mut();
            for (id, v) in vel.iter() {
                if let Some(mut p) = pos.get_mut(id) {
                    p.x += v.x * dt;
                    p.y += v.y * dt;
                }
            }
        }
    });

    pm.task_fn("control", 100.0, {
        let sim = sim.clone();
        move |pm| {
            let mut sim = sim.borrow_mut();
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
        "{} entities x {} ticks in {:.1} ms — {:.2} ns per entity-update (join: iter + get_mut)",
        ENTITIES,
        TICKS,
        elapsed.as_secs_f64() * 1e3,
        elapsed.as_secs_f64() * 1e9 / ops as f64,
    );
    let p = pos.borrow();
    let sample = p.values()[1];
    println!("sample pos[1] = ({:.1}, {:.1})", sample.x, sample.y);
}
