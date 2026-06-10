//! The Pm kernel: flat task scheduler, named state singletons, named
//! component pools, entity id lifecycle with end-of-tick deferred removal.
//!
//! Usage pattern (mirrors the C++ framework): fetch pools and states during
//! init, clone the `Rc` handles into the task closure, and `borrow_mut()`
//! inside the task. A `RefCell` borrow panic is the Rust equivalent of a
//! C++ TaskFault: it means two tasks (or one task, twice) held the same
//! pool mutably at the same time.

use std::any::Any;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::{Duration, Instant};

use crate::id::{Id, IdAllocator};
use crate::pool::{ErasedPool, Pool};

type TaskFn = Box<dyn FnMut(&mut Pm)>;

struct Task {
    name: String,
    priority: f32,
    interval: f32, // 0 = every tick
    accum: f32,
    func: TaskFn,
}

/// Cumulative per-task timing, collected by `loop_once` (~80 ns overhead
/// per task call). Reset with `task_stats_reset`.
#[derive(Default, Clone, Debug)]
pub struct TaskStat {
    pub calls: u64,
    pub ns_total: u64,
    pub ns_max: u64,
}

pub struct Pm {
    states: HashMap<String, Box<dyn Any>>,
    pools: HashMap<String, Box<dyn Any>>,
    erased_pools: Vec<Rc<RefCell<dyn ErasedPool>>>,
    tasks: Vec<Task>,
    tasks_dirty: bool,
    stop_requests: Vec<String>,
    stats: HashMap<String, TaskStat>,
    ids: IdAllocator,
    pending_removes: Vec<Id>,
    removal_log: Vec<(Id, u32)>,
    removal_hold: bool,
    tick: u32,
    dt: f32,
    quit: bool,
    /// Target ticks per second for `loop_run`. 0 = uncapped.
    pub loop_rate: u32,
    /// This instance's peer id: 0 = server/single-player; clients get
    /// theirs at handshake. `id_add` allocates in this peer's space, and
    /// only this peer's indices are ever recycled locally.
    pub local_peer: u8,
}

impl Default for Pm {
    fn default() -> Self {
        Self {
            states: HashMap::new(),
            pools: HashMap::new(),
            erased_pools: Vec::new(),
            tasks: Vec::new(),
            tasks_dirty: false,
            stop_requests: Vec::new(),
            stats: HashMap::new(),
            ids: IdAllocator::new(),
            pending_removes: Vec::new(),
            removal_log: Vec::new(),
            removal_hold: false,
            // Tick 1 from birth so a brand-new peer's ack of 0 means
            // "send everything", including init-time adds.
            tick: 1,
            dt: 0.0,
            quit: false,
            loop_rate: 60,
            local_peer: 0,
        }
    }
}

impl Pm {
    pub fn new() -> Self {
        Self::default()
    }

    // --- states & pools ------------------------------------------------

    /// Named singleton state. Created (via Default) on first fetch; returns
    /// a handle to the same instance on re-fetch.
    pub fn state_get<T: Default + 'static>(&mut self, name: &str) -> Rc<RefCell<T>> {
        self.states
            .entry(name.to_string())
            .or_insert_with(|| Box::new(Rc::new(RefCell::new(T::default()))))
            .downcast_ref::<Rc<RefCell<T>>>()
            .unwrap_or_else(|| panic!("state '{name}' already registered with a different type"))
            .clone()
    }

    /// Named sparse-set component pool. Created on first fetch.
    pub fn pool_get<T: 'static>(&mut self, name: &str) -> Rc<RefCell<Pool<T>>> {
        if !self.pools.contains_key(name) {
            let pool = Rc::new(RefCell::new(Pool::<T>::new()));
            pool.borrow_mut().tick_set(self.tick);
            self.erased_pools.push(pool.clone());
            self.pools.insert(name.to_string(), Box::new(pool));
        }
        self.pools[name]
            .downcast_ref::<Rc<RefCell<Pool<T>>>>()
            .unwrap_or_else(|| panic!("pool '{name}' already registered with a different type"))
            .clone()
    }

    // --- tasks ----------------------------------------------------------

    /// Register a task that runs every tick. Lowest priority runs first.
    /// Tasks added from inside a task start on the next tick.
    pub fn task_add(&mut self, name: &str, priority: f32, func: impl FnMut(&mut Pm) + 'static) {
        self.task_add_every(name, priority, 0.0, func);
    }

    /// Register a periodic task that runs once per `interval` seconds.
    pub fn task_add_every(
        &mut self,
        name: &str,
        priority: f32,
        interval: f32,
        func: impl FnMut(&mut Pm) + 'static,
    ) {
        self.tasks.push(Task {
            name: name.to_string(),
            priority,
            interval,
            accum: 0.0,
            func: Box::new(func),
        });
        self.tasks_dirty = true;
    }

    /// Per-task cumulative timings since start (or last reset), heaviest
    /// first. Callable from inside a task.
    pub fn task_stats(&self) -> Vec<(String, TaskStat)> {
        let mut v: Vec<_> = self.stats.iter().map(|(n, s)| (n.clone(), s.clone())).collect();
        v.sort_by_key(|(_, s)| std::cmp::Reverse(s.ns_total));
        v
    }

    pub fn task_stats_reset(&mut self) {
        self.stats.clear();
    }

    /// Stop a task by name. If called from inside a task, takes effect at
    /// the end of the current tick.
    pub fn task_stop(&mut self, name: &str) {
        self.tasks.retain(|t| t.name != name);
        self.stop_requests.push(name.to_string());
    }

    // --- ids --------------------------------------------------------------

    pub fn id_add(&mut self) -> Id {
        self.ids.add(self.local_peer)
    }

    pub fn id_add_for(&mut self, peer: u8) -> Id {
        self.ids.add(peer)
    }

    pub fn id_alive(&self, id: Id) -> bool {
        self.ids.alive(id)
    }

    /// Deferred removal: flushed from every pool at the end of `loop_once`.
    pub fn id_remove(&mut self, id: Id) {
        self.pending_removes.push(id);
    }

    /// Accept a remote id (networking): mark alive, record its generation.
    pub fn id_sync(&mut self, id: Id) {
        self.ids.sync(id);
    }

    fn id_process_removes(&mut self) {
        while let Some(id) = self.pending_removes.pop() {
            if !self.ids.alive(id) {
                continue; // duplicate or stale request
            }
            self.ids.kill(id);
            for pool in &self.erased_pools {
                pool.borrow_mut().erased_remove(id);
            }
            self.removal_log.push((id, self.tick));
        }
        // Without a NetServer holding the log, prune immediately
        // (single-player behavior). A NetServer gates this on
        // min(last_acked_tick) across peers so an index is never reused
        // before every peer has seen the removal.
        if !self.removal_hold {
            self.removal_release_upto(self.tick);
        }
    }

    pub(crate) fn removal_hold_set(&mut self, hold: bool) {
        self.removal_hold = hold;
    }

    pub(crate) fn removal_log(&self) -> &[(Id, u32)] {
        &self.removal_log
    }

    /// Release (recycle) logged removals stamped at or before `tick`.
    pub(crate) fn removal_release_upto(&mut self, tick: u32) {
        let mut i = 0;
        while i < self.removal_log.len() {
            if self.removal_log[i].1 <= tick {
                let (id, _) = self.removal_log.swap_remove(i);
                // Only recycle indices this instance owns; synced foreign
                // ids are the owning peer's to reuse.
                if id.peer() == self.local_peer {
                    self.ids.release(id);
                }
            } else {
                i += 1;
            }
        }
    }

    // --- loop ---------------------------------------------------------------

    /// Current kernel tick. Increments once per `loop_once`; pool change
    /// stamps and the removal log are expressed in this clock.
    pub fn tick(&self) -> u32 {
        self.tick
    }

    pub fn loop_dt(&self) -> f32 {
        self.dt
    }

    pub fn loop_quit(&mut self) {
        self.quit = true;
    }

    /// Run one tick: all active tasks in priority order, then flush
    /// deferred id removals. Public so tests and headless sims can drive
    /// the kernel with a fixed dt.
    pub fn loop_once(&mut self, dt: f32) {
        self.dt = dt;
        self.tick += 1;
        for pool in &self.erased_pools {
            pool.borrow_mut().tick_set(self.tick);
        }
        if self.tasks_dirty {
            self.tasks.sort_by(|a, b| a.priority.total_cmp(&b.priority));
            self.tasks_dirty = false;
        }

        // Take the task list out of self so tasks can borrow Pm mutably.
        // task_add during a tick pushes into the (now empty) self.tasks
        // and is merged back in below, sorted at the start of next tick.
        let mut running = std::mem::take(&mut self.tasks);
        for task in &mut running {
            if task.interval > 0.0 {
                task.accum += dt;
                if task.accum < task.interval {
                    continue;
                }
                task.accum -= task.interval;
            }
            let started = Instant::now();
            (task.func)(self);
            let ns = started.elapsed().as_nanos() as u64;
            match self.stats.get_mut(&task.name) {
                Some(s) => {
                    s.calls += 1;
                    s.ns_total += ns;
                    s.ns_max = s.ns_max.max(ns);
                }
                None => {
                    self.stats
                        .insert(task.name.clone(), TaskStat { calls: 1, ns_total: ns, ns_max: ns });
                }
            }
        }
        running.append(&mut self.tasks);
        self.tasks = running;
        for name in std::mem::take(&mut self.stop_requests) {
            self.tasks.retain(|t| t.name != name);
        }

        self.id_process_removes();
    }

    /// Run the loop at `loop_rate` ticks per second until `loop_quit`.
    ///
    /// Ticks are scheduled against absolute deadlines: `thread::sleep`
    /// reliably oversleeps by scheduler latency, and a relative
    /// sleep-per-tick accumulates that error (60 becomes ~57 on WSL).
    /// Advancing a fixed deadline instead means oversleep on one tick
    /// shortens the next sleep, so the average rate stays exact; per-tick
    /// jitter remains ~the OS wakeup latency.
    pub fn loop_run(&mut self) {
        let mut last = Instant::now();
        let mut deadline = Instant::now();
        while !self.quit {
            let now = Instant::now();
            let dt = (now - last).as_secs_f32();
            last = now;
            self.loop_once(dt);
            if self.loop_rate > 0 {
                let period = Duration::from_secs_f64(1.0 / self.loop_rate as f64);
                deadline += period;
                let now = Instant::now();
                if deadline > now {
                    std::thread::sleep(deadline - now);
                } else if now - deadline > 4 * period {
                    // Fell far behind (suspend, debugger, long tick burst):
                    // resync instead of running back-to-back to catch up.
                    deadline = now;
                }
            }
        }
    }
}
