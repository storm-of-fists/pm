# pm (Rust)

A ground-up restart of the pm kernel in Rust. Not a transliteration — same
philosophy (data-oriented, flat scheduler, sparse-set ECS, noun_verb API),
redesigned where C++ idioms don't survive the borrow checker.

## Build & test

```bash
cd src_rust
cargo test                          # all tests (incl. QUIC loopback)
cargo run --release --example sim   # headless perf sanity check
cargo run --release --example demo  # networked demo (terminal renderer)
cargo run --release --features sdl --example demo  # same, SDL3 window
cargo clippy --all-targets          # lints
```

The demo connects 8 clients to a QUIC server — 7 bots and you. Every peer
drives its own server-authoritative vehicle (input as sequenced datagrams,
state back as snapshot deltas); yours renders as an arrow showing its
heading, steered with WASD; `p` toggles a live profiling panel; `q` quits.
Run roles separately with `-- server` / `-- client` / `-- bot` (default is
everything in one process). Feel a real link on the player's connection:

```bash
PM_LAG_MS=80 PM_LOSS=0.05 cargo run --release --example demo
```

**hellfire** is the bigger example — the C++ flagship game ported: a
networked top-down wave shooter (authoritative headless server, up to 8
players, 5 score-gated monster waves to 8000 points, sprite rendering
with hot-reload, mouse-aim command-frame input):

```bash
cargo run --release --features sdl --example hellfire           # play: server + 3 bots + you
cargo run --release --example hellfire server                   # dedicated server
cargo run --release --features sdl --example hellfire client    # join 127.0.0.1
cargo run --release --example hellfire bot 4                    # headless bot
```

WASD moves, mouse aims, left-click/space shoots, R restarts after game
over, Esc quits. Edit `examples/hellfire/resources/*.png` while it runs
to see sprite hot-reload. Monsters/bullets are change-dense pools
streaming through the snapshot byte budget; players/status/roster are
change-sparse and converge — same replication mechanism (see Networking
model).

## Profiling

- `pm.task_stats()` — always-on per-task timing (calls / total / max ns),
  collected by `loop_once` at ~80 ns overhead per task call;
  `task_stats_reset()` to window it. The demo's `p` panel and the
  dedicated server's 5-second log are built on this.
- `pm::probe::scope("name")` — drop-in scoped probe for hot spots *inside*
  a task; thread-local, read with `pm::probe::stats()`.
- `QuicServer/QuicClient::link_lag_set(delay, loss)` — simulate link
  conditions both directions; QUIC's RTT/loss handling reacts as if real.

## Mapping from C++

| C++ | Rust | Notes |
|-----|------|-------|
| `pm.pool_get<T>("name")` → `Pool<T>*` | `pm.pool_get::<T>("name")` → `Rc<RefCell<Pool<T>>>` | Clone the handle into the task closure at init, `borrow_mut()` inside the task |
| `pm.state_get<T>("name")` → `T*` | `pm.state_get::<T>("name")` → `Rc<RefCell<T>>` | Same singleton-on-refetch behavior; `T: Default` |
| `pm.task_add(name, prio, lambda)` | `pm.task_fn(name, prio, closure)` | Same flat priority scheduler, lowest first |
| `pm.task_add(name, prio, interval, fn)` | `pm.task_fn_every(name, prio, interval, fn)` | |
| — | `pm.task_add(name, prio, impl Task)` | **New:** struct tasks with a `start`/`run`/`end` lifecycle; handles live in named fields. Closures are the same thing implicitly (captures = fields), so `task_fn` is sugar over the same machinery |
| `pool->each(fn)` + `PoolEntry::get_mut()` | `pool.iter()` (reads) / `pool.iter_mut()` (writes) | `iter_mut` yields `Mut` handles — exact `PoolEntry` semantics: stamped changed only when written through |
| `pool->get(id)` → `PoolEntry` | `pool.get(id)` / `pool.get_mut(id)` → `Option<&T>` / `Option<Mut<T>>` | `Mut` derefs like `&mut T`, stamps the changed-tick on mutable deref only |
| `pool->change_count(id)` | `pool.changed_tick(id)` / `pool.changed_since(tick)` | Tick stamps instead of counters — a peer's whole view state is one acked tick (see Networking model below) |
| TaskFault on bad access | `RefCell` borrow panic / `Option` | A borrow panic means two tasks held the same pool mutably — a real bug either way |
| `pm.id_add(peer)` | `pm.id_add()` / `pm.id_add_for(peer)` | **Diverges:** generational `[peer:8 \| gen:8 \| index:16]`, FIFO-recycled, recycling gated by the removal log — bounded memory, stale handles fail the gen check |
| `pm.id_remove(id)` | same | Deferred, flushed across all pools at end of `loop_once`, logged for sync |
| `NetSys` (`pm_udp.hpp`) | `NetServer` / `NetClient` + `QuicServer` / `QuicClient` | Snapshot-delta replication over QUIC (quinn-proto, sans-IO — no async runtime) |
| `loop_run`, `loop_dt`, `loop_quit`, `loop_rate` | same | `loop_once(dt)` is public for headless/test driving |

## Design decisions

- **`Rc<RefCell<...>>` handles instead of raw pointers.** The C++ pattern
  ("fetch during init, capture the pointer in the lambda") maps directly:
  fetch during init, clone the `Rc` into the closure. Borrows are checked at
  runtime; the cost is one counter check per `borrow_mut()`, done once per
  task per tick — not per entity — so it doesn't show up in the hot loop.
- **Single-threaded kernel for now.** Parallel `each` needs a different
  mechanism in Rust (rayon over dense slices, or task-declared pool access).
  Decide after the kernel feels right, not before.
- **Tasks take `&mut Pm`.** The scheduler moves the task list out of `Pm`
  during a tick so tasks can borrow the kernel mutably (`id_add`, `loop_dt`,
  `task_add`, `loop_quit`). Tasks added mid-tick start next tick.
- **Tasks are `Box<dyn Task>` with a lifecycle.** `start` runs once on the
  first scheduled tick (fallible — an `Err` benches the task before it
  ever runs), `run` every scheduled tick, `end` exactly once when the
  task leaves the schedule (`task_stop`, `module_remove`, a fault from
  `run`, or `loop_run` draining the schedule after `loop_quit`). Pool and
  state handles live in fields, fetched in the constructor or in `start`.
  Closure tasks (`task_fn`) wrap into the same machinery — a closure *is*
  an anonymous struct of its captures, so the two registration paths are
  performance-identical (~80 ns/tick fixed overhead either way; see
  `examples/taskbench.rs`). `task_fn` keeps a direct `FnMut` bound rather
  than a blanket `impl Task for F` because closure parameter inference
  and higher-ranked lifetimes only work through `Fn`-family bounds.
- **Tasks report failure as values, not panics.** `start`/`run` return
  `Result<(), TaskError>`; closures may return `()` or `Result` (the
  `IntoTaskResult` conversion trait — axum-handler-style return
  polymorphism). A faulted task lands in `pm.task_faults()` with its
  module, tick, and error; the loop survives. Deliberately no
  `catch_unwind`: a panic (e.g. a `RefCell` double-borrow) is a bug and
  stays loud.

## Networking model (implemented)

Server-authoritative snapshot-delta, the Quake/Source/Overwatch lineage.
The design doc that drove this (SYNC_DESIGN.md) is retired; what it decided
is now code, summarized here:

- **Tick-versioned change tracking.** The kernel tick stamps every pool
  entry on insert/mutation (`Mut` guard: stamped only when written
  through). Adds are upserts — snapshots are idempotent, loss just means
  resend.
- **Per-entity confirmation, byte-budgeted snapshots.** Per peer and
  entity slot the server tracks the confirmed change-tick (peer acked a
  snapshot carrying it) and the in-flight one (sent, ack pending); an
  entry packs when it has changed past both, in rotation order, until
  the budget (`snapshot_budgeted` + `QuicServer::snapshot_budget`) runs
  out. An ack confirms exactly that snapshot's entries and declares
  older unacked snapshots lost (entries resend); a silent ack gap
  expires in-flight state after 60 ticks. This one mechanism covers both
  replication temperaments: change-sparse pools converge to silence
  (delta), change-dense pools larger than the budget stream through it
  round-robin with bounded staleness that dead reckoning hides (the
  Tribes prioritized-replication shape, vs Quake 3's per-client
  snapshot diffs). No per-pool mode setting — behavior emerges from
  change density vs budget. If bandwidth ever pinches, quantize by
  making the synced pool's component type compact (i16 positions); the
  replicated pool *is* the wire format.
- **Removal log gates id recycling.** Removed indices return to the free
  list only after every peer acked the removal, so a recycled id can never
  race its predecessor's death on the wire. Generational ids catch stale
  local handles.
- **QUIC via quinn-proto, driven synchronously.** Unreliable datagrams
  carry snapshots, acks, and input; one reliable bi stream per connection
  carries the schema-checked handshake and typed events. Keep-alive 2 s,
  idle timeout 5 s (dead clients reap; their entities despawn).
- **Command-frame input.** Sequenced input datagrams (last 8 ride along
  redundantly); the server consumes one per tick and echoes the applied
  seq in every snapshot — the client's prediction reconciles against
  exactly that (rewind + replay; see the demo's reference implementation).
- **State vs events rule.** If a late joiner needs to know it, it is pool
  state — replication is the multicast. Facts that outlive the ack gap
  belong in state with a TTL ("explosion at x,y,tick", ~2 s). Only true
  must-see instants ride the reliable event stream. Failure of a predicted
  request needs no message: state never confirms it and reconciliation
  rolls it back.

Known limits, deliberate until a workload demands them: per-peer pack
scan is O(entities) per net tick (dirty lists if profiles ever say so);
removals always pack in full (tiny, but unbounded in principle); interest
management, lag-compensation history, per-pool priority weights, and
reconnect/peer-id reassignment are future sync-layer work; u32 ticks
last ~2.2 years.

## Threaded stores (design sketch, not built)

Threading is a choice with a price tag, so it gets a marked door — not
ambient scheduler parallelism. The sketch:

- A `Pm` is a thread. Its own pools/states stay `Rc<RefCell>` — private,
  zero-cost, exactly today's model.
- A **Store** is the explicit shared thing: a registry of named pools,
  states, and event queues created before threads spawn and **frozen** —
  the registry itself is read-only and lock-free to access. A `Pm` holds
  an `Arc<Store>` and passes it to child `Pm`s it spawns.
- **Locks live on each pool/state, not the store**: `Arc<Mutex<Pool<T>>>`
  per entry (or channels for queues). The type names the cost at the call
  site.
- **Passive phase alignment instead of clever locking.** The
  absolute-deadline scheduler gives every loop a stable phase (~1 us
  median jitter). Each loop counts `try_lock` contentions; on contention
  it nudges its deadline phase by a small random jostle, and stays put
  when quiet. No coordinator, no per-task tuning to start — whole-loop
  phase shifts should let the system anneal globally into a
  non-interfering schedule, desynchronizing like organisms partitioning a
  niche. The mutex remains the correctness backstop; alignment is purely a
  performance optimizer. If contention can't anneal to zero, the counters
  are telling you the workload genuinely needs more cores or sharding.
- Open questions for later: per-task phase offsets within a tick (only if
  whole-loop nudging proves insufficient), RwLock vs Mutex per access
  pattern, and double/triple-buffered snapshots for latest-wins readers.

## Perf

`cargo run --release --example sim`: 100k entities × 600 ticks,
velocity→position join (dense iteration + sparse lookup per entity):
**~2.3 ns per entity-update** on the same 20-core WSL reference machine.
(~1.2 ns before sync foundations; the difference is the generation check
per lookup and the `Mut` guard's changed-tick stamps — the price of
network-ready change tracking, still <1% of a 60 Hz frame at 100k
entities.)

## Roadmap

Multiplayer sync is the core concern and came first — the model is
implemented (see Networking model above). The hellfire port (first cut)
landed: server/client/bot modes on the QUIC stack, struct tasks with
the start/run/end lifecycle, sprite hot-reload. Still missing from C++
parity: menu/lobby UI, bitmap-font text + debug overlay, diag JSON
reports + smoke script, camera zoom/follow, mod hot-reload (item 12).

1. ~~**Sync foundations**~~ — DONE: kernel tick + removal-log-gated id
   recycling, generational ids, `changed_tick` + `Mut` guard,
   `changed_since(tick)` query
2. ~~**NetSys headless**~~ — DONE: `NetServer`/`NetClient`, ack-cursor
   deltas, removal replication, ack-gated recycling; proven by two `Pm`
   instances converging through in-memory queues under packet loss
3. ~~**QUIC transport**~~ — DONE: `QuicServer`/`QuicClient` over
   quinn-proto (sans-IO, no async runtime), pumped by an ordinary net
   task; schema-checked handshake, snapshot datagrams + acks, typed
   events on the reliable stream. Try it: `cargo run --release
   --example demo`
4. ~~**Client conventions**~~ — DONE (reference implementation in the
   demo): sequenced redundant input datagrams; server command-frame model
   (one input per tick, echo of the applied seq); client prediction with
   rewind+replay reconciliation against the echo; dead reckoning + blend
   for remote entities; shared fixed-dt step function. Hoist into a
   `pm::predict` helper once a second game proves the shape.
5. **Kernel polish** — partly DONE: ~~task faults~~ (tasks may return
   `Result<(), E>` — `Err` benches the task into `pm.task_faults()` and
   the loop survives; infallible tasks keep returning `()` via the
   `IntoTaskResult` conversion trait, axum-handler style. Deliberately
   no `catch_unwind`: tasks report failure as values, panics stay
   loud), ~~**module system**~~ (named bundles of tasks + pools +
   states: `module_add(name, init)` passes `&mut Pm` to init and tags
   everything first-created — including things a module's tasks
   register at runtime — for `module_remove` teardown; init `Err`
   rolls the module back. The unit game features compose from, and
   the unit mods load as). Still open: `remove_all` (deferred),
   `clear_world`, typed event queues, join iterator (`each_with`)
6. ~~**Math + util**~~ — DONE: `Vec2` (std::ops operators, `Pod` so it
   nests in replicated components), `Rng` (xorshift32), the PLC helpers
   (Hysteresis/Cooldown/DelayTimer/RisingEdge/FallingEdge/Latch/Counter),
   and `SpatialGrid` (one cell per entity, exact-distance query, zero
   steady-state allocation) — straight ports of pm_math/pm_util/
   pm_spatial_grid with unit tests
7. **Benchmarks** — threshold-based regression gates like the C++ suite
8. **Parallel iteration** — rayon over dense slices behind an explicit opt-in
9. ~~**SDL3**~~ — DONE (first cut): `sdl3` crate behind the `sdl`
   feature, built from source (pin: sdl3-src must match sdl3-sys's SDL
   version). The demo's SDL client shares all netcode with the terminal
   client via `add_client_tasks` — only input and rendering differ; the
   pm loop drives SDL directly, no second event loop. Needs system
   X11/Wayland dev headers (notably libxtst-dev, libxss-dev).
10. **Threaded stores** — explicit opt-in shared state between `Pm`
    threads with passive phase alignment (see design sketch above)
11. **Assets & debug** — partly DONE: ~~sprite loading with mtime
    hot-reload~~ (`pm::Sprite` behind the `sdl` feature: pure-Rust `png`
    decode — no SDL3_image build dep — `unsafe_textures` for storable
    textures, `changed()`/`reload()` keep the old texture when a load
    fails mid-save). Still open: on-screen debug overlay (task table,
    entity stats) on top of the existing `task_stats`/probe machinery
12. **Mods** — tiered, built on the module system (item 5) + threaded
    store (item 10):
    - *Tier 1 — store mods (the default):* a mod is a dylib exposing one
      entry symbol; the host spawns it as its **own `Pm` on its own
      thread**, handing it only `Arc<Store>`. Crash isolation via
      `catch_unwind` + thread death; unload = quit loop, join, drop lib —
      safe to dlclose because the host never holds mod code, only Pod
      data the mod left in shared pools. ABI exposure is minimized: shared
      *data* is `repr(C)` Pod (networking already requires this); only the
      container types ride the same-toolchain contract (mods built by the
      pinned toolchain, like every Rust studio's internal hot-reload).
    - *Tier 2 — injected mods (the sharp knife):* a module bundle loaded
      INTO an existing `Pm` (tasks in the host scheduler). Synchronous
      with the game tick, but the host holds mod closures, so unload
      discipline is C++-style (`task_stop` before dlclose) and a panic
      hits the host loop. Opt-in per mod.
    - *Tier 3 (maybe, later):* wasm for untrusted third-party mods —
      true sandbox, copies at the boundary, no store access.
