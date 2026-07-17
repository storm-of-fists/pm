# Engine roadmap — queued designs and known cliffs

*Started 2026-07-17 from the engine-state review (the 200-hog starvation
session). Each entry says why it's queued, what triggers it, and rough
size. When something lands, its durable parts move onto the types and
the entry here shrinks to a line in the rationale doc (README doctrine).
This file is the index; deeper design lives next to each area.*

## Netcode

**Multi-datagram snapshot flights — LANDED 2026-07-17** (per-send seq
in the header, acks echo `(tick, seq)`, ALPN bumped to `pm/2`; send
loop in `netmod::serve` extends the flight until the backlog drains,
the `net.sendtune` kbps budget / 8-datagram rail is spent, or
`dgram_space` says BBR isn't draining; hogs' `net_kbps` param drives
it live). Durable contracts live on the types (`net.rs` module docs,
`Snapshot::more`, `PmServer::send_tune`). Fairness stays load-bearing
under congestion exactly as designed.

**Pack-scan cost — parked until ~10k entities.** The per-peer pack is
O(entities × pools) per DATAGRAM — a flight multiplies it, and the
2026-07-17 fairness fix added a counting scan (`dirty_bytes`) per send
on top. Fine at horde scale (measured ~µs); fix when it shows up in
profiles: a per-tick dirty journal per pool, or fold count+pack into
one pass. The canonical note is the TODO block atop `net.rs` (with the
removal-log ack-OR-timer release and reconnect, also parked).

**Params stage 2 — LANDED 2026-07-17** (36 params: the server-read
tuning set + shared-step constants read through the client replica;
correction-blip soak passed — see docs/params.md). Stage 3 stays
queued there: startup echo of non-default params, range-corner
invariant soaks, host-only param gate the day public servers exist.

## Rendering

**Horde instancing — LANDED 2026-07-17** (`Renderer3d::instances` +
`Frame3::draw_instanced`, per-instance model/tint at instance step
rate; hogs+corpses+tracers = 4 draw calls). Measured on WSLg: 31 →
17.5 ms at 200 hogs; a 1000-hog wave renders at ~21 ms — the cliff is
gone, the sim and the wire are the horde ceilings now. The segmented-
parts gait plan (mesh/animation doc) slots straight into the same
batches (more meshes, same four calls per part kind).

## Collisions (all example-land — the engine has no collision module)

**Part behavior as data — small, worth doing.** Damage multipliers and
the tail kick live as match arms inside `heli_hits`
(`PART_ROTOR => base * 2.0`, …). Move `dmg_mul`/`kick` onto the part's
collider entry: new vehicles get part behavior without touching
response tasks, and the numbers become params-tunable.

**`*_hits` task unification — wait for the 4th vehicle.** The three
response tasks share a skeleton, but the differences ARE the per-
vehicle feel (bite scrub vs none, tail kick). Rule of three: unify the
health-chip half only when a new vehicle proves the pattern.

**Shape vocabulary — deliberately capsule + altitude band.** Becomes
debt only when gameplay demands walkable bridges / stacking — which is
exactly the Box3D layer-3 trigger already parked in the physics plan.

**Broadphase — linear is fine to ~1–2k hogs.** Each bullet sweeps every
collider in its rewound frame (~25k capsule tests/tick at 200 hogs +
60 bullets). Past a couple thousand entities, plug in the engine's
existing `SpatialGrid`.

## Leanness: promote doctrine into types (opportunistic, cheap)

The engine's remaining complexity is contracts living in folklore, not
code. When touching these areas anyway:

- Named phase constants instead of float literals (28 bites / 31 sweep /
  32 drain / 33 wave / 95 net-send…) — the same-tick contact contract
  already has a runtime guard; the numbers deserve names.
- The "read `net.*` singles at prio > 5" rule.
- Two smoothing APIs (`pool_mirror` in demo vs `interp_pool` everywhere
  real) — fold when demo stops needing the simple teaching path.

## Watch list (no action until measured)

- History-ring memory and rewind scans past a few thousand colliders.
- Single-core sim ceiling — `hog_ai` was the biggest task; the
  2026-07-17 `ai_stride` param (think every Nth tick per hog, slot-
  staggered cohorts, move every tick) cut its decision cost to
  1/stride. Re-profile before reaching for the parked opt-in threading
  design; stride buys headroom, threading is still the eventual answer
  if hordes grow 10x.
- Interp draw-pool per-frame cost once visible-entity counts grow
  (cullable).

## Cross-session queue (owned by other docs/memories)

Interp default pick (33 vs 50) + interactive fairness pass · `addr=` /
reconnect · hog gait (mesh/animation step 1) · sound design · D3D12
warm-up native-Windows verify · recordings/saving (design sketch in
net.rs TODOs).
