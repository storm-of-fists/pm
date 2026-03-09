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

## Conventions

- **`noun_verb` naming:** all public API methods follow `noun_verb` order — `id_add()`, `pool_get()`, `task_stop()`, `loop_run()`
- **`_prefix` for private members:** private/internal fields use a leading underscore (`_next_seq`, `_alive`), not `m_` prefix
- **All game logic runs inside tasks:** never put simulation code after `loop_run()` or outside a task lambda — tasks are the only execution unit

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

Fetch pools and states **during init** and capture them in the lambda closure — don't call
`pool_get<T>()` or `state_get<T>()` inside a task.

Quick reference:

- `pm.state_get<T>("name")` — singleton state, returns same pointer on re-fetch
- `pm.pool_get<T>("name")` — sparse-set component pool
- `pm.task_add("name", priority, lambda)` — register a task (runs every tick)
- `pm.task_add("name", priority, interval, lambda)` — register a periodic task (runs once per `interval` seconds)
- `pm.id_add(peer)` — monotonic entity creation (returns 32-bit Id, `[8-bit peer | 24-bit seq]`).
  Default peer 0. NOT thread-safe in parallel `each`/`each_mut`.
- `pm.id_remove(id)` — deferred removal (flushed at end of `loop_once()`)
- `pm.id_alive(id)` — check if an Id is currently alive
- `pm.id_sync(id)` — accept a remote Id (networking), advances peer sequence
- `pm.id_set_next_sequence(peer, seq)` — forward-only sequence set (server handshake)
- `pool->values()` — `std::span<const T>` for range-for without IDs
- `pool->values_mut()` — `std::span<T>` for mutable range-for (no change hooks)
- `pool->ids()` — `std::span<const Id>` of dense Id array
- `pool->size()`, `pool->empty()` — element count / emptiness check
- `pool->remove_all()` — deferred removal of all entities in the pool
- `pool->each([](const T&) { ... })` — read-only iteration, no change hooks
- `pool->each_mut([](T&) { ... })` — mutable iteration, fires change hooks
- Tasks receive `Pm& pm` with `pm.loop_dt()`, `pm.loop_quit()`, `pm.id_add()`, etc.
- Networking: `net->on_recv(type, handler)`, `net->send_to(peer, data, size)`
- Net diagnostics: `net->peer_rtt(p)`, `net->peer_reliable_pending(p)`, `net->is_open()`, `net->bytes_sent()`
- Net lifecycle: `net->close()` — close socket
- All time is `float` seconds (matches `pm.loop_dt()`)
- Mods: `.so` files exporting `extern "C" pm_mod_load(Pm&)` / `pm_mod_unload(Pm&)`
- `pm.task_stop("name")` — stop a task (clears fn + deactivates, safe for dlclose)

## Example game: Hellfire

A networked multiplayer top-down shooter. See
[src/examples/hellfire/README.md](src/examples/hellfire/README.md) for game docs, architecture,
and game-specific roadmap.

## Tests & Benchmarks

98 tests + 24 benchmark cases in a single binary (`pm_tests`), powered by doctest.
Benchmarks have threshold-based pass/fail (~2x measured max) to catch regressions.

```bash
ctest --test-dir build                  # run all (tests + benchmarks)
./build/pm_tests -ts=bench             # benchmarks only
./build/pm_tests -tse=bench            # tests only, skip benchmarks
./build/pm_tests -tc="pool add"        # single test/benchmark
./build/pm_tests -ltc                  # list all test cases
```

### Key numbers (20-core WSL, -O2)

| Operation | ns/op |
|-----------|------:|
| `pool->get(id)` | ~0.7 |
| `pm.id_add()` | ~1.2 |
| `pool->remove(id)` | ~3 |
| `each()` trivial (100k, seq) | ~0.6 |
| `each()` trig (100k, parallel) | ~3.6 |
| `id_process_removes` (10k, 8 pools) | ~62 |
| Monster AI (400, seq) | ~8 |
| Collision frame (400m + 600b) | ~31 |
| Server tick level 5 | 0.06ms total |
| Cooldown::ready | ~0.7 |

## v3 Roadmap

### Phase 1 — Kernel cleanup (DONE)

Deferred removes, lambda `each()`/`each_mut()` with auto-parallel,
permanent pools/states, no entity names, TaskFault, no Hz sub-stepping.

### Phase 2 — Build system & Architecture

**Done:**
- Compiler warnings: `-Wall -Wextra -Wpedantic -Werror`
- Single build config: `RelWithDebInfo` (`-O2 -g`), sanitizer presets (`asan`, `tsan`)
- doctest: test names, actual vs expected on failure, CLI filtering
- Benchmark suite: median-of-5, threshold-based pass/fail via doctest
- Monotonic peer-owned Ids: 32-bit `[8-bit peer | 24-bit sequence]`, never recycled, no
  generations. Paged sparse array for Pool ID-to-index lookup (two-level page table, pages
  allocated on demand). `id_add(peer)`, `id_alive(id)`, `id_sync(id)`,
  `id_set_next_sequence(peer, seq)`. Peer 0 = server/single-player.

**Next up:**
- **C++20 adoption:**
  - Concept constraints on `each()`/`each_mut()` — `requires` clauses for clean error messages
  - Concept constraints on init functions — `std::invocable` on `task_add()`, `sdl_init`, etc.
  - `std::span` in networking API — replace `void* data, len` pairs in `pm_udp.hpp`
- **CI (4 jobs):** ASan, UBSan, TSan, clean release
- **Test splitting:** `test_pool.cpp`, `test_kernel.cpp`, `test_net.cpp`, etc.
- **`Checked<T>`:** `pool->get(id)` with null check + `TaskFault`
- **Join iterator:** `pool->each_with(other_pool, fn)` — iterate smaller, lookup larger
- **`pm.clear_world()`:** loop pools, clear_all, reset kernel state
- **Multi-hook support for Pool:** multiple observers per pool (currently limited to one)
- **Typed event queues:** `push<T>()` / `drain<T>()` for inter-task data flow
- **Module system:** named ownership groups, `unload_module()` tears down everything tagged

### Ideas to evaluate
- **Entity pooling:** mark entities inactive instead of removing — reuse on spawn, skip in
  simulation/rendering, sync `active` flag over the network. Eliminates `id_add`/`id_remove`
  churn and reliable removal packets. Options: grow-on-demand with pool scan for inactive
  slots, or pre-allocate all entities upfront (fixed pool, hard cap). Needs a clean mutation
  tracking story first (see `PoolRef<T>` below).
- **`PoolRef<T>` (mutation handle):** `pool->get_mut(id)` returns an RAII handle that fires
  `notify_change(id)` on destruction. Eliminates manual `notify_change` calls and the
  `each_mut` problem (marks ALL entities dirty every frame via change hook). Direct iteration
  with `get_mut` would only dirty entities actually touched. Prerequisite for clean entity
  pooling.
- **Spatial hash maps (`pm_spatial_grid.hpp`):** generalize the current hellfire-specific
  `SpatialGrid` into a reusable framework primitive. Configurable cell size, typed queries,
  integrate with `each()`/interest management.
- **SDL_GPU + Slang:** replace SDL3 renderer with SDL_GPU for compute shaders, custom
  pipelines, and Slang for cross-platform shader authoring. Enables GPU-driven particle
  systems, instanced rendering, and post-processing.
- **Multi-runner model:** named threads with own tick loops, tasks assigned to runners
- **Pool snapshots:** double-buffered dense arrays for lock-free cross-runner reads
- **Queue system (cross-runner):** lock-free MPSC ring buffer for inter-runner communication
- **Chunk allocation:** 16KB pages, cache-line aligned, stable pointers across growth
- **Filtered views:** `pool->where(fn)` — bitset per pool, composable with joins and parallel
- **Query caching:** generation counter invalidation for joins
- **`pool->sort_by(fn)`:** reorder dense arrays, update sparse back-pointers
- **Variadic emplace:** `pool->emplace(id, args...)`
- **`-Wconversion`:** noisy in game code, needs dedicated cleanup pass
- **Fuzz testing:** network recv path + mod loading via libFuzzer/AFL
- **Deterministic replay:** record inputs (packets, player inputs, RNG seeds)
- **Relationship system:** entity-to-entity (`parent_of`, `held_by`, `targets`), cascade removal

See [src/examples/hellfire/README.md](src/examples/hellfire/README.md) for game-specific
roadmap (SDL3_ttf, monster AI, spatial quad-tree, mod enhancements).

## License

TBD
