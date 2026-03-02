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

## Example game: Hellfire

A networked multiplayer top-down shooter. See
[src/examples/hellfire/README.md](src/examples/hellfire/README.md) for game docs, architecture,
and game-specific roadmap.

## Tests & Benchmarks

97 tests + 23 benchmark cases in a single binary (`pm_tests`), powered by doctest.
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

## v3 Roadmap

### Phase 1 — Kernel cleanup (DONE)

Bitpacked Id slots, deferred removes, lambda `each()`/`each_mut()` with auto-parallel,
single-owner generation, permanent pools/states, no entity names, TaskFault, no Hz sub-stepping.

### Phase 2 — Build system & Architecture

**Done:**
- Compiler warnings: `-Wall -Wextra -Wpedantic -Werror`
- Single build config: `RelWithDebInfo` (`-O2 -g`), sanitizer presets (`asan`, `tsan`)
- doctest: test names, actual vs expected on failure, CLI filtering
- Benchmark suite: median-of-5, threshold-based pass/fail via doctest

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

## Environment

This project lives inside WSL (Ubuntu-24.04). When running from Windows, prefix commands:
```
wsl -d Ubuntu-24.04 -- bash -c "cd /home/clatham/pm && <command>"
```

## License

TBD
