//! The Pm kernel: flat task scheduler, named component pools (singletons
//! are just single-entity pools — see `single`), entity id lifecycle with
//! end-of-tick deferred removal.
//!
//! Usage pattern (mirrors the C++ framework): fetch pool/singleton
//! handles during init, clone them into the task closure, and access
//! inside the task. Tasks are plain closures; a task's "state" is its
//! captures. Closures may return `Result` — an `Err` benches the task
//! (recorded in `task_faults`) and the loop survives, so fallible data
//! access (`try_get`/`try_mut` + `?`) turns bad-access bugs into
//! captured faults instead of crashes. The panicking `borrow` forms
//! remain for hot paths where a conflict is a programming error you
//! want loud.

use std::any::Any;
use std::cell::{Ref, RefCell, RefMut};
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

struct TaskEntry {
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

/// Why a fallible data access failed. Implements `Error`, so inside a
/// fallible task `?` converts it straight into a `TaskFault`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AccessError {
    /// The pool is already mutably borrowed — two tasks colliding on the
    /// same data, or one task borrowing it twice.
    Busy { pool: String },
    /// The entity isn't in the pool (never added, or removed).
    Missing { pool: String, id: Id },
}

impl std::fmt::Display for AccessError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AccessError::Busy { pool } => write!(f, "pool '{pool}' is busy (borrowed elsewhere)"),
            AccessError::Missing { pool, id } => {
                write!(f, "entity {:?} not in pool '{pool}'", id)
            }
        }
    }
}

impl std::error::Error for AccessError {}

/// Handle to a named pool. This is what `pm.pool()` returns and what
/// task closures capture; it hides the `Rc<RefCell<..>>` plumbing.
///
/// Two access styles:
/// - `borrow`/`borrow_mut` lock the whole pool for iteration. They
///   panic on a borrow conflict — that's two tasks holding the same
///   pool mutably at once, a real bug worth a loud stop.
/// - `try_get`/`try_mut`/`try_borrow`/`try_borrow_mut` return
///   `Result<_, AccessError>`, so per-entity access in a fallible task
///   propagates with `?` and a bad access benches the task instead of
///   crashing the loop.
pub struct Handle<T> {
    name: Rc<str>,
    rc: Rc<RefCell<Pool<T>>>,
}

impl<T> Clone for Handle<T> {
    fn clone(&self) -> Self {
        Self { name: self.name.clone(), rc: self.rc.clone() }
    }
}

impl<T: 'static> Handle<T> {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub(crate) fn rc(&self) -> &Rc<RefCell<Pool<T>>> {
        &self.rc
    }

    /// Whole-pool read lock (iteration). Panics if mutably borrowed.
    pub fn borrow(&self) -> Ref<'_, Pool<T>> {
        self.rc.borrow()
    }

    /// Whole-pool write lock (iteration/insert/remove). Panics if borrowed.
    pub fn borrow_mut(&self) -> RefMut<'_, Pool<T>> {
        self.rc.borrow_mut()
    }

    pub fn try_borrow(&self) -> Result<Ref<'_, Pool<T>>, AccessError> {
        self.rc.try_borrow().map_err(|_| AccessError::Busy { pool: self.name.to_string() })
    }

    pub fn try_borrow_mut(&self) -> Result<RefMut<'_, Pool<T>>, AccessError> {
        self.rc.try_borrow_mut().map_err(|_| AccessError::Busy { pool: self.name.to_string() })
    }

    /// Read one entity, fallibly.
    pub fn try_get(&self, id: Id) -> Result<Ref<'_, T>, AccessError> {
        let pool = self.try_borrow()?;
        Ref::filter_map(pool, |p| p.get(id))
            .map_err(|_| AccessError::Missing { pool: self.name.to_string(), id })
    }

    /// Write one entity, fallibly. Stamps the changed-tick immediately,
    /// so a synced entity replicates after this.
    pub fn try_mut(&self, id: Id) -> Result<RefMut<'_, T>, AccessError> {
        let pool = self.try_borrow_mut()?;
        RefMut::filter_map(pool, |p| p.get_mut_stamped(id))
            .map_err(|_| AccessError::Missing { pool: self.name.to_string(), id })
    }
}

/// Handle to a named singleton: one entity in an ordinary pool (there is
/// no separate "state" concept). `borrow_mut`/`try_mut` stamp the
/// changed-tick, so a synced singleton replicates on write.
pub struct Single<T> {
    handle: Handle<T>,
    id: Id,
}

impl<T> Clone for Single<T> {
    fn clone(&self) -> Self {
        Self { handle: self.handle.clone(), id: self.id }
    }
}

impl<T: 'static> Single<T> {
    pub fn id(&self) -> Id {
        self.id
    }

    /// The underlying pool handle (e.g. to register it for sync).
    pub fn pool(&self) -> &Handle<T> {
        &self.handle
    }

    /// Read access; panics on borrow conflict (bug-loud form).
    pub fn borrow(&self) -> Ref<'_, T> {
        self.try_get().expect("singleton access failed")
    }

    /// Write access; panics on borrow conflict (bug-loud form).
    pub fn borrow_mut(&self) -> RefMut<'_, T> {
        self.try_mut().expect("singleton access failed")
    }

    pub fn try_get(&self) -> Result<Ref<'_, T>, AccessError> {
        self.handle.try_get(self.id)
    }

    pub fn try_mut(&self) -> Result<RefMut<'_, T>, AccessError> {
        self.handle.try_mut(self.id)
    }
}

/// Cumulative per-task timing, collected by `loop_once` (~80 ns overhead
/// per task call). Reset with `task_stats_reset`.
#[derive(Default, Clone, Debug)]
pub struct TaskStat {
    pub calls: u64,
    pub ns_total: u64,
    pub ns_max: u64,
}

/// Recover the typed pool from the erased store. The same trick as
/// `Rc::<dyn Any>::downcast`, done by hand because the `RefCell` sits
/// between the `Rc` and the trait object: verify the concrete type via
/// the `Any` supertrait, then re-point the `Rc`.
fn downcast_pool<T: 'static>(rc: &Rc<RefCell<dyn ErasedPool>>) -> Option<Rc<RefCell<Pool<T>>>> {
    {
        let pool = rc.borrow();
        let any: &dyn Any = &*pool; // supertrait upcast (ErasedPool: Any)
        if !any.is::<Pool<T>>() {
            return None;
        }
    }
    let raw = Rc::into_raw(rc.clone()) as *const RefCell<Pool<T>>;
    // Safety: the allocation really is an `RefCell<Pool<T>>` (checked
    // above); we only drop the vtable half of the fat pointer.
    Some(unsafe { Rc::from_raw(raw) })
}

pub struct Pm {
    /// The one store: every pool, type-erased. Typed access goes through
    /// `pool()` (downcast); the kernel's own bookkeeping (tick stamps,
    /// entity removal) goes through the `ErasedPool` vtable.
    pools: HashMap<String, Rc<RefCell<dyn ErasedPool>>>,
    tasks: Vec<TaskEntry>,
    tasks_dirty: bool,
    stop_requests: Vec<String>,
    module_stops: Vec<String>,
    /// Set while a module's init or one of its tasks runs, so anything
    /// registered during that window is tagged as the module's.
    current_module: Option<Rc<str>>,
    pool_owners: HashMap<String, Rc<str>>,
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
            pools: HashMap::new(),
            tasks: Vec::new(),
            tasks_dirty: false,
            stop_requests: Vec::new(),
            module_stops: Vec::new(),
            current_module: None,
            pool_owners: HashMap::new(),
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

    // --- pools & singletons ---------------------------------------------

    /// Named sparse-set component pool. Created on first fetch. A pool
    /// first created while a module is active belongs to that module.
    pub fn pool<T: 'static>(&mut self, name: &str) -> Handle<T> {
        if let Some(rc) = self.pools.get(name) {
            let typed = downcast_pool::<T>(rc).unwrap_or_else(|| {
                panic!("pool '{name}' already registered with a different type")
            });
            return Handle { name: Rc::from(name), rc: typed };
        }
        if let Some(m) = &self.current_module {
            self.pool_owners.insert(name.to_string(), m.clone());
        }
        let rc = Rc::new(RefCell::new(Pool::<T>::new()));
        rc.borrow_mut().tick_set(self.tick);
        let erased: Rc<RefCell<dyn ErasedPool>> = rc.clone();
        self.pools.insert(name.to_string(), erased);
        Handle { name: Rc::from(name), rc }
    }

    /// Named singleton: a single-entity pool. Created (with one
    /// `T::default()` entity) on first fetch; re-fetch returns a handle
    /// to the same entity. Being ordinary pool state, a singleton syncs,
    /// hot-swaps, and tears down with modules like everything else.
    ///
    /// Authority rule: only the side that owns a replicated singleton
    /// may `single()` it (creation adds an entity). A replica fetches
    /// the pool with `pool()` and reads the synced entity instead.
    pub fn single<T: Default + 'static>(&mut self, name: &str) -> Single<T> {
        let handle = self.pool::<T>(name);
        let existing = handle.borrow().ids().first().copied();
        let id = match existing {
            Some(id) => id,
            None => {
                let id = self.id_add();
                handle.borrow_mut().add(id, T::default());
                id
            }
        };
        Single { handle, id }
    }

    // --- tasks ----------------------------------------------------------

    /// Register a task that runs every tick. Lowest priority runs first;
    /// tasks added from inside a task start on the next tick. A task is
    /// a closure over its captures (pool/singleton handles, counters,
    /// sockets...). It may return `()` or `Result<(), E>`; returning
    /// `Err` benches it — recorded in `task_faults`, loop survives.
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
        self.tasks.push(TaskEntry {
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

    /// Stop a task by name. If called from inside a task, takes effect
    /// at the end of the current tick.
    pub fn task_stop(&mut self, name: &str) {
        self.tasks.retain(|t| t.name != name);
        self.stop_requests.push(name.to_string());
    }

    // --- modules ----------------------------------------------------------

    /// Install a named module: a bundle of tasks, pools, and singletons
    /// that can be torn down as a unit with `module_remove`.
    ///
    /// `init` receives `&mut Pm` and registers things exactly the way
    /// `main` does — `task_add`, `pool`, `single`. While it runs (and
    /// later, while any of the module's tasks run), everything
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

    /// Tear down a module: stop its tasks and drop its pools from the
    /// store. Tasks from other modules that hold handles to a removed
    /// pool keep a working (now orphaned) pool until they drop the
    /// handle; a fresh `pool()` of the same name creates a new, empty
    /// pool. If called from inside a task, the module's tasks stop at
    /// the end of the current tick.
    pub fn module_remove(&mut self, name: &str) {
        self.tasks.retain(|t| t.module.as_deref() != Some(name));
        self.module_stops.push(name.to_string());

        let owned: Vec<String> = self
            .pool_owners
            .iter()
            .filter(|(_, owner)| &***owner == name)
            .map(|(n, _)| n.clone())
            .collect();
        for pool_name in owned {
            self.pool_owners.remove(&pool_name);
            self.pools.remove(&pool_name);
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

    /// Queue removal of every entity currently in `pool` (deferred, like
    /// `id_remove`). The "clear this part of the world" primitive — e.g.
    /// despawn all monsters and bullets on game restart.
    pub fn id_remove_all<T: 'static>(&mut self, pool: &Handle<T>) {
        for &id in pool.borrow().ids() {
            self.pending_removes.push(id);
        }
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
            for pool in self.pools.values() {
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
        for pool in self.pools.values() {
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
        for entry in &mut running {
            if entry.interval > 0.0 {
                entry.accum += dt;
                if entry.accum < entry.interval {
                    continue;
                }
                entry.accum -= entry.interval;
            }
            // Anything the task registers at runtime inherits its module.
            self.current_module = entry.module.clone();
            let started_at = Instant::now();
            let result = (entry.func)(self);
            let ns = started_at.elapsed().as_nanos() as u64;
            self.current_module = None;
            if let Err(err) = result {
                eprintln!("pm: task '{}' faulted at tick {}: {err}", entry.name, self.tick);
                entry.faulted = true;
                self.faults.push(TaskFault {
                    task: entry.name.clone(),
                    module: entry.module.as_deref().map(str::to_string),
                    tick: self.tick,
                    error: err.to_string(),
                });
            }
            match self.stats.get_mut(&entry.name) {
                Some(s) => {
                    s.calls += 1;
                    s.ns_total += ns;
                    s.ns_max = s.ns_max.max(ns);
                }
                None => {
                    self.stats
                        .insert(entry.name.clone(), TaskStat { calls: 1, ns_total: ns, ns_max: ns });
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
