# pm (Rust)

A ground-up restart of the pm kernel in Rust. Not a transliteration — same
philosophy (data-oriented, flat scheduler, sparse-set ECS, noun_verb API),
redesigned where C++ idioms don't survive the borrow checker.

## Workspace layout

```
crates/pm             the kernel: scheduler, pools, ids, net, transport, mods (NO cargo features, by design)
crates/pm_sdl         SDL companions: Sprite (png + hot-reload), Font (fontdue); re-exports sdl3
examples/hellfire     the flagship game (bin); examples/hellfire_core holds its shared/replicated types
examples/demo         8-car networked prediction demo (terminal; `--features sdl` for a window)
examples/solids       3D solids + fly camera on pm_sdl::gpu3d
examples/drive        networked 3D driving: server-authoritative, predicted local car, chase camera
examples/meteor       example dylib mod for hellfire (hot-reloads into a running server)
```

`pm` deliberately has zero cargo features: a mod dylib must link the exact
compiled pm its host links (TypeId equality), and features change crate
metadata. SDL lives in `pm_sdl` instead of behind a feature for the same
reason.

## Build & test

```bash
cargo test --workspace              # all tests (incl. QUIC loopback)
cargo run --release -p pm --example sim   # headless perf sanity check
cargo run --release -p demo         # networked demo (terminal renderer)
cargo run --release -p demo --features sdl  # same, SDL3 window
cargo run --release -p solids       # 3D solids + fly camera
cargo clippy --workspace --all-targets     # lints
```

The demo connects 8 clients to a QUIC server — 7 bots and you. Every peer
drives its own server-authoritative vehicle (input as sequenced datagrams,
state back as snapshot deltas); yours renders as an arrow showing its
heading, steered with WASD; `p` toggles a live profiling panel; `q` quits.
Run roles separately with `-- server` / `-- client` / `-- bot` (default is
everything in one process). Feel a real link on the player's connection:

```bash
PM_LAG_MS=80 PM_LOSS=0.05 cargo run --release -p demo
```

**hellfire** is the bigger example — the C++ flagship game ported: a
networked top-down wave shooter (authoritative headless server, up to 8
players, 5 score-gated monster waves to 8000 points, sprite rendering
with hot-reload, mouse-aim command-frame input):

```bash
cargo run --release -p hellfire           # play: server + 3 bots + you
cargo run --release -p hellfire server    # dedicated server
cargo run --release -p hellfire client    # join 127.0.0.1
cargo run --release -p hellfire bot 4     # headless bot
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
| `pm.pool_get<T>("name")` → `Pool<T>*` | `pm.pool::<T>("name")` → `Handle<T>` | The handle hides the `Rc<RefCell<..>>`; clone it into the task closure at init. `borrow`/`borrow_mut` lock the whole pool (iteration); `try_get(id)`/`try_mut(id)` are fallible per-entity access |
| `pm.state_get<T>("name")` → `T*` | `pm.single::<T>("name")` → `Single<T>` | There is no separate "state": a singleton is a single-entity pool, so it syncs and module-tears-down like everything else. Same singleton-on-refetch behavior; `T: Default` |
| `pm.task_add(name, prio, lambda)` | `pm.task_add(name, prio, closure)` | Same flat priority scheduler, lowest first. Tasks are closures; their "fields" are their captures |
| `pm.task_add(name, prio, interval, fn)` | `pm.task_add_every(name, prio, interval, fn)` | |
| `pool->each(fn)` + `PoolEntry::get_mut()` | `pool.iter()` (reads) / `pool.iter_mut()` (writes) | `iter_mut` yields `Mut` handles — exact `PoolEntry` semantics: stamped changed only when written through |
| `pool->get(id)` → `PoolEntry` | `pool.get(id)` / `pool.get_mut(id)` → `Option<&T>` / `Option<Mut<T>>` | `Mut` derefs like `&mut T`, stamps the changed-tick on mutable deref only |
| `pool->change_count(id)` | `pool.changed_tick(id)` / `pool.changed_since(tick)` | Tick stamps instead of counters — a peer's whole view state is one acked tick (see Networking model below) |
| TaskFault on bad access | `try_get`/`try_mut` + `?` in a fallible task | `AccessError` (`Busy`/`Missing`) flows into `pm.task_faults()`: the task is benched, the loop survives — C++ TaskFault semantics as values. The panicking `borrow` forms remain for hot paths where a conflict should be loud |
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
- **Tasks are closures, full stop.** A closure *is* an anonymous struct
  of its captures, so "struct with fields" and "closure" are the same
  machine code; the experiment with a `start`/`run`/`end` trait was
  removed because almost no task used the lifecycle (lazy-init inside
  the closure covers the rest). One registration path: `task_add` /
  `task_add_every`.
- **One erased store.** Pools live in a single
  `HashMap<String, Rc<RefCell<dyn ErasedPool>>>`; `ErasedPool: Any`
  (supertrait upcasting) lets `pool()` recover the typed
  `Rc<RefCell<Pool<T>>>` from the same entry the kernel uses for tick
  stamps and removal flushes — no parallel registries.
- **Tasks report failure as values, not panics.** A task closure may
  return `()` or `Result<(), E>` (the
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

## 3D (SDL3 GPU)

`examples/drive` (`cargo run --release -p drive`) is the full-stack
validation: the demo's netcode shape (authoritative server, one input
per tick, `pm::Predictor` rewind-replay on the client, `pool_mirror`
dead reckoning for remotes) driving 3D cars through `pm_sdl::gpu3d`
with a sprung chase camera. The sim is deliberately 2D ground-plane
physics — only the presentation is 3D. Client input runs at a FIXED
60 Hz cadence via a dt accumulator inside the net task, decoupled from
the render loop rate: prediction must step `FIXED_DT` exactly as the
server does, whatever the display refresh is.

Pacing gotcha (WSLg): the GPU swapchain is created vsync but WSLg does
not honor it — an uncapped loop free-runs (~700 fps). Windowed examples
therefore pace `pm.loop_rate` to the display's measured refresh rate
(`window.get_display().get_mode().refresh_rate`); on platforms where
vsync does block, the absolute-deadline loop just absorbs the wait.

The 3D plumbing lives in `pm_sdl::gpu3d`: `Renderer3d` (device, one
standard flat-shaded pipeline as a cull/no-cull pair, depth texture,
`upload_mesh`, `frame().draw(mesh, model, tint, cull)`),
`bake`/`box_tris`/`checker_ground` mesh helpers, and the WGSL shader
compiled to SPIR-V at build time by naga (a build-dependency — no
global toolchain, nothing committed). `Vec3`/`Mat4` were promoted into
`pm::math` once drive became their second consumer (the Vec2 rule).
Conventions in one breath: +y up, +z forward, depth 0..1, projection
bakes the Vulkan y-flip, so front faces are CLOCKWISE on screen —
author meshes CCW-from-outside and `gpu3d` handles the rest.

`examples/solids` (`cargo run --release -p solids`) is the gpu3d
playground: fly camera, spinning solids, C toggles back-face culling
live (fly under the ground to watch an open surface vanish — culling is
only free for closed meshes). SDL_gpu SPIR-V convention worth
remembering: vertex-stage uniform buffers live in descriptor set 1
(`@group(1)` in WGSL), binding = slot index passed to
`push_vertex_uniform_data`.

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
5. **Kernel polish** — mostly DONE: ~~task faults~~ (tasks may return
   `Result<(), E>` — `Err` benches the task into `pm.task_faults()` and
   the loop survives; infallible tasks keep returning `()` via the
   `IntoTaskResult` conversion trait, axum-handler style. Deliberately
   no `catch_unwind`: tasks report failure as values, panics stay
   loud), ~~**module system**~~ (named bundles of tasks + pools +
   states: `module_add(name, init)` passes `&mut Pm` to init and tags
   everything first-created — including things a module's tasks
   register at runtime — for `module_remove` teardown; init `Err`
   rolls the module back. The unit game features compose from, and
   the unit mods load as). Also done in the post-hellfire core round: `Pool::retain`
   (in-place filtered removal — no more collect-ids-then-remove),
   `Pool::iter_with`/`each_with` (joins over companion pools, callback
   style for the mutable case), `Pm::id_remove_all(&handle)` (deferred
   bulk despawn through the removal log), `pm::Outbox` (client event
   outbox as singleton data: any task queues reliable events, the net
   task — sole owner of the socket — drains and sends; replaced
   hellfire's pseudo-button hack), `pm::pool_mirror` + `coast_blend`
   (the presentation mirror both games hand-rolled: add/blend/stale-drop
   with the game supplying only blend math), and `pm::Predictor`
   (rewind-replay prediction generic over state/command pods + the
   shared step fn, hoisted from the demo). Still open: typed event
   queues (sugar over Outbox + events_drain)
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
    fails mid-save), ~~TTF text~~ (`pm::Font`: fontdue rasterization of
    system fonts + per-glyph texture cache — HUD, lobby, and the
    hellfire F1 debug overlay are built on it; overlay shows client
    frame/rtt/pools, server entity counts via the replicated `dbg`
    singleton, and live task stats). Nothing open here for now
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
    - *Tier 2 — injected mods (the sharp knife):* ~~DONE~~ —
      `pm::ModLoader`: a mod is a cdylib exporting
      `extern "C-unwind" pm_mod_abi() -> u64` (echo `pm::mod_abi()` — the
      version constant mixed with a hash of the build's `Pm` TypeId, so a
      mod built with a different toolchain, profile, or *feature set* is
      refused with a message instead of crashing in `pool()` on foreign
      TypeIds) and `pm_mod_init(&mut Pm) -> bool`; the loader installs it
      via `module_add` (so everything it registers is tagged), wraps init
      in `catch_unwind` (the one place panics are caught: foreign code
      must not take the host down; a panicking init is rolled back and
      benched), and the mtime watcher hot-swaps it on rebuild —
      `module_remove` runs before dlclose so no closure, vtable, or pool
      from the old library survives the unload. The mod links the same
      compiled `pm` + game-core crates as the host (TypeId equality), so
      it reaches the live replicated pools: see `mods/meteor` +
      `hellfire_core` — edit the meteor shower, rebuild it **with the
      same profile and features as the running game** (the server prints the exact command at startup:
      `cargo build --release -p meteor -p hellfire` — joint selection,
      because cargo resolves features per selected-package graph and a
      bare `-p meteor` can produce a different pm unit = different
      TypeIds), and watch it
      hot-swap into the running server and replicate to every client. Synchronous with
      the game tick; a panic hits the host loop (Tier 1 is the isolated
      flavor). Faults via `Result` are benched like any task.
    - *Tier 3 (maybe, later):* wasm for untrusted third-party mods —
      true sandbox, copies at the boundary, no store access.
