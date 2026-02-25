# Project: pm

A data-oriented game framework in C++17. Header-only, built around a flat task scheduler
and sparse-set ECS. Designed for performance, simplicity, and API consistency — with
extensive customizability and modding as a core goal (hot-reloading tasks, state, and
game logic at runtime).

## Philosophy
- Data-oriented: POD structs in pools, no inheritance hierarchies
- Flat scheduler: tasks are just functions with priorities, no dependency graphs
- Composable: small headers that opt-in to features (SDL, networking, debug, etc.)
- Moddable: hot-reload assets (done), hot-reload code/tasks via dlopen (done)
- PLC-inspired utilities: timers, latches, edge detectors, counters

## Architecture

### Framework headers (`src/pm/`)
| Header | Purpose |
|--------|---------|
| `pm_core.hpp` | Kernel: Pm scheduler, Pool<T>, State<T>, TaskContext, ECS, ThreadPool |
| `pm_math.hpp` | Vec2, distance, normalize, Rng (xorshift32) |
| `pm_udp.hpp` | Networked sync: NetSys, peer management, reliable messaging, pool sync |
| `pm_sdl.hpp` | SDL3 window/renderer, DrawQueue, KeyQueue, pixel font, exe_dir() |
| `pm_sprite.hpp` | PNG sprite loading, hot-reload via mtime polling, centered drawing |
| `pm_debug.hpp` | Debug overlay: FPS, task table, entity stats, faults |
| `pm_util.hpp` | PLC helpers: Hysteresis, Cooldown, DelayTimer, edges, Latch, Counter |
| `pm_mod.hpp` | Mod hot-reload: ModLoader watches .so files, dlopen/dlclose on mtime change |
| `pm_spatial_grid.hpp` | Spatial hashing for collision queries |

### Example game (`src/examples/hellfire/`)
- `hellfire_common.hpp` — shared types (Player, Monster, Bullet, packets, Phase constants)
- `hellfire_server.cpp` — authoritative server (no SDL, headless)
- `hellfire_client.cpp` — rendering client with lobby, sprites, debug overlay

### Phase constants
Phase constants are game-specific, not framework-level. Hellfire defines its own in
`hellfire_common.hpp`. Framework init functions (`sdl_init`, `net_init`, `debug_init`)
take `float` priority parameters — tasks run lowest to highest. Document conventions
per-game, don't bake them into pm_core.

### Iteration: lambda `each()` / `each_mut()`
```cpp
// Read-only (no change hooks fired):
pool->each([](const T& val) { ... });               // value only
pool->each([](Id id, const T& val) { ... });        // with entity id
pool->each(fn, Parallel::Off);                      // force sequential
pool->each(fn, Parallel::On);                       // force parallel
pool->each(fn);                                     // Auto: parallel above 1024 items

// Mutable (fires change hooks after each call):
pool->each_mut([](T& val) { ... });                 // value only
pool->each_mut([](Id id, T& val) { ... });          // with entity id
pool->each_mut(fn, Parallel::Off);                  // force sequential
pool->each_mut(fn);                                 // Auto: parallel above 1024 items
```
- Lambda is the only iteration API (range-based forms removed)
- `each()` is read-only: passes `const T&`, does NOT fire change hooks. Safe for parallel.
- `each_mut()` is mutable: passes `T&`, auto-fires change hooks after every lambda call.
  If a change hook is installed and parallel is requested, `each_mut()` falls back to
  sequential to prevent data races in the hook.
- Auto-parallel dispatches chunks across a ThreadPool (lazy-init, `hardware_concurrency()`)
- `continue` in old range-for becomes `return` in lambda
- Writes to `T&`: safe (your chunk in `each_mut`). Reads via `get()`: safe.
  `remove_entity()`: safe (deferred).
- `spawn()`/`add()` in parallel `each`/`each_mut`: NOT safe (vector reallocation race).
  Future: deferred spawn queue (like `remove_entity`) or mutex-protected spawn.

### Id flags
Id flags (bits 15..0) must be immutable for the entity's lifetime (except `is_free`
which is kernel-internal). Changing flags would require rewriting every `dense_ids`
cache entry across all pools that contain the entity.

### Deferred removes
`remove_entity(id)` queues the Id (mutex-protected, thread-safe). Entities stay alive
and iterable for the rest of the frame. All queued removes flush at the end of
`tick_once()` after all tasks complete. Double-removes are harmless (second is stale,
skipped). Spawns are immediate (append to end of dense arrays); `each()` snapshots
pool size at start so newly spawned entities are not visited.

## v3 Roadmap

### Phase 1 — Kernel cleanup (DONE)
- ~~Id slots: bitpacked `uint64_t`, `m_slots` vector~~
- ~~Deferred removes: `remove_entity()` queues, `flush_removes()` after all tasks~~
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
- **C++20 target:** concepts, `std::span`, designated initializers. Fully supported across
  GCC, Clang, MSVC.
- **Compiler warnings (always on):** `-Wall -Wextra -Wpedantic -Werror -Wconversion -Wshadow`
- **Debug builds:** `-fsanitize=address`, `-fsanitize=undefined`, `-fstack-protector-strong`
- **Release builds:** `-D_FORTIFY_SOURCE=2`, `-fsanitize=undefined -fno-sanitize-recover`
- **CI (4 jobs):** ASan build, UBSan build, TSan build (separate — can't combine with ASan),
  clean release build
- **Test splitting:** `test_pool.cpp`, `test_kernel.cpp`, `test_net.cpp`, etc. — each compiles
  and runs independently. Failing network tests don't block pool results.
- **doctest:** replace raw asserts. Test names, actual vs expected on failure, CLI filtering
  (`./test --test-case="*orphan*"`).
- **Fuzz testing:** network recv path + mod loading via libFuzzer/AFL. Finds buffer overreads,
  malformed packets, truncated crashes.
- **Benchmark suite:** `bench_spawn_10k`, `bench_remove_10k`, `bench_iterate_100k_with_join`,
  `bench_sort_by`. Google Benchmark, ns/op, CSV output, regression tracking per commit.
- **Deterministic replay:** record inputs (packets, player inputs, RNG seeds). Replay = bug
  report. RNG already seeded (`Rng{42}`), just need input stream capture.
- **Compile time tracking:** `-ftime-trace` (Clang), visualize in Chrome tracing, track in CI.
- **Coverage:** `--coverage`, lcov/gcovr. Not chasing 100% — identify untested critical paths.
- **Crash dump collection:** core dumps or Google Breakpad for minidumps. Ship release with
  `-g1` for function names in stack traces. Module tag identifies which mod crashed.

**Architecture:**
- **Multi-runner model:** named threads with own tick loops. `pm.schedule("runner", "task",
  priority, fn)`. Runners auto-vivify — first task on a name creates the thread.
- **Module system:** named ownership groups. `pm.module("name")` tags all registered tasks,
  pools, queue handlers. `pm.unload_module("name")` tears down everything tagged. No tiered
  system/mod distinction. Hot reload: dlclose old → unload_module → dlopen new → re-register.
- **Pool snapshots:** `pm.pool<T>("name", {.snapshot = true})`. Double-buffered dense arrays.
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
- **Debug entity inspector:** click entity, see all components across all pools
- **Per-pool staleness timeout:** framework-managed parallel array, reset on `add()`,
  incremented per frame, entity removed when exceeded. Configured on `bind_recv`:
  `net->bind_recv(bullets, read_bullets, {.stale_timeout = 1.5f})`

### Phase 4 — Relationship system
General-purpose entity-to-entity relationship module. Built on top of the framework —
games that don't need it pay nothing.
```cpp
struct Rel { Id from; Id to; NameId type; };  // "parent_of", "held_by", "targets"
auto* rels = pm.pool<Rel>("relationships");
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

## Next up: SDL3_ttf text rendering

**Plan file:** `.claude/plans/sparkling-imagining-cocke.md` (approved, ready to implement)

Replace pixel font (`push_str`/`tiny_font`) with SDL3_ttf. Debug overlay currently
causes ~30% CPU from 10k-15k tiny DrawRect quads per character pixel. TTF renders
each string as 1 texture instead.

Steps: Add SDL3_ttf via FetchContent (release-3.2.2) → create `pm_text.hpp`
(Font, TextQueue, push_text, measure_text, render_text_queue, text_init) → download
JetBrains Mono to `resources/` → convert ~43 push_str callsites (hellfire_client 25,
pm_debug 17, example_mod 1) → build + verify.

Key files: new `src/pm/pm_text.hpp`, modified `pm_debug.hpp` (include pm_text, add
Font* param to debug_init), `hellfire_client.cpp`, `CMakeLists.txt` (both top-level
and hellfire).

## Planned work (non-v3)
- Spatial quad-tree (pm_spatial_quad_tree.hpp)
- Monster AI refactor (idle monsters shouldn't move every frame)
- Mod enhancements: directory scanning, copy-before-load, DebugOverlay cleanup API

## Environment
This project lives inside WSL (Ubuntu-24.04). All Bash commands must be prefixed with:
```
wsl -d Ubuntu-24.04 -- bash -c "cd /home/clatham/pm && <command>"
```
Do NOT run bare `make`, `cmake`, `g++`, etc. — they won't find the right toolchain.

## Build
```bash
cmake --preset debug        # configure (once)
cmake --build build         # build all targets
cmake --build build --target hellfire_client
cmake --build build --target hellfire_server
cmake --build build --target pm_tests
cmake --build build --target example_mod  # rebuild mod .so for hot-reload
ctest --test-dir build -V   # run tests
```
**Do NOT `rm -rf build`** — SDL3 and SDL3_image are built from source via FetchContent
and take a long time to rebuild. To reconfigure, just re-run `cmake --preset debug`.

## Key patterns
- `pm.state<T>("name")` — singleton state, returns same pointer on re-fetch
- `pm.pool<T>("name")` — sparse-set component pool
- `pm.schedule("name", priority, lambda)` — register a task
- `pm.spawn()` — immediate entity creation (returns Id). NOT thread-safe in parallel
  `each`/`each_mut` (vector reallocation race). Future: deferred spawn queue or mutex.
- `pm.remove_entity(id)` — deferred removal (flushed at end of `tick_once()`)
- `pool->each([](const T&) { ... })` — read-only iteration, no change hooks
- `pool->each_mut([](T&) { ... })` — mutable iteration, fires change hooks
- Tasks receive `TaskContext& ctx` with `ctx.dt()`, `ctx.pm()`, `ctx.quit()`
- Networking: `net->on_recv(type, handler)`, `net->send_to(peer, data, size)`
- All time is `float` seconds (matches `ctx.dt()`)
- Mods: `.so` files exporting `extern "C" pm_mod_load(Pm&)` / `pm_mod_unload(Pm&)`
- `pm.unschedule("name")` — remove a task (nulls fn + deactivates, safe for dlclose)