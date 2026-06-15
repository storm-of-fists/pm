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
fn single_is_one_entity_shared_by_name() {
    let mut pm = Pm::new();
    let a = pm.single::<Hits>("hits");
    a.get_mut().0 = 7;
    let b = pm.single::<Hits>("hits");
    assert_eq!(b.get().0, 7);
    assert_eq!(a.id(), b.id());
    // It's an ordinary pool entity underneath.
    assert_eq!(pm.pool::<Hits>("hits").get().len(), 1);
}

#[test]
#[should_panic(expected = "different type")]
fn single_type_mismatch_panics() {
    let mut pm = Pm::new();
    let _ = pm.single::<Hits>("thing");
    let _ = pm.single::<Pos>("thing");
}

#[test]
fn tasks_run_in_priority_order() {
    let mut pm = Pm::new();
    let order = Rc::new(RefCell::new(Vec::new()));
    for (name, prio) in [("c", 30.0), ("a", 10.0), ("b", 20.0)] {
        let order = order.clone();
        pm.task_add(name, prio, 0.0, move |_| order.borrow_mut().push(name));
    }
    pm.loop_once(1.0 / 60.0);
    assert_eq!(*order.borrow(), vec!["a", "b", "c"]);
}

#[test]
fn the_canonical_pattern_works() {
    // Fetch during init, capture the handle in the closure, mutate in the task.
    let mut pm = Pm::new();
    let pos = pm.pool::<Pos>("pos");

    let id = pm.id_add();
    pos.get_mut().add(id, Pos { x: 0.0, y: 0.0 });

    pm.task_add("physics", 30.0, 0.0, {
        let pos = pos.clone();
        move |pm| {
            let dt = pm.loop_dt();
            for (_, mut p) in pos.get_mut().iter_mut() {
                p.x += 10.0 * dt;
            }
        }
    });

    pm.loop_once(0.5);
    pm.loop_once(0.5);
    assert_eq!(pos.get().get(id), Some(&Pos { x: 10.0, y: 0.0 }));
}

#[test]
fn id_remove_is_deferred_and_flushes_all_pools() {
    let mut pm = Pm::new();
    let pos = pm.pool::<Pos>("pos");
    let hp = pm.pool::<u32>("hp");

    let id = pm.id_add();
    pos.get_mut().add(id, Pos::default());
    hp.get_mut().add(id, 100);

    let seen_alive_after_remove = Rc::new(RefCell::new(false));
    pm.task_add("reaper", 10.0, 0.0, {
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
    assert!(!pos.get().contains(id));
    assert!(!hp.get().contains(id));
}

#[test]
fn removed_ids_recycle_with_bumped_generation() {
    let mut pm = Pm::new();
    let pos = pm.pool::<Pos>("pos");

    let a = pm.id_add();
    pos.get_mut().add(a, Pos { x: 1.0, y: 0.0 });
    pm.id_remove(a);
    pm.loop_once(1.0 / 60.0); // flush: kill + (no peers) release

    let b = pm.id_add();
    assert_eq!(b.index(), a.index(), "index should recycle");
    assert_eq!(b.generation(), a.generation() + 1, "generation should bump");
    assert!(!pm.id_alive(a) && pm.id_alive(b));

    pos.get_mut().add(b, Pos { x: 2.0, y: 0.0 });
    assert_eq!(pos.get().get(a), None, "stale handle must miss");
    assert_eq!(pos.get().get(b), Some(&Pos { x: 2.0, y: 0.0 }));
}

#[test]
fn tick_advances_and_stamps_changes() {
    let mut pm = Pm::new();
    let hp = pm.pool::<u32>("hp");
    let id = pm.id_add();
    hp.get_mut().add(id, 100); // init-time add, stamped tick 1
    let init_tick = pm.tick();

    pm.task_add("damage", 10.0, 0.0, {
        let hp = hp.clone();
        move |_| {
            if let Some(mut h) = hp.get_mut().get_mut(id) {
                *h -= 1;
            }
        }
    });
    pm.loop_once(1.0 / 60.0);
    pm.loop_once(1.0 / 60.0);

    assert_eq!(pm.tick(), init_tick + 2);
    assert_eq!(hp.get().changed_tick(id), Some(pm.tick()));
    // A peer that acked the first tick still sees the later change.
    assert_eq!(hp.get().changed_since(init_tick + 1).count(), 1);
    assert_eq!(hp.get().changed_since(pm.tick()).count(), 0);
}

#[test]
fn periodic_task_fires_once_per_interval() {
    let mut pm = Pm::new();
    let count = Rc::new(RefCell::new(0));
    pm.task_add("slow", 10.0, 1.0, {
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

    pm.task_add("spawner", 10.0, 0.0, {
        let log = log.clone();
        move |pm| {
            log.borrow_mut().push("spawner");
            let log = log.clone();
            pm.task_add("spawned", 5.0, 0.0, move |_| {
                log.borrow_mut().push("spawned")
            });
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
    pm.task_add("control", 0.0, 0.0, {
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
    pm.task_add("busy", 1.0, 0.0, |_| {
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
    pm.task_add("count", 0.0, 0.0, |pm| {
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
    assert!(
        (0.46..0.56).contains(&elapsed),
        "120 ticks at 240Hz took {elapsed:.3}s"
    );
}

/// Manual probe: `cargo test --release jitter_probe -- --ignored --nocapture`
#[test]
#[ignore]
fn jitter_probe() {
    for (label, spin_us) in [
        ("plain sleep (spin=0)", 0u32),
        ("sleep+spin (default)", 2000),
    ] {
        let mut pm = Pm::new();
        pm.loop_rate = 60;
        pm.loop_spin_us = spin_us;
        let times = Rc::new(RefCell::new(Vec::<std::time::Instant>::new()));
        pm.task_add("probe", 0.0, 0.0, {
            let times = times.clone();
            move |pm| {
                times.borrow_mut().push(std::time::Instant::now());
                if pm.tick() >= 181 {
                    pm.loop_quit();
                }
            }
        });
        pm.loop_run(); // 180 ticks = 3 s
        let times = times.borrow();
        // Deviation of each tick interval from the nominal period.
        let period = 1.0 / 60.0;
        let mut devs: Vec<f64> = times
            .windows(2)
            .map(|w| ((w[1] - w[0]).as_secs_f64() - period).abs() * 1e6)
            .collect();
        devs.sort_by(f64::total_cmp);
        let pct = |p: f64| devs[((devs.len() - 1) as f64 * p) as usize];
        println!(
            "{label}: interval deviation p50 {:.0} us  p90 {:.0} us  p99 {:.0} us  max {:.0} us",
            pct(0.50),
            pct(0.90),
            pct(0.99),
            pct(1.0),
        );
    }
}

#[test]
fn faulting_task_is_benched_and_the_loop_survives() {
    let mut pm = Pm::new();
    let counts = Rc::new(RefCell::new((0u32, 0u32)));

    pm.task_add("flaky", 10.0, 0.0, {
        let counts = counts.clone();
        move |_| -> Result<(), String> {
            counts.borrow_mut().0 += 1;
            Err("disk on fire".to_string())
        }
    });
    pm.task_add("steady", 20.0, 0.0, {
        let counts = counts.clone();
        move |_| counts.borrow_mut().1 += 1
    });

    for _ in 0..3 {
        pm.loop_once(1.0 / 60.0);
    }
    // Flaky ran once, got benched; steady never noticed.
    assert_eq!(*counts.borrow(), (1, 3));
    assert_eq!(pm.task_faults().len(), 1);
    let fault = &pm.task_faults()[0];
    assert_eq!(fault.task, "flaky");
    assert_eq!(fault.error, "disk on fire");
    pm.task_faults_clear();
    assert!(pm.task_faults().is_empty());
}

#[test]
fn tasks_can_use_question_mark() {
    let mut pm = Pm::new();
    pm.task_add(
        "parse",
        10.0,
        0.0,
        |_| -> Result<(), Box<dyn std::error::Error>> {
            let n: i32 = "not a number".parse()?;
            let _ = n;
            Ok(())
        },
    );
    pm.loop_once(1.0 / 60.0);
    assert_eq!(pm.task_faults().len(), 1);
    assert!(pm.task_faults()[0].error.contains("invalid digit"));
}

#[test]
fn task_add_or_replace_swaps_a_task_in_place() {
    let mut pm = Pm::new();
    let log = Rc::new(RefCell::new(Vec::new()));

    pm.task_add("worker", 10.0, 0.0, {
        let log = log.clone();
        move |_| log.borrow_mut().push("v1")
    });
    pm.loop_once(1.0 / 60.0); // v1

    // Replace between ticks: the new body takes over, no duplicate.
    pm.task_add_or_replace("worker", 10.0, 0.0, {
        let log = log.clone();
        move |_| log.borrow_mut().push("v2")
    });
    pm.loop_once(1.0 / 60.0);
    pm.loop_once(1.0 / 60.0);
    assert_eq!(*log.borrow(), vec!["v1", "v2", "v2"]);
}

#[test]
fn task_add_or_replace_drops_the_old_body_mid_tick() {
    // The hot-reload shape: a lower-priority "reloader" replaces a task
    // that is *also scheduled this tick*. The old body must not survive
    // alongside the new one — they share a name, so the merge keeps the
    // most recent and drops the in-flight copy.
    let mut pm = Pm::new();
    let log = Rc::new(RefCell::new(Vec::new()));

    pm.task_add("worker", 10.0, 0.0, {
        let log = log.clone();
        move |_| log.borrow_mut().push("v1")
    });
    pm.task_add("reload", 5.0, 0.0, {
        let log = log.clone();
        move |pm| {
            let log = log.clone();
            pm.task_add_or_replace("worker", 10.0, 0.0, move |_| log.borrow_mut().push("v2"));
            pm.task_stop("reload");
        }
    });

    pm.loop_once(1.0 / 60.0); // reload swaps worker; v1 still runs this tick, then is dropped
    pm.loop_once(1.0 / 60.0);
    pm.loop_once(1.0 / 60.0);
    assert_eq!(*log.borrow(), vec!["v1", "v2", "v2"]);
}

#[test]
fn pool_remove_drops_the_pool() {
    let mut pm = Pm::new();
    let pool = pm.pool::<Pos>("p");
    let id = pm.id_add();
    pool.get_mut().add(id, Pos { x: 1.0, y: 0.0 });
    pm.pool_remove("p");
    // A fresh fetch makes a new empty pool.
    assert_eq!(pm.pool::<Pos>("p").get().len(), 0);
}

// --- per-entity access (Option) and pool locking -----------------------

#[test]
fn get_id_is_none_for_a_missing_entity() {
    let mut pm = Pm::new();
    let pos = pm.pool::<Pos>("pos");
    let ghost = pm.id_add(); // never added to the pool
    assert!(pos.get_id(ghost).is_none());
    assert!(pos.get_id_mut(ghost).is_none());

    let id = pm.id_add();
    pos.get_mut().add(id, Pos { x: 1.0, y: 1.0 });
    assert_eq!(*pos.get_id(id).unwrap(), Pos { x: 1.0, y: 1.0 });
}

#[test]
fn get_id_mut_stamps_the_changed_tick() {
    let mut pm = Pm::new();
    let pos = pm.pool::<Pos>("pos");
    let id = pm.id_add();
    pos.get_mut().add(id, Pos::default());
    pm.loop_once(1.0 / 60.0);
    pm.loop_once(1.0 / 60.0);

    let before = pos.get().changed_tick(id).unwrap();
    pos.get_id_mut(id).unwrap().x = 5.0;
    let after = pos.get().changed_tick(id).unwrap();
    assert!(after > before, "get_id_mut must stamp for the sync layer");
}

#[test]
fn a_singleton_survives_id_remove_of_its_entity() {
    let mut pm = Pm::new();
    let hits = pm.single::<Hits>("hits");
    hits.get_mut().0 = 9;
    // Even if the singleton's id is fed to the removal flush, the pool
    // lock keeps its entity (and the value) intact.
    pm.id_remove(hits.id());
    pm.loop_once(1.0 / 60.0);
    assert_eq!(hits.get().0, 9);
    assert_eq!(pm.pool::<Hits>("hits").get().len(), 1);
}
