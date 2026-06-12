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
    a.borrow_mut().0 = 7;
    let b = pm.single::<Hits>("hits");
    assert_eq!(b.borrow().0, 7);
    assert_eq!(a.id(), b.id());
    // It's an ordinary pool entity underneath.
    assert_eq!(pm.pool::<Hits>("hits").borrow().len(), 1);
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
    pos.borrow_mut().add(id, Pos { x: 0.0, y: 0.0 });

    pm.task_add("physics", 30.0, 0.0, {
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
    let pos = pm.pool::<Pos>("pos");
    let hp = pm.pool::<u32>("hp");

    let id = pm.id_add();
    pos.borrow_mut().add(id, Pos::default());
    hp.borrow_mut().add(id, 100);

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
    assert!(!pos.borrow().contains(id));
    assert!(!hp.borrow().contains(id));
}

#[test]
fn removed_ids_recycle_with_bumped_generation() {
    let mut pm = Pm::new();
    let pos = pm.pool::<Pos>("pos");

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
    let hp = pm.pool::<u32>("hp");
    let id = pm.id_add();
    hp.borrow_mut().add(id, 100); // init-time add, stamped tick 1
    let init_tick = pm.tick();

    pm.task_add("damage", 10.0, 0.0, {
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
            pm.task_add("spawned", 5.0, 0.0, move |_| log.borrow_mut().push("spawned"));
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
    assert!((0.46..0.56).contains(&elapsed), "120 ticks at 240Hz took {elapsed:.3}s");
}

/// Manual probe: `cargo test --release jitter_probe -- --ignored --nocapture`
#[test]
#[ignore]
fn jitter_probe() {
    for (label, spin_us) in [("plain sleep (spin=0)", 0u32), ("sleep+spin (default)", 2000)] {
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
    assert_eq!(fault.module, None);
    pm.task_faults_clear();
    assert!(pm.task_faults().is_empty());
}

#[test]
fn tasks_can_use_question_mark() {
    let mut pm = Pm::new();
    pm.task_add("parse", 10.0, 0.0, |_| -> Result<(), Box<dyn std::error::Error>> {
        let n: i32 = "not a number".parse()?;
        let _ = n;
        Ok(())
    });
    pm.loop_once(1.0 / 60.0);
    assert_eq!(pm.task_faults().len(), 1);
    assert!(pm.task_faults()[0].error.contains("invalid digit"));
}

#[test]
fn module_add_and_remove_tear_down_as_a_unit() {
    let mut pm = Pm::new();
    let outside = pm.pool::<Pos>("outside");
    let runs = Rc::new(RefCell::new(0u32));

    pm.module_add("physics", |pm| {
        let pos = pm.pool::<Pos>("mod_pos");
        let _hits = pm.single::<Hits>("mod_hits");
        let runs = runs.clone();
        pm.task_add("mod_task", 10.0, 0.0, move |_| {
            *runs.borrow_mut() += 1;
            let _ = &pos;
        });
    })
    .unwrap();

    pm.loop_once(1.0 / 60.0);
    assert_eq!(*runs.borrow(), 1);

    pm.module_remove("physics");
    pm.loop_once(1.0 / 60.0);
    // Task stopped; the pool registry forgot the module's pool, so a
    // re-fetch creates a fresh one, while unowned pools are untouched.
    assert_eq!(*runs.borrow(), 1);
    let fresh = pm.pool::<Pos>("mod_pos");
    assert_eq!(fresh.borrow().len(), 0);
    // The unowned pool is untouched: a re-fetch sees the same data.
    let outside_id = pm.id_add();
    outside.borrow_mut().add(outside_id, Pos { x: 9.0, y: 9.0 });
    assert_eq!(pm.pool::<Pos>("outside").borrow().get(outside_id), Some(&Pos { x: 9.0, y: 9.0 }));
}

#[test]
fn module_init_error_rolls_back_registration() {
    let mut pm = Pm::new();
    let result = pm.module_add("broken", |pm| -> Result<(), String> {
        let _pool = pm.pool::<Pos>("broken_pool");
        pm.task_add("broken_task", 10.0, 0.0, |_| {});
        Err("init failed".to_string())
    });
    assert!(result.is_err());

    // Nothing of the module survives: fresh pool on re-fetch, task gone.
    let pool = pm.pool::<Pos>("broken_pool");
    let id = pm.id_add();
    pool.borrow_mut().add(id, Pos { x: 1.0, y: 0.0 });
    pm.loop_once(1.0 / 60.0);
    assert!(pm.task_stats().iter().all(|(name, _)| name != "broken_task"));
}

#[test]
fn runtime_additions_by_module_tasks_belong_to_the_module() {
    let mut pm = Pm::new();
    let spawned_runs = Rc::new(RefCell::new(0u32));

    pm.module_add("spawner_mod", |pm| {
        let spawned_runs = spawned_runs.clone();
        pm.task_add("spawner", 10.0, 0.0, move |pm| {
            let spawned_runs = spawned_runs.clone();
            pm.task_add("late_task", 5.0, 0.0, move |_| *spawned_runs.borrow_mut() += 1);
            pm.task_stop("spawner");
        });
    })
    .unwrap();

    pm.loop_once(1.0 / 60.0); // spawner registers late_task
    pm.loop_once(1.0 / 60.0); // late_task runs
    assert_eq!(*spawned_runs.borrow(), 1);

    pm.module_remove("spawner_mod");
    pm.loop_once(1.0 / 60.0);
    // late_task was added at runtime by a module task — still owned, still removed.
    assert_eq!(*spawned_runs.borrow(), 1);
}

#[test]
fn faulting_module_task_records_its_module() {
    let mut pm = Pm::new();
    pm.module_add("m", |pm| {
        pm.task_add("doomed", 10.0, 0.0, |_| -> Result<(), String> { Err("oops".into()) });
    })
    .unwrap();
    pm.loop_once(1.0 / 60.0);
    assert_eq!(pm.task_faults()[0].module.as_deref(), Some("m"));
}

// --- fallible data access (try_* -> task faults) -----------------------

#[test]
fn bad_entity_access_faults_the_task_instead_of_crashing() {
    let mut pm = Pm::new();
    let pos = pm.pool::<Pos>("pos");
    let ghost = pm.id_add(); // never added to the pool

    let healthy_runs = Rc::new(RefCell::new(0u32));
    pm.task_add("reader", 10.0, 0.0, {
        let pos = pos.clone();
        move |_| -> Result<(), pm::TaskError> {
            let p = pos.try_get(ghost)?; // Missing -> AccessError -> fault
            let _ = *p;
            Ok(())
        }
    });
    pm.task_add("healthy", 20.0, 0.0, {
        let healthy_runs = healthy_runs.clone();
        move |_| *healthy_runs.borrow_mut() += 1
    });

    for _ in 0..3 {
        pm.loop_once(1.0 / 60.0);
    }
    assert_eq!(*healthy_runs.borrow(), 3);
    assert_eq!(pm.task_faults().len(), 1);
    assert!(pm.task_faults()[0].error.contains("not in pool 'pos'"));
}

#[test]
fn busy_pool_is_an_error_not_a_panic_via_try() {
    let mut pm = Pm::new();
    let pos = pm.pool::<Pos>("pos");
    let id = pm.id_add();
    pos.borrow_mut().add(id, Pos { x: 1.0, y: 1.0 });

    let held = pos.borrow_mut(); // simulate another task holding the pool
    assert!(matches!(pos.try_get(id), Err(pm::AccessError::Busy { .. })));
    assert!(matches!(pos.try_mut(id), Err(pm::AccessError::Busy { .. })));
    drop(held);
    assert_eq!(*pos.try_get(id).unwrap(), Pos { x: 1.0, y: 1.0 });
}

#[test]
fn try_mut_stamps_the_changed_tick() {
    let mut pm = Pm::new();
    let pos = pm.pool::<Pos>("pos");
    let id = pm.id_add();
    pos.borrow_mut().add(id, Pos::default());
    pm.loop_once(1.0 / 60.0);
    pm.loop_once(1.0 / 60.0);

    let before = pos.borrow().changed_tick(id).unwrap();
    pos.try_mut(id).unwrap().x = 5.0;
    let after = pos.borrow().changed_tick(id).unwrap();
    assert!(after > before, "try_mut must stamp for the sync layer");
}
