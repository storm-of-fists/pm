//! Micro-bench: per-tick cost of the three ways a task can reach its
//! pool. Run with `cargo run --release --example taskbench`.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Instant;

use pm::{Pm, Pool, Task, TaskError};

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

struct FieldTask {
    pool: Rc<RefCell<Pool<P>>>,
    id: pm::Id,
}

impl Task for FieldTask {
    fn run(&mut self, _pm: &mut Pm) -> Result<(), TaskError> {
        self.pool.borrow_mut().get_mut(self.id).unwrap().x += 1.0;
        Ok(())
    }
}

fn main() {
    // Closure capturing the handle (fetched once at init).
    let mut pm = Pm::new();
    let pool = pm.pool_get::<P>("pos");
    let id = pm.id_add();
    pool.borrow_mut().add(id, P { x: 0.0 });
    pm.task_fn("work", 10.0, move |_| {
        pool.borrow_mut().get_mut(id).unwrap().x += 1.0;
    });
    bench("closure, captured handle", pm);

    // Struct task holding the handle as a field.
    let mut pm = Pm::new();
    let pool = pm.pool_get::<P>("pos");
    let id = pm.id_add();
    pool.borrow_mut().add(id, P { x: 0.0 });
    pm.task_add("work", 10.0, FieldTask { pool, id });
    bench("struct, handle field", pm);

    // The anti-pattern: hunt the pool by name every run.
    let mut pm = Pm::new();
    let pool = pm.pool_get::<P>("pos");
    let id = pm.id_add();
    pool.borrow_mut().add(id, P { x: 0.0 });
    drop(pool);
    pm.task_fn("work", 10.0, move |pm| {
        pm.pool_get::<P>("pos").borrow_mut().get_mut(id).unwrap().x += 1.0;
    });
    bench("pool_get every run", pm);
}
