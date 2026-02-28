# pm

A data-oriented game framework in C++20. Header-only, built around a flat task scheduler
and sparse-set ECS. Designed for performance, simplicity, and API consistency — with
extensive customizability and modding as a core goal (hot-reloading tasks, state, and
game logic at runtime).

## Philosophy

- **Data-oriented:** POD structs in pools, no inheritance hierarchies
- **Flat scheduler:** tasks are just functions with priorities, no dependency graphs
- **Composable:** small headers that opt-in to features (SDL, networking, debug, etc.)
- **Moddable:** hot-reload assets, hot-reload code/tasks via dlopen
- **PLC-inspired utilities:** timers, latches, edge detectors, counters

## Quick start

This project builds inside WSL (Ubuntu-24.04). All commands assume you're running from
the project root.

```bash
cmake --preset dev          # configure (once), build dir: build/
cmake --build build         # build all targets
ctest --test-dir build -V   # run tests + benchmarks
```

Individual targets:

```bash
cmake --build build --target hellfire_client
cmake --build build --target hellfire_server
cmake --build build --target pm_tests
cmake --build build --target example_mod    # rebuild mod .so for hot-reload
```

Sanitizer builds (reconfigure to switch):

```bash
cmake --preset asan         # ASan + UBSan
cmake --preset tsan         # thread sanitizer (can't combine with ASan)
cmake --build build
```

SDL3 and SDL3_image are built from source via FetchContent — first build takes a while.
**Do NOT `rm -rf build`** — rebuilding SDL3 from source is slow. To reconfigure, just
re-run `cmake --preset dev`.

## Framework headers (`src/pm/`)

| Header | Purpose |
|--------|---------|
| `pm_core.hpp` | Kernel: Pm scheduler, Pool\<T\>, State\<T\>, ECS, ThreadPool |
| `pm_math.hpp` | Vec2, distance, normalize, Rng (xorshift32) |
| `pm_udp.hpp` | Networked sync: NetSys, peer management, reliable messaging, pool sync |
| `pm_sdl.hpp` | SDL3 window/renderer, DrawQueue, KeyQueue, pixel font, exe_dir() |
| `pm_sprite.hpp` | PNG sprite loading, hot-reload via mtime polling, centered drawing |
| `pm_debug.hpp` | Debug overlay: FPS, task table, entity stats, faults |
| `pm_util.hpp` | PLC helpers: Hysteresis, Cooldown, DelayTimer, edges, Latch, Counter |
| `pm_mod.hpp` | Mod hot-reload: ModLoader watches .so files, dlopen/dlclose on mtime change |
| `pm_spatial_grid.hpp` | Spatial hashing for collision queries |

## Key patterns

```cpp
Pm pm;

// Singleton state
auto* cfg = pm.state_get<Config>("config");

// Sparse-set component pool
auto* pos = pm.pool_get<Pos>("pos");

// Task scheduling (runs lowest priority first)
pm.task_add("physics", 30.f, [pos](Pm& pm) {
    float dt = pm.loop_dt();
    pos->each_mut([dt](Pos& p) {
        p.x += p.vx * dt;
        p.y += p.vy * dt;
    });
});

// Entity lifecycle
Id id = pm.id_add();
pos->add(id, {10.f, 20.f});
pm.id_remove(id);  // deferred, flushed at end of tick

// Run game loop
pm.loop_rate = 60;
pm.loop_run();
```

### Init-time capture

Fetch pools, states, and other pointers **during init** and capture them in the lambda
closure. Don't call `pool_get<T>()` or `state_get<T>()` inside a task — those are init-time
operations. Tasks receive `Pm&` directly, giving access to per-frame info (`loop_dt()`, `id_add()`,
`id_remove()`) and everything else.

```cpp
void physics_init(Pm& pm) {
    auto* pos = pm.pool_get<Pos>("pos");   // capture at init
    auto* vel = pm.pool_get<Vel>("vel");

    pm.task_add("physics", 30.f, [pos, vel](Pm& pm) {
        // Good: pos/vel already captured, no lookup per frame
        float dt = pm.loop_dt();
        pos->each_mut([dt](Pos& p) { p.x += p.vx * dt; });
    });
}
```

Quick reference:

- `pm.state_get<T>("name")` — singleton state, returns same pointer on re-fetch
- `pm.pool_get<T>("name")` — sparse-set component pool
- `pm.task_add("name", priority, lambda)` — register a task
- `pm.id_add()` — immediate entity creation (returns Id). NOT thread-safe in parallel
  `each`/`each_mut` (vector reallocation race). Future: deferred spawn queue or mutex.
- `pm.id_remove(id)` — deferred removal (flushed at end of `loop_once()`)
- `pool->each([](const T&) { ... })` — read-only iteration, no change hooks
- `pool->each_mut([](T&) { ... })` — mutable iteration, fires change hooks
- Tasks receive `Pm& pm` with `pm.loop_dt()`, `pm.loop_quit()`, `pm.id_add()`, etc.
- Networking: `net->on_recv(type, handler)`, `net->send_to(peer, data, size)`
- All time is `float` seconds (matches `pm.loop_dt()`)
- Mods: `.so` files exporting `extern "C" pm_mod_load(Pm&)` / `pm_mod_unload(Pm&)`
- `pm.task_stop("name")` — stop a task (clears fn + deactivates, safe for dlclose)

### Iteration: `each()` / `each_mut()`

```cpp
// Read-only (no change hooks fired):
pool->each([](const T& val) { ... });               // value only
pool->each([](Id id, const T& val) { ... });        // with entity id
pool->each(fn, Parallel::Off);                      // force sequential
pool->each(fn, Parallel::On);                       // force parallel
pool->each(fn);                                     // auto: parallel above 1024 items
pool->each(fn, Parallel::On, 4);                    // force parallel, limit to 4 threads

// Mutable (fires change hooks after each call):
pool->each_mut([](T& val) { ... });                 // value only
pool->each_mut([](Id id, T& val) { ... });          // with entity id
pool->each_mut(fn, Parallel::Off);                  // force sequential
pool->each_mut(fn);                                 // auto: parallel above 1024 items
pool->each_mut(fn, Parallel::On, 8);                // force parallel, limit to 8 threads
```

- Lambda is the only iteration API (range-based forms removed)
- `each()` is read-only: passes `const T&`, does NOT fire change hooks. Safe for parallel.
- `each_mut()` is mutable: passes `T&`, auto-fires change hooks after every lambda call.
  If a change hook is installed and parallel is requested, `each_mut()` falls back to
  sequential to prevent data races in the hook.
- Auto-parallel dispatches chunks across a ThreadPool (lazy-init, `hardware_concurrency()`)
- Third parameter `threads` (default 0 = all workers) limits how many threads are active
  for that specific call. Workers beyond the limit wake but skip work. Useful for tuning
  per-pool: heavy compute benefits from all cores, light work is better with fewer.
- `pm.thread_count = n` controls how many worker threads are created (clamped to
  `hardware_concurrency()`). 0 = auto. Must be set before first parallel each.
- `continue` in old range-for becomes `return` in lambda
- Writes to `T&`: safe (your chunk in `each_mut`). Reads via `get()`: safe.
  `id_remove()`: safe (deferred).
- `id_add()`/`add()` in parallel `each`/`each_mut`: NOT safe (vector reallocation race).
  Future: deferred spawn queue (like `id_remove`) or mutex-protected spawn.

### Networking

```cpp
auto* net = pm.state_get<NetSys>("net");
net->port = 9998;
net->start();
net_init(pm, net, Phase::NET_RECV, Phase::NET_SEND);

net->on_recv(PKT_INPUT, [](Pm&, const uint8_t* buf, int n, sockaddr_in&) {
    // handle packet
});

net->bind_send(pm, pool, "sync_name", Phase::NET_SEND, write_fn);
```

### Modding

```cpp
// In your .so mod:
extern "C" void pm_mod_load(Pm& pm) {
    pm.task_add("mod_task", 50.f, [](Pm& pm) { /* ... */ });
}
extern "C" void pm_mod_unload(Pm& pm) {
    pm.task_stop("mod_task");
}
```

ModLoader watches `.so` files for mtime changes and hot-reloads via dlopen/dlclose.

## Architecture

### Phase constants

Phase constants are game-specific, not framework-level. Hellfire defines its own in
`hellfire_common.hpp`. Framework init functions (`sdl_init`, `net_init`, `debug_init`)
take `float` priority parameters — tasks run lowest to highest. Document conventions
per-game, don't bake them into pm_core.

### Id flags

Id flags (bits 15..0) must be immutable for the entity's lifetime (except `is_free`
which is kernel-internal). Changing flags would require rewriting every `dense_ids`
cache entry across all pools that contain the entity.

### Deferred removes

`id_remove(id)` queues the Id (mutex-protected, thread-safe). Entities stay alive
and iterable for the rest of the frame. All queued removes flush at the end of
`loop_once()` after all tasks complete. Double-removes are harmless (second is stale,
skipped). Spawns are immediate (append to end of dense arrays); `each()` snapshots
pool size at start so newly spawned entities are not visited.

## Example game: Hellfire

A networked multiplayer top-down shooter. See
[src/examples/hellfire/README.md](src/examples/hellfire/README.md) for game docs, architecture,
and game-specific roadmap.

## Benchmarks

~90 benchmarks covering kernel operations and real game workloads. Built with `-O2`.
Benchmarks run automatically after tests pass via `ctest --test-dir build -V`.

Results are written to `benchmarks/latest.csv` (git-tracked) for regression comparison.

### Benchmark groups

| Group | Benchmarks | What it covers |
|-------|-----------|----------------|
| Pool ops | 26 | add, get, has, remove, each, each_mut, clear, mixed |
| State ops | 2 | fetch, create |
| Entity/kernel | 7 | id_add, id_process_removes, entity churn |
| Integrated workloads | 6 | game tick, multi-archetype, join patterns, sustained churn |
| Thread scaling | 3 workloads x N threads | scaling behavior across core counts |
| Spatial grid | 6 | insert, query (small/large radius), full frame rebuild+query |
| Bullet churn | 3 | sustained spawn/expire, bullet physics |
| Monster AI | 3 | steering + shooting (seq vs parallel, scaling) |
| Collision frame | 3 | full collision pass, grid vs brute force comparison |
| Server tick | 3 | full hellfire frame simulation (level 1, level 5, sustained) |
| PLC utilities | 5 | Cooldown, Hysteresis, RisingEdge, DelayTimer, Counter |
| Multi-pool tick | 2 | hellfire pool structure with cross-pool iteration |

### Key numbers (20-core WSL, -O2)

| Operation | ns/op |
|-----------|------:|
| `pool->get(id)` | ~2 |
| `pm.id_add()` | 1.5 |
| `pool->remove(id)` | ~9 |
| `each()` trivial (100k, seq) | ~0.5 |
| `each()` trig (100k, parallel) | ~6 |
| `id_process_removes` (10k, 8 pools) | ~38 |
| Monster AI (400, seq) | ~7 |
| Collision frame (400m + 600b) | ~28 |
| Server tick level 5 | 0.06ms total |
| Cooldown::ready | ~0.5 |

## Tests

94 tests + ~90 benchmarks in a single binary. Tests run first; benchmarks run after all tests pass.

```bash
cmake --build build --target pm_tests
ctest --test-dir build -V
```

## v3 Roadmap

### Phase 1 — Kernel cleanup (DONE)
- ~~Id slots: bitpacked `uint64_t`, `m_slots` vector~~
- ~~Deferred removes: `id_remove()` queues, `id_process_removes()` after all tasks~~
- ~~Lambda `each()` with auto-parallel, ThreadPool~~
- ~~Single-owner generation: Pool stores indices only~~
- ~~Permanent pools/states via `unique_ptr`~~
- ~~Kill pool/state graveyard vectors~~
- ~~Remove entity string names~~
- ~~Replace `Result` with `TaskFault`~~
- ~~Remove `Hz` sub-stepping~~
- ~~Phase constants moved to game code~~

### Phase 2 — Build system & Architecture

**Build system:**
- **C++20 (active):** `CMAKE_CXX_STANDARD 20` set. Incremental adoption of C++20 features:
  - **Concept constraints on Pool iteration:** add `requires` clauses to `each()`/`each_mut()`
    so passing a bad lambda produces a clean error instead of template noise. Internal dispatch
    stays `if constexpr` (handles two valid signatures per method). ~4 lines.
  - **Concept constraints on init functions:** `sdl_init`, `net_init`, `debug_init` and
    `pm.task_add()` take callable parameters — constrain with `std::invocable`.
  - **`std::span` in networking API:** replace `const void* data, int/uint16_t len` pairs in
    `pm_udp.hpp` (~10 functions: `send_to`, `broadcast`, `push`, `send_reliable`, etc.) with
    `std::span<const uint8_t>`. Touches all callsites in hellfire server/client — standalone
    refactor task.
  - **`std::span` for Pool snapshots:** reader runners get `std::span<const T>` over frozen
    copy (planned in pool snapshot feature below).
  - **Devirtualize PoolBase:** replace virtual `remove`/`clear_all`/`shrink_to_fit` with
    function pointers or type-erased callables. C++20 not strictly required but concepts
    can constrain the erased interface.
- ~~**Compiler warnings (always on):** `-Wall -Wextra -Wpedantic -Werror`~~
- ~~**Single build config:** `RelWithDebInfo` (`-O2 -g`), sanitizer presets (`asan`, `tsan`)~~
- **`-Wconversion`:** deferred — extremely noisy in game code, needs dedicated cleanup pass
- **CI (4 jobs):** ASan build, UBSan build, TSan build (separate — can't combine with ASan),
  clean release build
- **Test splitting:** `test_pool.cpp`, `test_kernel.cpp`, `test_net.cpp`, etc. — each compiles
  and runs independently. Failing network tests don't block pool results.
- **doctest:** replace raw asserts. Test names, actual vs expected on failure, CLI filtering
  (`./test --test-case="*orphan*"`).
- **Fuzz testing:** network recv path + mod loading via libFuzzer/AFL. Finds buffer overreads,
  malformed packets, truncated crashes.
- **Benchmark suite:** median-of-5 timing harness (no external deps), ns/op, CSV output to
  `benchmarks/latest.csv` (git-tracked) for regression tracking. ~90 benchmarks across
  kernel ops, thread scaling, and hellfire game workloads (spatial grid, collision, AI,
  bullet churn, PLC utils, full server tick simulation). Destructive benchmarks (remove,
  flush, clear) use a setup/work split pattern — setup runs un-timed before each timed run.
- **Deterministic replay:** record inputs (packets, player inputs, RNG seeds). Replay = bug
  report. RNG already seeded (`Rng{42}`), just need input stream capture.
- **Compile time tracking:** `-ftime-trace` (Clang), visualize in Chrome tracing, track in CI.
- **Coverage:** `--coverage`, lcov/gcovr. Not chasing 100% — identify untested critical paths.
- **Crash dump collection:** core dumps or Google Breakpad for minidumps. Ship release with
  `-g1` for function names in stack traces. Module tag identifies which mod crashed.

**Architecture:**
- **Multi-runner model:** named threads with own tick loops. `pm.task_add("runner", "task",
  priority, fn)`. Runners auto-vivify — first task on a name creates the thread.
- **Module system:** named ownership groups. `pm.module("name")` tags all registered tasks,
  pools, queue handlers. `pm.unload_module("name")` tears down everything tagged. No tiered
  system/mod distinction. Hot reload: dlclose old → unload_module → dlopen new → re-register.
- **Pool snapshots:** `pm.pool_get<T>("name", {.snapshot = true})`. Double-buffered dense arrays.
  Writer mutates live array normally. `swap_snapshot()` at end of frame atomically swaps.
  Reader runners get `span<const T>` over the frozen copy — no locks, one frame latency.
  2x memory cost, only enable for cross-runner pools.
- **Queue system (cross-runner):** lock-free MPSC ring buffer. Pushers do `atomic_fetch_add`
  to claim slot, write data, set ready flag. Consumer scans from read position, no atomics.
  Single-runner case (push + drain on same thread) degenerates to plain vector — no overhead.
  Auto-detect based on push/drain runner affinity.
- **Devirtualize Pool:** replace virtual `PoolBase` with function pointers or template-erased
  callables. Eliminates vtable dispatch on every remove. Linear scan of pools is fine —
  `Pool::remove` fast-fails in 2 comparisons (~1-2ns per pool).
- **Multi-hook support for Pool:** `set_change_hook()` / `set_swap_hook()` currently allow only
  one observer per pool (raw function pointer, second call silently clobbers the first). Need to
  support multiple hooks so e.g. network sync and debug/dirty-tracking can both observe the same
  pool. Options: small inline vector of hooks, or a single dispatcher that fans out.
- **Chunk allocation:** 16KB pages, cache-line aligned. Stable pointers across growth — no
  realloc stalls. Tension with `sort_by` (cross-chunk moves). Consider sort within chunks only.

### Phase 3 — API improvements & Advanced
- **`Checked<T>`** for `pool->get(id)` — null check + throw `TaskFault`, caught at task boundary
- **Join iterator:** `pool->each_with(other_pool, fn)` — iterate smaller, O(1) lookup larger
- **Filtered views:** `pool->where(fn)` — bitset per pool, skip non-matching, composable
  with `each_with` and parallel. Bitset rebuilt on structural change or explicitly.
- **Variadic emplace:** `pool->emplace(id, args...)` construct in-place
- **`pool->sort_by(fn)`:** reorder dense arrays, update sparse back-pointers
- **`pm.clear_world()`:** loop pools, clear_all, reset kernel state
- **Typed event queues:** `ctx.push<T>("name", val)` / `ctx.drain<T>("name", fn)` for
  inter-task data flow. Frame-scoped (drain clears every frame) vs persistent (consumed
  explicitly). Critical for hot reload: mods push to queues core tasks drain.
- **Query caching:** generation counter invalidation for joins — O(1) setup for stable pools

### Phase 4 — Relationship system
General-purpose entity-to-entity relationship module. Built on top of the framework —
games that don't need it pay nothing.
```cpp
struct Rel { Id from; Id to; NameId type; };  // "parent_of", "held_by", "targets"
auto* rels = pm.pool_get<Rel>("relationships");
rels->from(entity_a, "parent_of");   // forward lookup
rels->to(entity_b, "held_by");      // reverse lookup
```
- Forward/reverse hash map indexes, rebuilt on structural change (generation counter)
- Parent-child, inventory, targeting — all use cases of the same primitive
- Cascade removal: query `rels->from(id)`, destroy or orphan each target
- Transform propagation: iterate `rels->from(id, "parent_of")`, propagate position
- Network sync: send world positions, clients don't reconstruct hierarchy
- Deep hierarchies (10+ levels): consider flattening to world-space
- Dirty tracking on position change to skip clean subtrees

## Planned work

**Next up:** CI pipeline, doctest migration, test splitting, `-Wconversion` cleanup.

See [src/examples/hellfire/README.md](src/examples/hellfire/README.md) for game-specific
roadmap (SDL3_ttf, monster AI, spatial quad-tree, mod enhancements).

## Environment

This project lives inside WSL (Ubuntu-24.04). When running from Windows, prefix commands:
```
wsl -d Ubuntu-24.04 -- bash -c "cd /home/clatham/pm && <command>"
```

## License

TBD
