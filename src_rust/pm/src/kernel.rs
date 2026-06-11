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

/// What a task hands back when it can't do its job. Boxed so tasks can
/// return any error type (or a plain `String`/`&str`) via `?`.
pub type TaskError = Box<dyn std::error::Error>;

/// Lets `task_add` accept closures that return nothing (infallible task)
/// *or* `Result<(), E>` (fallible task) without two registration APIs.
/// This is return-type polymorphism via a conversion trait — the same
/// pattern axum uses for handlers (`IntoResponse`).
pub trait IntoTaskResult {
    fn into_task_result(self) -> Result<(), TaskError>;
}

impl IntoTaskResult for () {
    fn into_task_result(self) -> Result<(), TaskError> {
        Ok(())
    }
}

impl<E: Into<TaskError>> IntoTaskResult for Result<(), E> {
    fn into_task_result(self) -> Result<(), TaskError> {
        self.map_err(Into::into)
    }
}

type TaskFn = Box<dyn FnMut(&mut Pm) -> Result<(), TaskError>>;

struct Task {
    name: String,
    module: Option<Rc<str>>,
    priority: f32,
    interval: f32, // 0 = every tick
    accum: f32,
    faulted: bool,
    func: TaskFn,
}

/// Record of a task that returned `Err` and was benched. The task no
/// longer runs; the rest of the loop is unaffected.
#[derive(Clone, Debug)]
pub struct TaskFault {
    pub task: String,
    pub module: Option<String>,
    pub tick: u32,
    pub error: String,
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
    erased_pools: Vec<(String, Rc<RefCell<dyn ErasedPool>>)>,
    tasks: Vec<Task>,
    tasks_dirty: bool,
    stop_requests: Vec<String>,
    module_stops: Vec<String>,
    /// Set while a module's init or one of its tasks runs, so anything
    /// registered during that window is tagged as the module's.
    current_module: Option<Rc<str>>,
    pool_owners: HashMap<String, Rc<str>>,
    state_owners: HashMap<String, Rc<str>>,
    faults: Vec<TaskFault>,
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
    /// Max sleep-then-spin margin per tick, in microseconds. `loop_run`
    /// wakes early by up to this much and busy-waits to the exact
    /// deadline, bounding tick jitter at the cost of CPU. The margin
    /// adapts to measured OS oversleep. 0 = plain sleep (jitter ~= OS
    /// wakeup latency, no spin cost).
    pub loop_spin_us: u32,
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
            module_stops: Vec::new(),
            current_module: None,
            pool_owners: HashMap::new(),
            state_owners: HashMap::new(),
            faults: Vec::new(),
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
            loop_spin_us: 2000,
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
    /// a handle to the same instance on re-fetch. A state first created
    /// while a module is active belongs to that module.
    pub fn state_get<T: Default + 'static>(&mut self, name: &str) -> Rc<RefCell<T>> {
        if !self.states.contains_key(name) {
            if let Some(m) = &self.current_module {
                self.state_owners.insert(name.to_string(), m.clone());
            }
            self.states
                .insert(name.to_string(), Box::new(Rc::new(RefCell::new(T::default()))));
        }
        self.states[name]
            .downcast_ref::<Rc<RefCell<T>>>()
            .unwrap_or_else(|| panic!("state '{name}' already registered with a different type"))
            .clone()
    }

    /// Named sparse-set component pool. Created on first fetch. A pool
    /// first created while a module is active belongs to that module.
    pub fn pool_get<T: 'static>(&mut self, name: &str) -> Rc<RefCell<Pool<T>>> {
        if !self.pools.contains_key(name) {
            if let Some(m) = &self.current_module {
                self.pool_owners.insert(name.to_string(), m.clone());
            }
            let pool = Rc::new(RefCell::new(Pool::<T>::new()));
            pool.borrow_mut().tick_set(self.tick);
            self.erased_pools.push((name.to_string(), pool.clone()));
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
    ///
    /// The closure may return `()` (infallible) or `Result<(), E>` for any
    /// boxable error. A task that returns `Err` is benched: it stops
    /// running, the fault is recorded in `task_faults`, and the loop
    /// carries on.
    pub fn task_add<R: IntoTaskResult>(
        &mut self,
        name: &str,
        priority: f32,
        func: impl FnMut(&mut Pm) -> R + 'static,
    ) {
        self.task_add_every(name, priority, 0.0, func);
    }

    /// Register a periodic task that runs once per `interval` seconds.
    pub fn task_add_every<R: IntoTaskResult>(
        &mut self,
        name: &str,
        priority: f32,
        interval: f32,
        mut func: impl FnMut(&mut Pm) -> R + 'static,
    ) {
        self.tasks.push(Task {
            name: name.to_string(),
            module: self.current_module.clone(),
            priority,
            interval,
            accum: 0.0,
            faulted: false,
            func: Box::new(move |pm| func(pm).into_task_result()),
        });
        self.tasks_dirty = true;
    }

    /// Tasks benched after returning `Err`, oldest first.
    pub fn task_faults(&self) -> &[TaskFault] {
        &self.faults
    }

    pub fn task_faults_clear(&mut self) {
        self.faults.clear();
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

    // --- modules ----------------------------------------------------------

    /// Install a named module: a bundle of tasks, pools, and states that
    /// can be torn down as a unit with `module_remove`.
    ///
    /// `init` receives `&mut Pm` and registers things exactly the way
    /// `main` does — `task_add`, `pool_get`, `state_get`. While it runs
    /// (and later, while any of the module's tasks run), everything
    /// first-created is tagged as belonging to the module, so runtime
    /// additions by module tasks are owned too. If `init` returns `Err`,
    /// whatever it registered is rolled back and the error is returned.
    pub fn module_add<R: IntoTaskResult>(
        &mut self,
        name: &str,
        init: impl FnOnce(&mut Pm) -> R,
    ) -> Result<(), TaskError> {
        let prev = self.current_module.replace(Rc::from(name));
        let result = init(self).into_task_result();
        self.current_module = prev;
        if result.is_err() {
            self.module_remove(name);
        }
        result
    }

    /// Tear down a module: stop its tasks and drop its pools and states
    /// from the registries. Tasks from other modules that hold `Rc`
    /// handles to a removed pool keep a working (now orphaned) pool until
    /// they drop the handle; a fresh `pool_get` of the same name creates
    /// a new, empty pool. If called from inside a task, the module's
    /// tasks stop at the end of the current tick.
    pub fn module_remove(&mut self, name: &str) {
        self.tasks.retain(|t| t.module.as_deref() != Some(name));
        self.module_stops.push(name.to_string());

        let pools: Vec<String> = self
            .pool_owners
            .iter()
            .filter(|(_, owner)| &***owner == name)
            .map(|(n, _)| n.clone())
            .collect();
        for pool_name in pools {
            self.pool_owners.remove(&pool_name);
            self.pools.remove(&pool_name);
            self.erased_pools.retain(|(n, _)| *n != pool_name);
        }

        let states: Vec<String> = self
            .state_owners
            .iter()
            .filter(|(_, owner)| &***owner == name)
            .map(|(n, _)| n.clone())
            .collect();
        for state_name in states {
            self.state_owners.remove(&state_name);
            self.states.remove(&state_name);
        }
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
            for (_, pool) in &self.erased_pools {
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
        for (_, pool) in &self.erased_pools {
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
            // Anything the task registers at runtime inherits its module.
            self.current_module = task.module.clone();
            let started = Instant::now();
            let result = (task.func)(self);
            let ns = started.elapsed().as_nanos() as u64;
            self.current_module = None;
            if let Err(err) = result {
                eprintln!("pm: task '{}' faulted at tick {}: {err}", task.name, self.tick);
                task.faulted = true;
                self.faults.push(TaskFault {
                    task: task.name.clone(),
                    module: task.module.as_deref().map(str::to_string),
                    tick: self.tick,
                    error: err.to_string(),
                });
            }
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
        running.retain(|t| !t.faulted);
        running.append(&mut self.tasks);
        self.tasks = running;
        for name in std::mem::take(&mut self.stop_requests) {
            self.tasks.retain(|t| t.name != name);
        }
        for name in std::mem::take(&mut self.module_stops) {
            self.tasks.retain(|t| t.module.as_deref() != Some(name.as_str()));
        }

        self.id_process_removes();
    }

    /// Run the loop at `loop_rate` ticks per second until `loop_quit`.
    ///
    /// Ticks are scheduled against absolute deadlines: `thread::sleep`
    /// reliably oversleeps by scheduler latency, and a relative
    /// sleep-per-tick accumulates that error (60 becomes ~57 on WSL).
    /// Advancing a fixed deadline instead means oversleep on one tick
    /// shortens the next sleep, so the average rate stays exact.
    ///
    /// Per-tick jitter is bounded by sleep-then-spin: sleep until
    /// `loop_spin_us` short of the deadline, then busy-wait the rest. The
    /// margin adapts to the oversleep the OS actually delivers, so a quiet
    /// machine spins less and a noisy one spins enough.
    pub fn loop_run(&mut self) {
        let mut last = Instant::now();
        let mut deadline = Instant::now();
        // Adaptive spin margin: tracks recent worst sleep overshoot.
        let mut margin = Duration::from_micros(self.loop_spin_us as u64);
        while !self.quit {
            let now = Instant::now();
            let dt = (now - last).as_secs_f32();
            last = now;
            self.loop_once(dt);
            if self.loop_rate == 0 {
                continue;
            }
            let period = Duration::from_secs_f64(1.0 / self.loop_rate as f64);
            deadline += period;
            let now = Instant::now();
            if deadline > now {
                let max_margin = Duration::from_micros(self.loop_spin_us as u64);
                let until_deadline = deadline - now;
                if until_deadline > margin {
                    let before = Instant::now();
                    let requested = until_deadline - margin;
                    std::thread::sleep(requested);
                    // Learn the OS's actual overshoot; decay toward zero so
                    // one bad wakeup doesn't pin the margin high forever.
                    let overshoot = before.elapsed().saturating_sub(requested);
                    margin = margin
                        .saturating_sub(Duration::from_micros(20))
                        .max(overshoot)
                        .min(max_margin);
                }
                while Instant::now() < deadline {
                    std::hint::spin_loop();
                }
            } else if now - deadline > 4 * period {
                // Fell far behind (suspend, debugger, long tick burst):
                // resync instead of running back-to-back to catch up.
                deadline = now;
            }
        }
    }
}
