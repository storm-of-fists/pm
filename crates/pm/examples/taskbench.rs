//! Micro-bench: per-tick cost of the ways a task can reach pool data.
//! Run with `cargo run --release --example taskbench`.

use std::time::Instant;

use pm::{Pm, TaskError};

#[derive(Clone, Copy, PartialEq, Default)]
struct P {
    x: f32,
}

const TICKS: u32 = 1_000_000;

fn bench(label: &str, mut pm: Pm) {
    // Warmup, then measure.
    for _ in 0..10_000 {
        pm.loop_once(1.0 / 60.0);
    }
    let t = Instant::now();
    for _ in 0..TICKS {
        pm.loop_once(1.0 / 60.0);
    }
    let ns = t.elapsed().as_nanos() as f64 / TICKS as f64;
    println!("{label:28} {ns:7.1} ns/tick");
}

fn main() {
    // Whole-pool borrow + get_mut (the iteration pattern).
    let mut pm = Pm::new();
    let pool = pm.pool::<P>("p");
    let id = pm.id_add();
    pool.borrow_mut().add(id, P::default());
    pm.task_add("borrow", 1.0, {
        let pool = pool.clone();
        move |_| {
            pool.borrow_mut().get_mut(id).unwrap().x += 1.0;
        }
    });
    bench("borrow_mut + get_mut", pm);

    // Fallible per-entity access (`?` into the fault system).
    let mut pm = Pm::new();
    let pool = pm.pool::<P>("p");
    let id = pm.id_add();
    pool.borrow_mut().add(id, P::default());
    pm.task_add("try", 1.0, {
        let pool = pool.clone();
        move |_| -> Result<(), TaskError> {
            pool.try_mut(id)?.x += 1.0;
            Ok(())
        }
    });
    bench("try_mut (fallible)", pm);

    // Singleton access.
    let mut pm = Pm::new();
    let single = pm.single::<P>("p");
    pm.task_add("single", 1.0, {
        let single = single.clone();
        move |_| {
            single.borrow_mut().x += 1.0;
        }
    });
    bench("Single::borrow_mut", pm);

    // Empty task: scheduler floor.
    let mut pm = Pm::new();
    pm.task_add("empty", 1.0, |_| {});
    bench("empty task (floor)", pm);
}
