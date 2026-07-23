//! The Pm kernel: flat task scheduler, named component pools (singletons
//! are just single-entity pools — see `single`), entity id lifecycle with
//! end-of-tick deferred removal.
//!
//! Usage pattern: fetch pool/singleton handles during init, clone them
//! into the task closure — the [`task!`](crate::task) macro writes the
//! clone block from a capture list — and access inside the task. Tasks
//! are plain closures; a task's "state" is its captures. A closure may
//! return `Result` — an `Err` benches the task (recorded in
//! `task_faults`) and the loop survives — but that's for the task's own
//! errors; per-entity data access is just `Option` (there or not), since
//! single-threaded pm has no cross-pool contention to model.
//!
//! Two rules make gameplay tasks compose without collect-then-apply
//! ceremony: `id_add`/`id_remove` never borrow pools (removal is
//! DEFERRED to end of tick), and different pools are different
//! `RefCell`s — so spawning, killing, and writing OTHER pools are all
//! fine mid-iteration. Only touching the pool you're currently
//! iterating double-borrows; reach for [`Pool::retain`] or
//! [`Pool::each_with`] there.

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

/// Sugar for the task-registration idiom: clone the listed handles into
/// scope, then `task_add`. Every capture in the `[..]` list gets a
/// `let x = x.clone();` before the closure, so the closure's `move`
/// takes the clones (handles are cheap `Rc` bumps) and the originals
/// stay usable for the next task. Anything NOT in the list (window
/// pumps, local state) is moved as usual.
///
/// ```
/// use pm::{Pm, task};
///
/// let mut pm = Pm::server("127.0.0.1:0");
/// let score = pm.pool::<u32>("score");
/// // Without the macro this needs `{ let score = score.clone(); move |pm| ... }`.
/// task!(pm, "tally", 30.0, [score], move |_pm| {
///     let _total: u32 = score.get().values().iter().sum();
/// });
/// task!(pm, "report", 90.0, 5.0, [score], move |_pm| {
///     // interval form: runs every 5 seconds
///     let _ = score.get().len();
/// });
/// score.get_mut(); // original handle still usable after both tasks
/// pm.loop_once(1.0 / 60.0);
/// ```
#[macro_export]
macro_rules! task {
    // Every-tick form: interval defaults to 0.0.
    ($pm:expr, $name:expr, $prio:expr, [$($cap:ident),* $(,)?], $body:expr) => {
        $crate::task!($pm, $name, $prio, 0.0, [$($cap),*], $body)
    };
    ($pm:expr, $name:expr, $prio:expr, $interval:expr, [$($cap:ident),* $(,)?], $body:expr) => {{
        $(let $cap = $cap.clone();)*
        $pm.task_add($name, $prio, $interval, $body)
    }};
}

type TaskFn = Box<dyn FnMut(&mut Pm) -> Result<(), TaskError>>;

struct TaskEntry {
    name: String,
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
    pub tick: u32,
    pub error: String,
}

/// Handle to a named pool. This is what `pm.pool()` returns and what
/// task closures capture; it hides the `Rc<RefCell<..>>` plumbing.
///
/// - `get`/`get_mut` lock the whole pool for iteration. They panic on a
///   borrow conflict — single-threaded, that only happens if one task
///   borrows the same pool twice, a real bug worth a loud stop.
/// - `get_id`/`get_id_mut` reach one entity, returning `Option` — there
///   or not. (No cross-pool contention to model: a single-threaded task
///   can't hold two pools at once.)
pub struct PoolHandle<T> {
    name: Rc<str>,
    rc: Rc<RefCell<Pool<T>>>,
}

impl<T> Clone for PoolHandle<T> {
    fn clone(&self) -> Self {
        Self {
            name: self.name.clone(),
            rc: self.rc.clone(),
        }
    }
}

impl<T: 'static> PoolHandle<T> {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub(crate) fn rc(&self) -> &Rc<RefCell<Pool<T>>> {
        &self.rc
    }

    /// Whole-pool read lock (iteration). Panics if mutably borrowed.
    pub fn get(&self) -> Ref<'_, Pool<T>> {
        self.rc.borrow()
    }

    /// Whole-pool write lock (iteration/insert/remove). Panics if borrowed.
    pub fn get_mut(&self) -> RefMut<'_, Pool<T>> {
        self.rc.borrow_mut()
    }

    /// Reach one entity for reading — `None` if it isn't in the pool.
    /// Panics if the pool is already mutably borrowed (a double-borrow
    /// bug, same as `get`).
    pub fn get_id(&self, id: Id) -> Option<Ref<'_, T>> {
        Ref::filter_map(self.rc.borrow(), |p| p.get(id)).ok()
    }

    /// Reach one entity for writing — `None` if it isn't in the pool.
    /// Write-gated like everything else: the changed-tick stamps on the
    /// first mutable access through the guard, not on the lookup.
    pub fn get_id_mut(&self, id: Id) -> Option<EntryMut<'_, T>> {
        let pool = self.rc.borrow_mut();
        pool.contains(id).then_some(EntryMut { pool, id })
    }
}

/// Handle to a named singleton: one entity in an ordinary pool (there is
/// no separate "state" concept). Its one entity always exists (created
/// with the handle and protected by a pool lock), so `get`/`get_mut`
/// hand back the value directly — no `Option`. `get_mut` is write-gated
/// like a pool's [`Mut`](crate::Mut): the changed-tick stamps on the
/// first mutable access, so a task that only READS through `get_mut`
/// (or takes the guard and decides not to write) doesn't make a synced
/// singleton re-replicate.
pub struct SingleHandle<T> {
    handle: PoolHandle<T>,
    id: Id,
}

/// Write guard for one entity — what [`PoolHandle::get_id_mut`] and
/// [`SingleHandle::get_mut`] return. Holds the pool lock; reads through
/// it are free, the first mutable deref stamps the changed-tick (the
/// same write-gating as [`crate::Mut`]). The entity can't vanish while
/// the guard lives: removal is deferred to end of tick, which needs the
/// pool borrow this guard is holding.
pub struct EntryMut<'a, T: 'static> {
    pool: RefMut<'a, Pool<T>>,
    id: Id,
}

impl<T: 'static> std::ops::Deref for EntryMut<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        self.pool.get(self.id).expect("guarded entity missing")
    }
}

impl<T: 'static> std::ops::DerefMut for EntryMut<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        self.pool
            .get_mut_stamped(self.id)
            .expect("guarded entity missing")
    }
}

impl<T> Clone for SingleHandle<T> {
    fn clone(&self) -> Self {
        Self {
            handle: self.handle.clone(),
            id: self.id,
        }
    }
}

impl<T: 'static> SingleHandle<T> {
    pub fn id(&self) -> Id {
        self.id
    }

    /// The underlying pool handle (e.g. to register it for sync).
    pub fn pool(&self) -> &PoolHandle<T> {
        &self.handle
    }

    /// Read the singleton's value. Panics only on a double-borrow bug.
    pub fn get(&self) -> Ref<'_, T> {
        Ref::filter_map(self.handle.get(), |p| p.get(self.id))
            .unwrap_or_else(|_| panic!("singleton '{}' entity missing", self.handle.name()))
    }

    /// Lock the singleton for writing. The changed-tick stamps on the
    /// first mutable access through the guard, not on taking it — read
    /// first, write only what changed, and an unchanged synced singleton
    /// stays off the wire.
    pub fn get_mut(&self) -> EntryMut<'_, T> {
        EntryMut {
            pool: self.handle.get_mut(),
            id: self.id,
        }
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
    //
    // TODO(roadmap): threaded stores — parallelism as an explicit door,
    // not ambient scheduler magic. A `Pm` stays a thread (its own pools
    // `Rc<RefCell>`); a *Store* is a frozen registry of
    // `Arc<Mutex<Pool<T>>>` shared before threads spawn, so the type
    // names the locking cost at the call site. Unlocks "store mods"
    // (see modload): a mod as its own Pm + thread handed only an
    // `Arc<Store>`, for crash isolation and safe unload.
    pools: HashMap<String, Rc<RefCell<dyn ErasedPool>>>,
    tasks: Vec<TaskEntry>,
    /// ENGINE PHASES around the task cycle (see `loop_once`): PRESENT
    /// runs before every task ("the world is as fresh as the wire
    /// allows when your tasks run"), COMMIT after every task ("what
    /// you set this tick leaves this tick"). Crate-internal
    /// registration only — games order their own tasks with
    /// priorities; they can never schedule into the engine's phases,
    /// which is the point: engine/game ordering is structure, not
    /// float-literal folklore. Timed into `task_stats` by name.
    present: Vec<(String, PhaseFn)>,
    commit: Vec<(String, PhaseFn)>,
    tasks_dirty: bool,
    stop_requests: Vec<String>,
    /// Names whose currently-running task should be dropped at end of
    /// tick because `task_add_or_replace` superseded it this tick.
    replace_requests: Vec<String>,
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
    // TODO(roadmap): candidate pub(crate) — no game reads this anymore
    // (`ClientNet::peer()` is the game-facing read); it stays pub only
    // as the write seam the net task assigns at handshake.
    pub local_peer: u8,
}

#[doc(hidden)]
impl Default for Pm {
    fn default() -> Self {
        Self {
            pools: HashMap::new(),
            tasks: Vec::new(),
            present: Vec::new(),
            commit: Vec::new(),
            tasks_dirty: false,
            stop_requests: Vec::new(),
            replace_requests: Vec::new(),
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

/// An engine phase closure (see the `present`/`commit` fields on [`Pm`]).
type PhaseFn = Box<dyn FnMut(&mut Pm)>;

impl Pm {
    /// Register an engine PRESENT-phase closure (runs before the task
    /// cycle, in registration order). Crate-internal by design.
    pub(crate) fn phase_present(&mut self, name: &str, f: impl FnMut(&mut Pm) + 'static) {
        self.present.push((name.to_string(), Box::new(f)));
    }

    /// [`phase_present`](Self::phase_present), but FIRST in the phase:
    /// the net module's receive closure claims this slot at
    /// connect/serve so modifiers registered during setup (interp,
    /// ttl) always see a freshly-applied world.
    pub(crate) fn phase_present_front(&mut self, name: &str, f: impl FnMut(&mut Pm) + 'static) {
        self.present.insert(0, (name.to_string(), Box::new(f)));
    }

    /// Register an engine COMMIT-phase closure (runs after the task
    /// cycle and the removal flush, in registration order).
    pub(crate) fn phase_commit(&mut self, name: &str, f: impl FnMut(&mut Pm) + 'static) {
        self.commit.push((name.to_string(), Box::new(f)));
    }

    /// [`phase_commit`](Self::phase_commit), but FIRST in the phase:
    /// the net module's send closure claims this slot (it assigns the
    /// input seqs the predictor's replay closure consumes).
    pub(crate) fn phase_commit_front(&mut self, name: &str, f: impl FnMut(&mut Pm) + 'static) {
        self.commit.insert(0, (name.to_string(), Box::new(f)));
    }

    /// Record one timed run into `task_stats` — shared by the task loop
    /// and the phase runner (phases and tasks read out of one stats
    /// table on purpose: PM_PROF shows the whole tick, whoever ran it).
    fn stat_record(&mut self, name: &str, ns: u64) {
        match self.stats.get_mut(name) {
            Some(s) => {
                s.calls += 1;
                s.ns_total += ns;
                s.ns_max = s.ns_max.max(ns);
            }
            None => {
                self.stats.insert(
                    name.to_string(),
                    TaskStat {
                        calls: 1,
                        ns_total: ns,
                        ns_max: ns,
                    },
                );
            }
        }
    }

    /// Run one engine phase list (timed into `task_stats` per closure).
    fn phase_run(&mut self, take: impl Fn(&mut Pm) -> Vec<(String, PhaseFn)>, put: impl Fn(&mut Pm, Vec<(String, PhaseFn)>)) {
        let mut running = take(self);
        for (name, f) in &mut running {
            let started_at = Instant::now();
            f(self);
            let ns = started_at.elapsed().as_nanos() as u64;
            self.stat_record(name, ns);
        }
        // Phases registered DURING a phase (predict_pool called from a
        // setup task, say) land in the (empty) list and merge behind.
        put(self, running);
    }

    /// Bare headless kernel — pools/tasks/ids with no role. For the crate's
    /// own tests and benches; games are multiplayer-only and construct a
    /// role instead ([`Pm::server`] / [`Pm::client`]), which is why this is
    /// hidden from the docs.
    #[doc(hidden)]
    pub fn new() -> Self {
        Self::default()
    }

    // --- pools & singletons ---------------------------------------------

    /// Named sparse-set component pool. Created on first fetch.
    pub fn pool<T: 'static>(&mut self, name: &str) -> PoolHandle<T> {
        if let Some(rc) = self.pools.get(name) {
            let typed = downcast_pool::<T>(rc).unwrap_or_else(|| {
                panic!("pool '{name}' already registered with a different type")
            });
            return PoolHandle {
                name: Rc::from(name),
                rc: typed,
            };
        }
        let rc = Rc::new(RefCell::new(Pool::<T>::new()));
        rc.borrow_mut().tick_set(self.tick);
        let erased: Rc<RefCell<dyn ErasedPool>> = rc.clone();
        self.pools.insert(name.to_string(), erased);
        PoolHandle {
            name: Rc::from(name),
            rc,
        }
    }

    /// Named singleton: a single-entity pool. Created (with one
    /// `T::default()` entity) on first fetch; re-fetch returns a handle
    /// to the same entity. Being ordinary pool state, a singleton syncs,
    /// hot-swaps, and tears down with modules like everything else.
    ///
    /// Authority rule: only the side that owns a replicated singleton
    /// may `single()` it (creation adds an entity). A replica fetches
    /// the pool with `pool()` and reads the synced entity instead.
    pub fn single<T: Default + 'static>(&mut self, name: &str) -> SingleHandle<T> {
        let handle = self.pool::<T>(name);
        let existing = handle.get().ids().first().copied();
        let id = match existing {
            Some(id) => id,
            None => {
                let id = self.id_add();
                let mut pool = handle.get_mut();
                pool.add(id, T::default());
                // A singleton's entity is permanent: lock the pool so the
                // id-removal flush can never drop it from under the handle.
                pool.lock();
                id
            }
        };
        SingleHandle { handle, id }
    }

    /// Protect a pool from the end-of-tick id-removal flush (see
    /// [`Pool::lock`]). `single` does this automatically; call it
    /// explicitly to pin a regular pool's entities.
    pub fn pool_lock(&mut self, name: &str) {
        if let Some(rc) = self.pools.get(name) {
            rc.borrow_mut().lock();
        }
    }

    // --- tasks ----------------------------------------------------------

    /// Register a task. Lowest priority runs first; `interval` is
    /// seconds between runs (0.0 = every tick); tasks added from inside
    /// a task start on the next tick. A task is a closure over its
    /// captures (pool/singleton handles, counters, sockets...). It may
    /// return `()` or `Result<(), E>`; returning `Err` benches it —
    /// recorded in `task_faults`, loop survives.
    pub fn task_add<R: IntoTaskResult>(
        &mut self,
        name: &str,
        priority: f32,
        interval: f32,
        mut func: impl FnMut(&mut Pm) -> R + 'static,
    ) {
        self.tasks.push(TaskEntry {
            name: name.to_string(),
            priority,
            interval,
            accum: 0.0,
            faulted: false,
            func: Box::new(move |pm| func(pm).into_task_result()),
        });
        self.tasks_dirty = true;
    }

    /// Register a task, replacing any existing task with the same name
    /// (its old closure is dropped). The dev-hot-reload primitive: a
    /// reloaded module re-runs its init with this, swapping each task's
    /// body in place — no teardown bookkeeping, and pools keep their
    /// data across the reload. If the old task is mid-run this tick, it's
    /// dropped at end of tick and the new one starts next tick.
    pub fn task_add_or_replace<R: IntoTaskResult>(
        &mut self,
        name: &str,
        priority: f32,
        interval: f32,
        func: impl FnMut(&mut Pm) -> R + 'static,
    ) {
        self.tasks.retain(|t| t.name != name);
        // Covers the case where the old task is in this tick's running
        // set (taken out of self.tasks): drop it during the end-of-tick
        // merge so it doesn't come back alongside the replacement.
        self.replace_requests.push(name.to_string());
        self.task_add(name, priority, interval, func);
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
        let mut v: Vec<_> = self
            .stats
            .iter()
            .map(|(n, s)| (n.clone(), s.clone()))
            .collect();
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

    /// Drop a pool from the store by name (e.g. a hot-reloaded mod
    /// clearing its own private pools). Tasks still holding a handle to
    /// it keep a working, now-orphaned pool until they drop it; a fresh
    /// `pool()` of the same name makes a new empty one.
    pub fn pool_remove(&mut self, name: &str) {
        self.pools.remove(name);
    }

    /// Every pool in the store by name with its live entity count,
    /// largest first (name-ordered within a count) — the introspection
    /// seam for debug overlays and, later, a live console. Singles are
    /// one-entity pools, so they show up too. Callable from inside a
    /// task; panics only if some pool is mutably borrowed at the call
    /// (a task holding a lock across it — don't).
    pub fn pool_stats(&self) -> Vec<(String, usize)> {
        let mut v: Vec<_> = self
            .pools
            .iter()
            .map(|(n, p)| (n.clone(), p.borrow().erased_len()))
            .collect();
        v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        v
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
    pub fn id_remove_all<T: 'static>(&mut self, pool: &PoolHandle<T>) {
        for &id in pool.get().ids() {
            self.pending_removes.push(id);
        }
    }

    /// Accept a remote id (networking): mark alive, record its generation.
    pub(crate) fn id_sync(&mut self, id: Id) {
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
        // PRESENT: the engine brings the world up to date (receive +
        // apply + interp + reconcile) before any game task runs.
        self.phase_run(
            |pm| std::mem::take(&mut pm.present),
            |pm, mut v| {
                v.append(&mut pm.present);
                pm.present = v;
            },
        );
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
            let started_at = Instant::now();
            let result = (entry.func)(self);
            let ns = started_at.elapsed().as_nanos() as u64;
            if let Err(err) = result {
                eprintln!(
                    "pm: task '{}' faulted at tick {}: {err}",
                    entry.name, self.tick
                );
                entry.faulted = true;
                self.faults.push(TaskFault {
                    task: entry.name.clone(),
                    tick: self.tick,
                    error: err.to_string(),
                });
            }
            self.stat_record(&entry.name, ns);
        }
        running.retain(|t| !t.faulted);
        running.append(&mut self.tasks);
        self.tasks = running;
        // For each task_add_or_replace this tick, keep only the most
        // recent task of that name (appended last) and drop any older
        // copy — including one still in this tick's running set. Both
        // share the name, so we dedup by position, not by name-inequality.
        for name in std::mem::take(&mut self.replace_requests) {
            if let Some(last) = self.tasks.iter().rposition(|t| t.name == name) {
                let mut i = 0;
                self.tasks.retain(|t| {
                    let keep = t.name != name || i == last;
                    i += 1;
                    keep
                });
            }
        }
        for name in std::mem::take(&mut self.stop_requests) {
            self.tasks.retain(|t| t.name != name);
        }

        // Removal flush BEFORE commit: entities removed by this tick's
        // tasks (or TTL) enter the removal log now, so the commit
        // phase's snapshots carry them THIS tick, not next.
        self.id_process_removes();
        // COMMIT: the engine ships what the tick produced (inputs set
        // by the game, journal frame, snapshot flights) — what you set
        // this tick leaves this tick.
        self.phase_run(
            |pm| std::mem::take(&mut pm.commit),
            |pm, mut v| {
                v.append(&mut pm.commit);
                pm.commit = v;
            },
        );
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
