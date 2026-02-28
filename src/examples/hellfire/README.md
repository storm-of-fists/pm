# Hellfire

A networked multiplayer top-down shooter built on the pm framework.

Up to 4 players connect to an authoritative server, fight through 5 escalating levels of
monster waves, and race to 8000 points. Server is headless (no SDL); clients render with
SDL3, sprites, and a debug overlay.

## How to run

```bash
# Build
cmake --build build --target hellfire_server
cmake --build build --target hellfire_client

# Start server on port 9998, then connect clients
./build/hellfire_server 9998
./build/hellfire_client               # connects to localhost:9998
```

Mods (hot-reload via dlopen):

```bash
cmake --build build --target example_mod
# Place .so in mods/ directory — ModLoader watches for mtime changes
```

## Files

| File | Purpose |
|------|---------|
| `hellfire_common.hpp` | Shared types, Phase constants, components, wire protocol, sync helpers |
| `hellfire_server.cpp` | Authoritative server (headless, 60 Hz tick) |
| `hellfire_client.cpp` | Rendering client with lobby, sprites, camera, debug overlay |
| `mods/example_mod.cpp` | Example hot-reload mod (.so) |
| `resources/` | PNG sprites (player front/back) |

## Architecture

### Phase constants

Tasks run lowest priority first. Hellfire defines 9 phases:

| Phase | Priority | Purpose |
|-------|----------|---------|
| INPUT | 10 | Read player input |
| NET_RECV | 15 | Receive network packets |
| SIMULATE | 30 | Physics, AI, spawning |
| COLLIDE | 50 | Collision detection |
| CLEANUP | 55 | Remove dead entities |
| DRAW | 70 | Render world (sprites, bullets, monsters) |
| HUD | 80 | Draw HUD, scores, debug text |
| RENDER | 90 | Present frame |
| NET_SEND | 95 | Send state to peers |

### Components

All POD structs stored in sparse-set pools:

- `Monster` — pos, vel, shoot_timer, size, RGB color
- `Bullet` — pos, vel, lifetime, size, player_owned flag
- `Player` — pos, hp, cooldown, invuln, alive, RGB color
- `Input` — dx, dy, ax, ay, shooting
- `PlayerInfo` — name, peer_id, connected

### Level system

5 levels with progressive difficulty. Score thresholds: 0, 500, 1500, 3000, 5500.

| Level | Speed | Spawn rate | Monster cap | Size |
|-------|-------|------------|-------------|------|
| 1 | 0.6x | 0.4x | 60 | 0.8x |
| 2 | 0.8x | 0.7x | 120 | 0.9x |
| 3 | 1.0x | 1.2x | 200 | 1.0x |
| 4 | 1.3x | 2.0x | 300 | 1.1x |
| 5 | 1.6x | 3.0x | 400 | 1.2x |

### Wire protocol

9 packet types, all packed structs:

| Type | Direction | Content |
|------|-----------|---------|
| PKT_INPUT | client→server | Player input (dx, dy, aim, shooting) |
| PKT_STATE | server→client | Game state (scores, player positions, round info) |
| PKT_JOIN | client→server | Player name |
| PKT_WELCOME | server→client | Assigned peer_id, player count |
| PKT_ROSTER | server→client | Connected player list |
| PKT_START / PKT_PAUSE / PKT_RESTART | client→server | Game flow control |
| PKT_DBG | server→client | Debug info (entity counts, ms/tick) |

Entity sync uses compressed structs (`MonSync`, `BulSync`) with int16 positions and packed
fields. Server writes entities into a flat buffer; client reads and reconciles via
`sync_id()`.

### EventBuf pattern

Simple push/clear container for inter-task event channels:

```cpp
auto* events = pm.state<EventBuf<HitEvent>>("hits");
// Producer: events->push({...});
// Consumer: for (auto& e : *events) { ... }
// Clear at start of producer or end of consumer each frame
```

## Roadmap

### SDL3_ttf text rendering (after pm_core Phase 3)

Replace the pixel font (`push_str`/`tiny_font`) with SDL3_ttf. The debug overlay currently
causes ~30% CPU from 10k-15k tiny DrawRect quads per character pixel. TTF renders each
string as a single texture instead.

- Font: JetBrains Mono (download to resources/)
- New framework header: `src/pm/pm_text.hpp`
- ~43 `push_str` callsites to convert across pm_debug.hpp, hellfire_client.cpp, example_mod.cpp

### Debug entity inspector

Click an entity in the debug overlay to see all its components across all pools. Useful for
diagnosing state bugs in multiplayer.

### Per-pool staleness timeout

Framework-managed parallel array, reset on `add()`, incremented per frame. Entity removed
when timeout exceeded. Configured on `bind_recv`:

```cpp
net->bind_recv(bullets, read_bullets, {.stale_timeout = 1.5f});
```

### Monster AI refactor

Idle monsters shouldn't move every frame. Use spatial interest radius or cooldowns to skip
monsters far from all players — reduces redundant simulation.

### Spatial quad-tree

Replace or supplement `pm_spatial_grid.hpp` with a hierarchical quad-tree for better culling
with large entity counts (400 monsters + 600 bullets).

### Mod enhancements

- Directory scanning — auto-discover mods instead of manual path
- Copy-before-load — avoid file lock conflicts during hot-reload
- DebugOverlay cleanup API — mods can register/deregister HUD elements cleanly
