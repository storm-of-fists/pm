# Game params: file-seeded, live-tunable, saved on demand

*Design, 2026-07-17. Stage 1 lands with this doc; the durable contracts
move onto the types as they land (README doctrine).*

## Problem

Game tuning lives in `pub const`s (`examples/hogs/src/common.rs`), so
every balance experiment is a recompile — Connor's ask (2026-07-17):
give everything an initial value from a file read at startup, let the
values be edited live from the pm-control side, and let the edited set
be saved back to that file. No recompiles to tune the game.

## What is a param (and what is not)

A **param** is a *server-owned tuning scalar*: a number a designer wants
to move while the game runs, whose meaning survives the change (wave
size, damage, aggro radius, cooldowns). Everything else stays a Rust
const:

- **Structural constants** — wire quantization scales, geometry tables
  (`BUILDINGS`), part/category ids, colors, `ADDR`, `FIXED_DT`. Changing
  these mid-run is meaningless or breaks contracts (a wire scale is a
  handshake fact, not a feel knob).
- **Client-cosmetic knobs** — day length, link sim. Already live-tunable
  per client via the telemetry node; they are per-client state, not
  shared truth, so they stay off the server params.
- **Creation-frozen config** — the interp delay: baked into pool
  registration at connect. A param must be *hot-readable*; interp needs
  a reconnect story first.
- **Shared-step constants** (`vmax`, grips, heli thrust…) ARE params
  since stage 2b: the client's predictor replays the same step, so the
  steps take `&Params` and each end reads its copy — the server its
  single, the client its replica (see below).

## Architecture

Doctrine unchanged: **clients send channels, the server replicates
state.**

```
hogs.params file ──(load, clamp)──> server Params single ──sync──> every client
      ^                                   ^     |
      └──(save event: server rewrites)    |     └─ reads: server tasks (stage 1),
                                          |        shared steps via replica (stage 2)
   pm-watch/pm-mon set ──> telemetry knob signals ──diff──> ParamSet event (reliable)
```

- **`Params` pod + `PARAM_SPECS` table** (`common.rs`): one f32 field
  per param, spec = `{name, default, min..max, blurb}`. Spec order IS
  field order; a `bytemuck::must_cast` to `[f32; N]` makes the count
  mismatch a compile error and a unit test pins name↔field agreement.
- **The file** (`hogs.params`, override `params=PATH`): pm-control
  save-file shape — `name=value` per line, `#` comments, unknown names
  ignored (a warning, not an error), missing names keep defaults, and
  every loaded value **clamps to its spec range** (the P1-13 lesson:
  hand-edited files never load raw). Loaded once in `main` before any
  thread spawns, so wave 1 already uses it in every mode.
- **Server truth**: `sync_single::<Params>("params")` seeded from the
  file. It replicates to every client (late joiners get it with their
  first snapshot) — server tasks read it instead of the old consts.
- **Live tuning**: the telemetry node grows one knob signal per spec
  (built *from the table* — the documented Vec-of-signals `Register`
  escape hatch — so adding a param is one spec line + one field), named
  `params.<name>`, range-clamped at the signal. A knob change is
  diffed and sent as a reliable `ParamSet { idx, value }` event; the
  server clamps again (never trust the wire) and writes the single.
- **Save**: `params.save` acts as a button (`set hogs params.save 1`,
  edge-detected). It sends `ParamSet { idx: PARAM_SAVE, .. }` — same
  channel, documented sentinel — and the **server** rewrites its file
  from the authoritative single. The file belongs to the process that
  owns the values: works unchanged for a dedicated server (it saves
  *its* file; an in-process session saves the one `main` loaded).
- **Bots** register the schema (handshake is strict equality) and never
  send.

### Why not…

- **Env vars / CLI flags** (`PM_HOGS`): no live path, no save path, and
  a second source of truth next to the file. `PM_HOGS` is retired by
  `wave_base`.
- **A TOML dep**: the codec is 20 lines and pm-control already defined
  the line shape (`SaveSet`); one format across the whole platform
  beats a second parser.
- **Client-owned file**: the wrong owner. The server is the authority;
  a remote client could neither seed wave 1 in time nor save the
  server's truth.

### Remote-client caveat (stage 1)

Knob signals seed from the *local* file; joining a **remote** server
whose params differ shows stale knob values until the first write. The
truthful display is a read of the replicated single — wire that into
the node when a remote-tuning session actually happens (the replica is
already on the client; it's a display question, not a transport one).

## The set (stages 1 + 2 landed 2026-07-17)

**`PARAM_SPECS` in `examples/hogs/src/common.rs` is the authoritative
table** — 36 params with defaults, ranges, and doc comments on the
`Params` fields (this doc stops duplicating it; the README anti-drift
rule). The shape of the set:

- **Stage 1 pilot (9)** — wave sizing, damages, hog speed, plus two
  that arrived with the day's engine work: `net_kbps` (per-peer
  snapshot-flight budget — the params task bridges it into the engine's
  `net.sendtune` single via `PmServer::send_tune`, the first param
  driving an ENGINE knob rather than a game read-site) and `ai_stride`
  (hog think cadence; 1 = the old every-tick brain, for A/B feel).
- **Stage 2a (18)** — the remaining server-read tuning: aggro/roam/
  flee, bite economics, kill/death points, knockback, the player gun
  (cd/range/bullet speed), hit pads, the gunner-hog envelope, tail
  kick, leap ceiling. Clients that display or mirror any of these (the
  cosmetic gun's cadence, the aim line's reach, the bots' lead
  arithmetic) read the REPLICA (`ClientWorld::params`) — never a const.
- **Stage 2b (9)** — the SHARED-STEP constants (`vmax`, grips, heat,
  heli lift/thrust ceiling/yaw): `truck_step`/`heli_step` now take
  `&Params`; the server passes its single, the predictors capture the
  client's `SingleRx<Params>` replica (see `client_setup`). A live
  change mispredicts only for the inputs in flight while the new value
  crosses the wire. Soak-verified at `lag=80 loss=0.03`, wave 200: a
  hard-driving bot corrects ~30/5s at baseline (that's the 3% input
  loss, pre-existing), a live `vmax` write added ~30 corrections in its
  one window (≈ one per in-flight input), and the counter fell straight
  back to baseline — a blip, never a storm.

What stays const is STRUCTURAL: geometry and hulls, wire quantization,
physics identities (G), FBW internals, category/part ids. `hogs.params`
is gitignored: it is local tuning state, like `settings.local`.

## Stage 3 (queued)

- **pm-mon**: params already appear as ordinary signals; a dedicated
  panel (ranges, dirty-vs-file markers) is nice-to-have.

## Stage 3 (ideas, unscheduled)

Autosave-with-debounce as an opt-in knob; per-map param files; a file
header with schema version if params ever need migrations.
