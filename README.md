# pm

A data-oriented game framework in Rust: flat task scheduler, sparse-set
pools, noun_verb API, and networking as a first-class core concern —
server-authoritative replication, prediction, and hot-reload mods are
built in, not bolted on.

## Workspace layout

```
crates/pm             the kernel: scheduler, pools, ids, math, net, transport, mods
crates/pm_sdl         SDL3 companions: Sprite (png + hot-reload), Font (fontdue), gpu3d; re-exports sdl3
examples/demo         8-car networked prediction reference (terminal renderer — works over ssh)
examples/drive        networked 3D driving: predicted local car, chase camera, gpu3d
examples/solids       3D solids + fly camera; gpu3d playground
examples/hellfire     the flagship game: wave shooter, sprites, lobby, HUD, mods
examples/hellfire_core  hellfire's shared/replicated types (a crate so mods can link them)
examples/meteor       example dylib mod that hot-reloads into a running hellfire server
```

`pm` has **zero cargo features, by design**: a mod dylib must link the
exact compiled pm its host links (TypeId equality), and features change
crate metadata. SDL lives in the `pm_sdl` crate instead of behind a
feature for the same reason.

## Build & run

```bash
cargo test --workspace                     # all tests (incl. QUIC loopback)
cargo clippy --workspace --all-targets     # lints

cargo run --release -p demo                # networked prediction demo (terminal)
cargo run --release -p drive               # networked 3D driving (SDL window)
cargo run --release -p solids              # 3D solids + fly camera
cargo run --release -p hellfire            # wave shooter: server + 3 bots + you

cargo run --release -p pm --example sim    # perf sanity check
cargo run --release -p pm --example taskbench  # pool-access micro-bench
examples/hellfire/smoke_test.sh            # headless end-to-end gate
```

Every networked example takes a role argument (`server` / `client` /
`bot`); no argument runs everything in one process. Simulate a real link
on the player's connection with `PM_LAG_MS=80 PM_LOSS=0.05`.

- **demo** — the netcode reference. 8 peers, server-authoritative cars,
  client prediction with rewind+replay reconciliation, dead reckoning.
  WASD-ish keys (latched throttle — terminals only report presses),
  `p` profiling panel, `q` quits.
- **drive** — the same netcode shape in 3D: predicted local car, sprung
  chase camera, budget-rotated snapshots, gpu3d rendering. WASD, Esc.
- **hellfire** — the flagship: up to 8 players, 5 score-gated waves to
  8000 points, mouse aim, sprite hot-reload (edit
  `examples/hellfire/resources/*.png` while it runs), lobby, F1 debug
  overlay, JSON diag reports, and dylib mod hot-reload (see Mods).
- **solids** — fly camera (WASD+EQ, hold RMB to look, wheel = speed),
  C toggles back-face culling live.

## API at a glance

```rust
let mut pm = Pm::new();
let car = pm.pool::<Car>("car");       // Handle<Car>: named, typed pool
let cfg = pm.single::<Cfg>("cfg");     // Single<Cfg>: single-entity pool

let id = pm.id_add();                  // generational id [peer|gen|index]
car.borrow_mut().add(id, Car::default());
pm.id_remove(id);                      // deferred: flushed end-of-tick, logged for sync

pm.task_add("drive", 30.0, move |pm| { // flat priority order, lowest first
    let dt = pm.loop_dt();
    for (id, mut c) in car.borrow_mut().iter_mut() { /* ... */ }
});
pm.task_add_every("status", 90.0, 5.0, move |pm| { /* every 5 s */ });

pm.loop_rate = 60;                     // absolute-deadline pacing (0 = uncapped)
pm.loop_run();
```

- Tasks are closures, full stop — a closure is an anonymous struct of its
  captures, so "task with fields" and "closure" are the same machine
  code. Clone handles in at registration; tasks take `&mut Pm`.
- A task may return `()` or `Result<(), E>` (the `IntoTaskResult`
  conversion, axum-handler style). `Err` benches the task into
  `pm.task_faults()` and the loop survives. Deliberately no
  `catch_unwind`: panics are bugs and stay loud.
- Access: `borrow`/`borrow_mut` lock the whole pool and panic on
  conflict (right for hot paths); `try_get`/`try_mut`/`try_borrow*`
  return `AccessError` (`Busy`/`Missing`) which `?`s into a task fault.
- Writes through the `Mut` guard stamp the entry's changed-tick — the
  change detection replication runs on. Compound assignment through a
  guard can't split-borrow (`c.pos += c.vel * dt` won't compile): read
  locals first, then write.
- Joins: `a.iter_with(&b)` (read), `a.each_with(&mut b, |id, a, b| ..)`
  (write, callback style — a streaming two-`Mut` iterator can't be
  expressed safely). In-place filtered removal: `pool.retain(..)` for
  local pools, deferred `pm.id_remove`/`pm.id_remove_all(&handle)` for
  anything replicated.
- Networking is module-shaped: after registering synced pools, hand the
  endpoint to `net.serve::<C>(pm, quic)` (server) or
  `net.connect::<C>(pm, quic, input_hz)` (client) and write gameplay
  against the published `"net.*"` singles (see Networking model).
- Presentation helpers: `pm::pool_mirror` (authoritative pool → draw
  pool: add/blend/stale-drop) + `coast_blend` (dead-reckoning math);
  `pm::Predictor<S, C>` (client prediction: rewind + replay against the
  server's input echo, driven by the net module's sent/applied logs).
- `module_add(name, init)` groups everything a feature registers —
  tasks, pools, runtime additions — for one-call `module_remove`
  teardown. The unit games compose from and mods load as.
- Math/util: `Vec2`/`Vec3`/`Mat4` (Pod, operator overloads, column-major),
  `Rng` (xorshift32), `SpatialGrid`, PLC helpers (Hysteresis, Cooldown,
  edges, …), `pm::probe::scope` drop-in profiling.

## Design decisions

- **`Rc<RefCell<..>>` behind handles, not raw pointers.** Fetch at init,
  clone into the closure. Borrow checks are runtime but per-task-per-tick
  (one counter check), not per-entity — invisible in the hot loop.
- **Single-threaded kernel.** Parallelism will be an explicit door
  (threaded stores, below), not ambient scheduler magic.
- **One erased store.** Pools live in a single
  `HashMap<String, Rc<RefCell<dyn ErasedPool>>>`; supertrait upcasting
  (`ErasedPool: Any`) recovers the typed pool from the same entry the
  kernel uses for tick stamps and removal flushes. No parallel
  registries, and no separate "state" concept — a singleton is a
  single-entity pool, so it replicates and tears down like everything
  else.
- **The replicated pool is the wire format.** Synced components are
  `Pod`; if bandwidth pinches, make the component compact (i16
  positions) rather than inventing a serializer.

## Networking model

Server-authoritative snapshot-delta, the Quake/Source/Overwatch lineage:

- **Tick-versioned change tracking.** Every pool entry is stamped on
  insert/mutation. Adds are upserts — snapshots are idempotent, loss
  just means resend.
- **Per-entity confirmation, byte-budgeted snapshots.** Per peer and
  entity slot the server tracks the confirmed change-tick and the
  in-flight one; an entry packs when it has changed past both, in
  rotation order, until the byte budget runs out
  (`snapshot_budgeted` + `QuicServer::snapshot_budget`). An ack confirms
  exactly that snapshot's entries and declares older ones lost; a silent
  gap expires in-flight state after 60 ticks. One mechanism, both
  temperaments: change-sparse pools converge to silence, change-dense
  pools stream through the budget round-robin with bounded staleness
  that dead reckoning hides.
- **Removal log gates id recycling.** A removed index is reused only
  after every peer acked the removal — a recycled id can never race its
  predecessor's death on the wire.
- **QUIC via quinn-proto, driven synchronously** (sans-IO, no async
  runtime, pumped by an ordinary net task). Unreliable datagrams carry
  snapshots/acks/input; one reliable stream carries the schema-checked
  handshake and typed events. Idle timeout reaps dead clients.
- **Command-frame input.** Sequenced input datagrams (last 8 ride along
  redundantly); the server consumes one per tick and echoes the applied
  seq; the client's `Predictor` reconciles against exactly that. Both
  sides step the same function at the same FIXED_DT — determinism is
  what makes reconciliation exact.
- **The net modules own the transport.** `NetServer::serve::<C>` /
  `NetClient::connect::<C>` (where `C` is the input pod) move the QUIC
  endpoint into one net task and publish plain data — games read and
  write `"net.*"` singles and never touch the socket. Server:
  `PeerEvents` (joins/leaves), `Commands<C>` (per-peer input queues;
  `pop` = command-frame, `latest` = newest-wins; consuming records the
  echoed seq), `ServerEvents` in, `ServerOutbox` out. Client:
  `NetStatus`, `NetInput<C>` (sent at a fixed cadence, decoupled from
  render rate), `SentLog<C>` + `AppliedLog` (drive a `Predictor` from
  them), `ClientEvents` in, `Outbox` out (held until the handshake).
  Per-tick singles are cleared and refilled by the net task at priority
  `NET_PRIO` (5.0) — read them from tasks above that. Registered via
  `module_add("net_server" | "net_client")`, so disconnecting is
  `module_remove`. The drive server is the reference: its entire
  netcode is "spawn a car per `PeerEvents` join, `cmds.pop(peer)` in
  the sim".
- **State vs events rule.** If a late joiner needs to know it, it is
  pool state (with a TTL if it's a fact that fades). Only true must-see
  instants ride the reliable event stream.

Known limits, deliberate until a workload demands otherwise: per-peer
pack scan is O(entities) per net tick; interest management,
lag-compensation history, and reconnect/peer-id reassignment are future
work; u32 ticks last ~2.2 years.

## 3D (SDL3 GPU)

`pm_sdl::gpu3d`: `Renderer3d` (device, flat-shaded pipeline in a
cull/no-cull pair, depth texture, `upload_mesh`,
`frame().draw(mesh, model, tint, cull)`), `bake`/`box_tris`/
`checker_ground` helpers, WGSL shader compiled to SPIR-V at build time
by naga (a build-dependency — no global toolchain, nothing committed).

Conventions in one breath: +y up, +z forward, depth 0..1, the projection
bakes the Vulkan y-flip, so front faces are CLOCKWISE on screen — author
meshes CCW-from-outside and gpu3d handles the rest. SDL_gpu SPIR-V
quirk: vertex-stage uniforms live in descriptor set 1 (`@group(1)` in
WGSL); binding = the slot passed to `push_vertex_uniform_data`.

Pacing gotcha (WSLg): the swapchain is created vsync but WSLg does not
honor it — an uncapped loop free-runs (~700 fps). Windowed examples pace
`pm.loop_rate` to the display's measured refresh rate; where vsync does
block, the absolute-deadline loop absorbs the wait. Client input/
prediction runs at a FIXED 60 Hz cadence via a dt accumulator regardless
of render rate (drive's client is the reference).

## Mods (dylib hot-reload)

A mod is a cdylib exporting `pm_mod_abi() -> u64` (echo `pm::mod_abi()`)
and `pm_mod_init(&mut Pm) -> bool`. `pm::ModLoader` watches the .so,
checks the ABI (a hash of the build's `Pm` TypeId — a mod built with a
different toolchain, profile, or dependency resolution is refused with a
message instead of crashing on foreign TypeIds), installs init via
`module_add`, and hot-swaps on rebuild — `module_remove` runs before
`dlclose` so nothing from the old library survives. Init is wrapped in
`catch_unwind` (the one place panics are caught: foreign code must not
take the host down).

The contract: build the mod **jointly with its host, same profile** —
`cargo build --release -p meteor -p hellfire`. Cargo resolves features
per selected-package graph, so a bare `-p meteor` can produce a
different pm unit (different TypeIds); joint selection pins it. The
hellfire server prints the exact command at startup. Try it: run
hellfire, edit `examples/meteor/src/lib.rs`, rebuild, watch the meteor
shower hot-swap in and replicate to every client.

## Profiling

- `pm.task_stats()` — always-on per-task timing (~80 ns overhead per
  task call); `task_stats_reset()` to window it. The demo's `p` panel,
  hellfire's F1 overlay, and the server 5-second logs are built on it.
- `pm::probe::scope("name")` — drop-in scoped probe inside a task;
  read with `pm::probe::stats()`.
- `link_lag_set(delay, loss)` on either QUIC end — simulated link;
  QUIC's RTT/loss handling reacts as if real.

## Perf

`cargo run --release -p pm --example sim`: 100k entities × 600 ticks,
velocity→position join via `each_with` (dense iteration + sparse lookup
per entity): **~1.7 ns per entity-update** on the 20-core WSL reference
machine — change tracking included, <1% of a 60 Hz frame at 100k
entities.

## Threaded stores (design sketch, not built)

Threading gets a marked door, not ambient parallelism. A `Pm` is a
thread; its own pools stay `Rc<RefCell>`. A **Store** is the explicit
shared thing: a frozen registry of `Arc<Mutex<Pool<T>>>` entries created
before threads spawn — the lock lives on each pool, so the type names
the cost at the call site. Loops keep their absolute-deadline phase and
nudge it on lock contention, annealing toward a non-interfering schedule
with no coordinator; the mutex stays the correctness backstop.

## Roadmap

1. **Typed event queues** — sugar over the event singles
2. **Threaded stores** — the sketch above
3. **Store mods (Tier 1)** — a mod as its own `Pm` + thread, handed only
   `Arc<Store>`: crash isolation, safe unload (today's injected mods
   stay as the sharp-knife tier; wasm is a maybe-later third tier)
4. **Benchmarks** — threshold-based regression gates
5. **Parallel iteration** — rayon over dense slices, explicit opt-in
