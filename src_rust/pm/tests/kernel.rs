use std::cell::RefCell;
use std::rc::Rc;

use pm::Pm;

#[derive(Default, Clone, Copy, PartialEq, Debug)]
struct Pos {
    x: f32,
    y: f32,
}

#[derive(Default)]
struct Hits(u32);

#[test]
fn state_is_a_singleton() {
    let mut pm = Pm::new();
    let a = pm.state_get::<Hits>("hits");
    a.borrow_mut().0 = 7;
    let b = pm.state_get::<Hits>("hits");
    assert_eq!(b.borrow().0, 7);
    assert!(Rc::ptr_eq(&a, &b));
}

#[test]
#[should_panic(expected = "different type")]
fn state_type_mismatch_panics() {
    let mut pm = Pm::new();
    let _ = pm.state_get::<Hits>("thing");
    let _ = pm.state_get::<Pos>("thing");
}

#[test]
fn tasks_run_in_priority_order() {
    let mut pm = Pm::new();
    let order = Rc::new(RefCell::new(Vec::new()));
    for (name, prio) in [("c", 30.0), ("a", 10.0), ("b", 20.0)] {
        let order = order.clone();
        pm.task_add(name, prio, move |_| order.borrow_mut().push(name));
    }
    pm.loop_once(1.0 / 60.0);
    assert_eq!(*order.borrow(), vec!["a", "b", "c"]);
}

#[test]
fn the_canonical_pattern_works() {
    // Fetch during init, capture the handle in the closure, mutate in the task.
    let mut pm = Pm::new();
    let pos = pm.pool_get::<Pos>("pos");

    let id = pm.id_add();
    pos.borrow_mut().add(id, Pos { x: 0.0, y: 0.0 });

    pm.task_add("physics", 30.0, {
        let pos = pos.clone();
        move |pm| {
            let dt = pm.loop_dt();
            for (_, mut p) in pos.borrow_mut().iter_mut() {
                p.x += 10.0 * dt;
            }
        }
    });

    pm.loop_once(0.5);
    pm.loop_once(0.5);
    assert_eq!(pos.borrow().get(id), Some(&Pos { x: 10.0, y: 0.0 }));
}

#[test]
fn id_remove_is_deferred_and_flushes_all_pools() {
    let mut pm = Pm::new();
    let pos = pm.pool_get::<Pos>("pos");
    let hp = pm.pool_get::<u32>("hp");

    let id = pm.id_add();
    pos.borrow_mut().add(id, Pos::default());
    hp.borrow_mut().add(id, 100);

    let seen_alive_after_remove = Rc::new(RefCell::new(false));
    pm.task_add("reaper", 10.0, {
        let seen = seen_alive_after_remove.clone();
        move |pm| {
            pm.id_remove(id);
            // Deferred: still alive within the same tick.
            *seen.borrow_mut() = pm.id_alive(id);
        }
    });

    pm.loop_once(1.0 / 60.0);
    assert!(*seen_alive_after_remove.borrow());
    assert!(!pm.id_alive(id));
    assert!(!pos.borrow().contains(id));
    assert!(!hp.borrow().contains(id));
}

#[test]
fn removed_ids_recycle_with_bumped_generation() {
    let mut pm = Pm::new();
    let pos = pm.pool_get::<Pos>("pos");

    let a = pm.id_add();
    pos.borrow_mut().add(a, Pos { x: 1.0, y: 0.0 });
    pm.id_remove(a);
    pm.loop_once(1.0 / 60.0); // flush: kill + (no peers) release

    let b = pm.id_add();
    assert_eq!(b.index(), a.index(), "index should recycle");
    assert_eq!(b.generation(), a.generation() + 1, "generation should bump");
    assert!(!pm.id_alive(a) && pm.id_alive(b));

    pos.borrow_mut().add(b, Pos { x: 2.0, y: 0.0 });
    assert_eq!(pos.borrow().get(a), None, "stale handle must miss");
    assert_eq!(pos.borrow().get(b), Some(&Pos { x: 2.0, y: 0.0 }));
}

#[test]
fn tick_advances_and_stamps_changes() {
    let mut pm = Pm::new();
    let hp = pm.pool_get::<u32>("hp");
    let id = pm.id_add();
    hp.borrow_mut().add(id, 100); // init-time add, stamped tick 1
    let init_tick = pm.tick();

    pm.task_add("damage", 10.0, {
        let hp = hp.clone();
        move |_| {
            if let Some(mut h) = hp.borrow_mut().get_mut(id) {
                *h -= 1;
            }
        }
    });
    pm.loop_once(1.0 / 60.0);
    pm.loop_once(1.0 / 60.0);

    assert_eq!(pm.tick(), init_tick + 2);
    assert_eq!(hp.borrow().changed_tick(id), Some(pm.tick()));
    // A peer that acked the first tick still sees the later change.
    assert_eq!(hp.borrow().changed_since(init_tick + 1).count(), 1);
    assert_eq!(hp.borrow().changed_since(pm.tick()).count(), 0);
}

#[test]
fn periodic_task_fires_once_per_interval() {
    let mut pm = Pm::new();
    let count = Rc::new(RefCell::new(0));
    pm.task_add_every("slow", 10.0, 1.0, {
        let count = count.clone();
        move |_| *count.borrow_mut() += 1
    });
    for _ in 0..10 {
        pm.loop_once(0.25); // 2.5 seconds total
    }
    assert_eq!(*count.borrow(), 2);
}

#[test]
fn task_add_and_stop_from_inside_a_task() {
    let mut pm = Pm::new();
    let log = Rc::new(RefCell::new(Vec::new()));

    pm.task_add("spawner", 10.0, {
        let log = log.clone();
        move |pm| {
            log.borrow_mut().push("spawner");
            let log = log.clone();
            pm.task_add("spawned", 5.0, move |_| log.borrow_mut().push("spawned"));
            pm.task_stop("spawner");
        }
    });

    pm.loop_once(1.0 / 60.0); // spawner runs, adds "spawned", stops itself
    pm.loop_once(1.0 / 60.0); // only "spawned" runs
    pm.loop_once(1.0 / 60.0);
    assert_eq!(*log.borrow(), vec!["spawner", "spawned", "spawned"]);
}

#[test]
fn quit_stops_loop_run() {
    let mut pm = Pm::new();
    pm.loop_rate = 0; // uncapped, finishes instantly
    let ticks = Rc::new(RefCell::new(0));
    pm.task_add("control", 0.0, {
        let ticks = ticks.clone();
        move |pm| {
            *ticks.borrow_mut() += 1;
            if *ticks.borrow() == 3 {
                pm.loop_quit();
            }
        }
    });
    pm.loop_run();
    assert_eq!(*ticks.borrow(), 3);
}

#[test]
fn task_stats_record_timings() {
    let mut pm = Pm::new();
    pm.task_add("busy", 1.0, |_| {
        std::hint::black_box((0..10_000u64).sum::<u64>());
    });
    pm.loop_once(1.0 / 60.0);
    pm.loop_once(1.0 / 60.0);
    let stats = pm.task_stats();
    let (name, s) = &stats[0];
    assert_eq!(name, "busy");
    assert_eq!(s.calls, 2);
    assert!(s.ns_total >= s.ns_max);
    pm.task_stats_reset();
    assert!(pm.task_stats().is_empty());
}

#[test]
fn loop_rate_is_accurate_on_average() {
    let mut pm = Pm::new();
    pm.loop_rate = 240;
    pm.task_add("count", 0.0, |pm| {
        if pm.tick() >= 121 {
            pm.loop_quit();
        }
    });
    let start = std::time::Instant::now();
    pm.loop_run(); // 120 timed ticks at 240 Hz = 500 ms nominal
    let elapsed = start.elapsed().as_secs_f64();
    // Relative sleeps accumulate ~0.5-1 ms oversleep per tick (~570+ ms);
    // absolute deadlines must hold the average. Generous upper bound for
    // loaded machines, tight enough to catch the accumulation bug.
    assert!((0.46..0.56).contains(&elapsed), "120 ticks at 240Hz took {elapsed:.3}s");
}
