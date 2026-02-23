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

### Framework headers (`src/`)
| Header | Purpose |
|--------|---------|
| `pm_core.hpp` | Kernel: Pm scheduler, Pool<T>, State<T>, TaskContext, ECS |
| `pm_math.hpp` | Vec2, distance, normalize, Rng (xorshift32) |
| `pm_udp.hpp` | Networked sync: NetSys, peer management, reliable messaging, pool sync |
| `pm_sdl.hpp` | SDL2 window/renderer, DrawQueue, KeyQueue, pixel font, exe_dir() |
| `pm_sprite.hpp` | PNG sprite loading, hot-reload via mtime polling, centered drawing |
| `pm_debug.hpp` | Debug overlay: FPS, task table, entity stats, faults |
| `pm_util.hpp` | PLC helpers: Hysteresis, Cooldown, DelayTimer, edges, Latch, Counter |
| `pm_mod.hpp` | Mod hot-reload: ModLoader watches .so files, dlopen/dlclose on mtime change |
| `pm_spatial_grid.hpp` | Spatial hashing for collision queries |

### Example game (`examples/hellfire/`)
- `hellfire_common.hpp` — shared types (Player, Monster, Bullet, packets)
- `hellfire_server.cpp` — authoritative server (no SDL, headless)
- `hellfire_client.cpp` — rendering client with lobby, sprites, debug overlay

### Phase constants (task execution order)
```
INPUT=10  NET_RECV=15  SIMULATE=30  COLLIDE=50  CLEANUP=55
DRAW=70   HUD=80       RENDER=90    NET_SEND=95
```
Use fractional priorities for ordering within a phase (e.g. `Phase::RENDER + 0.5f`).

## Planned work
- Server tick rate limiting (`pm.set_loop_rate(20)`)
- Spatial quad-tree (pm_spatial_quad_tree.hpp)
- Monster AI refactor (idle monsters shouldn't move every frame)
- Mod enhancements: directory scanning, copy-before-load, DebugOverlay cleanup API

## Environment
This project lives inside WSL (Ubuntu-24.04). All Bash commands must be prefixed with:
```
wsl -d Ubuntu-24.04 -- bash -c "cd /home/clatham/other/pm && <command>"
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

## Key patterns
- `pm.state<T>("name")` — singleton state, returns same pointer on re-fetch
- `pm.pool<T>("name")` — sparse-set component pool
- `pm.schedule("name", priority, lambda)` — register a task
- `pm.spawn()` / `pm.remove_entity(id)` — deferred entity lifecycle
- Tasks receive `TaskContext& ctx` with `ctx.dt()`, `ctx.pm()`, `ctx.quit()`
- Networking: `net->on_recv(type, handler)`, `net->send_to(peer, data, size)`
- All time is `float` seconds (matches `ctx.dt()`)
- Mods: `.so` files exporting `extern "C" pm_mod_load(Pm&)` / `pm_mod_unload(Pm&)`
- `pm.unschedule("name")` — remove a task (nulls fn + deactivates, safe for dlclose)