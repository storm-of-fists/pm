# Sync design — multiplayer as a kernel concern

Status: **v3** (2026-06-09). Decisions locked: generational ids
`[peer:8 | gen:8 | index:16]`, QUIC via quinn-proto (sans-IO, no async
runtime), bytemuck for the wire format, dead reckoning + fixed-timestep
prediction for vehicles. Foundations (tick, removal log, `Mut` guard,
`changed_since`) are implemented; the net layer is not.

Multiplayer replication is integral to pm, not a component bolted on later.
The kernel's core data structures — tick, pools, ids — are shaped so the
sync layer falls out of them, and single-player is just "zero peers" paying
one u32 store per mutation.

The model is server-authoritative snapshot-delta over unreliable datagrams
(the Quake 3 / Source / Overwatch lineage): the server continuously sends
each peer "everything that changed since the last tick you acknowledged."
Packet loss needs no special handling — unacked changes simply keep being
sent until an ack advances.

## Decision 1: tick-versioned change tracking, not counters

The kernel keeps a global `tick: u32`, incremented once per `loop_once` and
pushed into every pool. Every pool entry stores `changed_tick` — the tick it
was last mutated *or inserted* (an add is just a change). The kernel keeps a
single tick-stamped removal log: `Vec<(Id, u32)>`, appended when
`id_remove` flushes.

Why not per-entity change *counters* (the C++ approach)? A counter only
tells a consumer "changed since I last looked" if the consumer remembers the
last counter it saw — per entity, per peer: O(entities × peers) bookkeeping.
With tick stamps, a peer's entire view state is **one u32**:
`last_acked_tick`. Replication is a range query:

- state to send: pool entries with `changed_tick > peer.last_acked_tick`
  (`pool.changed_since(t)`)
- deaths to send: removal-log entries stamped `> peer.last_acked_tick`

**Adds are changes.** `Pool::add` is an upsert, so the client applies every
received `(id, value)` identically whether it has seen the id or not,
calling `id_sync` for unknown ids. An entity added then removed before a
peer ever acked it appears only in the removal log; clients ignore removes
for unknown ids.

**The removal log is kernel-level, not per-pool**, because what replicates
is entity death, which already flows through the kernel into all pools.
Constraint accepted for simplicity: synced pools shouldn't remove components
from *living* entities. The log doubles as the id-recycling gate
(decision 3).

u32 ticks at 60 Hz last ~2.2 years of continuous uptime; wraparound is
deliberately not handled.

## Decision 2: precise mutation marking via a `Mut` guard

`get_mut`/`iter_mut` yield a guard (`Mut<'_, T>`) that derefs like `&mut T`
but stamps `changed_tick` only on mutable deref. Reading through it is
free. This avoids false-positive diffs (wasted bandwidth) from tasks that
iterate mutably but write conditionally, and it restores exactly the C++
`PoolEntry::get()`/`get_mut()` semantics. Bevy converged on the same design
for its change detection.

Mechanically: the kernel pushes the current tick into each pool at the start
of `loop_once` (via the erased-pool trait), and the guard holds
`&mut changed_tick` alongside `&mut T`. Known ergonomic edge:
`let p = &mut *guard` stamps immediately, defeating laziness — acceptable,
documented.

## Decision 3: generational, peer-owned, recycled ids

32-bit ids: `[peer:8 | gen:8 | index:16]`.

- **Peer-owned:** peer 0 (server) owns all replicated entities; clients
  spawn local-only entities (predicted cosmetics, effects) under their own
  peer with no possibility of collision.
- **Generational:** indices are recycled through a FIFO free list;
  each reuse bumps the slot's 8-bit generation. A stale handle held by game
  code fails the generation check on `id_alive`/pool lookups (the dense
  array stores full ids, so a lookup costs one extra compare). 256 reuses
  of one slot could alias a very stale handle; the FIFO free list maximizes
  reuse distance, making this a non-issue at game timescales.
- **Bounded memory:** sparse pages and generation tables are keyed by
  `peer|index` (24 bits) and plateau at the high-water mark of *concurrent*
  entities — this is entity pooling at the storage level. 65k concurrent
  entities per peer; the bit split is a pair of constants if a game needs a
  different budget. (Escape hatch if ever needed: u64 ids locally with
  `peer|index` on the wire — generations are a local liveness concept.)

**The recycle-after-ack rule** makes reuse safe on the wire: a freed index
returns to the free list only when its removal-log entry is pruned, i.e.
after **every connected peer has acked the removal**. No client can receive
a reused index before processing the death of its previous occupant. With
zero peers the log prunes immediately and reuse is instant.

**All replicated entities are server-spawned.** Clients request (via input
or a reliable event); the server spawns; the entity arrives at clients
through the normal delta stream — the upsert *is* the spawn response.
`id_sync` marks a remote id alive locally and records its generation.

## Decision 4: snapshot-delta over QUIC (quinn-proto, sans-IO)

Transport is QUIC via **quinn-proto** — the runtime-free state-machine core
of quinn. No tokio, no net thread: a high-priority pm task owns a
non-blocking UDP socket, feeds received datagrams into the endpoint, drains
outgoing transmits, and services timer deadlines, all inside the tick loop.
What QUIC buys over hand-rolled UDP: the reliable channel (streams) we'd
otherwise write ourselves, TLS encryption (table stakes for shipping;
self-signed certs initially), connection lifecycle/timeouts, and a future
browser path via WebTransport.

Channel assignment:

- **Unreliable datagrams (RFC 9221):** snapshot deltas (server→client) and
  input (client→server).
- **One ordered reliable stream:** events (decision 6) and
  handshake/control.

Datagrams cap near ~1200 bytes and don't fragment, so a large delta spans
several datagrams. Acks are therefore **per-packet, not per-tick**: every
datagram is independently applicable (everything is an upsert), the server
remembers which packet last carried each entity, and an acked packet marks
its entities clean; a lost packet's entities simply remain
newer-than-acked and get resent. (The Tribes/Halo "eventual consistency per
object" model.) Per peer the server keeps: connection handle,
`last_acked_tick` per entity-batch bookkeeping, last processed input
sequence, RTT stats.

The net tick is a periodic task (20–30 Hz), independent of the 60 Hz sim.

## Decision 5: client side — prediction, dead reckoning, fixed timestep

Designed for vehicle games specifically (Rocket League is the reference
point):

- **Input is a dedicated datagram, not delta state.** Newest-wins, each
  packet redundantly carrying the last N inputs (loss tolerance for free).
  The server echoes the last input sequence it processed in every snapshot.
- **Own vehicle — prediction + reconciliation:** the client simulates its
  own vehicle immediately. It keeps a ring buffer of (input, predicted
  state) per sim tick; when authoritative state for tick T arrives, it
  compares against the stored prediction for T and, on divergence, rewinds
  and re-simulates inputs T+1..now, then smooths the visual correction.
- **Remote vehicles — dead reckoning, not buffered interpolation.**
  Snapshots carry pos + vel + orientation + angular vel; between snapshots
  the client integrates forward from the latest one; when a new snapshot
  arrives it blends the error away over ~100 ms (projective velocity
  blending) instead of snapping. Buffered interpolation adds ~100 ms of
  perceived latency — wrong for racing/dogfighting; dead reckoning costs
  none and suits high-inertia entities.
- **Fixed-timestep simulation is a hard requirement** of reconciliation:
  replaying inputs must reproduce the same states. The kernel grows a
  fixed-dt mode (accumulator: render free-runs, sim tasks step at exactly
  1/64 s or similar, render interpolates between the last two sim states).
  Same-binary f32 determinism is sufficient — client replays its *own*
  simulation; we never require cross-machine determinism.

The kernel implication of all of this is one rule: **received network state
lands in its own pools** (ordinary named pools, e.g. `"pos_net"`), and
ordinary tasks produce displayed/simulated state from it. Deltas never stomp
live simulation pools. The client net API makes this indirection the
default. Lag-compensated hit detection (server rewind) is a sync-layer
history ring buffer, added later; not a kernel concern.

## Decision 6: events are not state

State replication cannot carry one-shots — explosions, sounds, hit
confirms, projectile spawns. pm pairs the delta stream with typed events on
the reliable QUIC stream: ordered, delivered exactly once, no retransmit
machinery of our own. This dovetails with the typed-event-queues roadmap
item: a networked event is a typed event that crosses the wire. Projectiles
and particles are the canonical use — **spawn events + local simulation**,
not networked entities, which also keeps id pressure trivial.

## Decision 7: POD wire format via bytemuck

Synced pools require `T: bytemuck::Pod` (`Copy`, no padding, no pointers) —
serialization is a memcpy of dense entries; bytemuck's derive verifies
soundness at compile time. Little-endian assumed on both ends (x86/ARM).
The handshake exchanges a schema table of `(pool name, size_of::<T>, type
hash)` so version mismatches fail loudly at connect. If a struct lives in a
pool, it's flat data, and flat data is wire-ready by definition.

## Decision 8: sync is opt-in per pool

`net.pool_sync::<T>("pos")` registers a pool for replication. Unregistered
pools never touch the wire. The universal costs of this design: one u32
stamp on first mutation per entity per tick, and the kernel removal log
(empty with no peers).

## Implementation order

1. ~~**Kernel foundations**~~ — DONE: global tick, generational id
   allocator with FIFO free list, removal-log-gated recycling,
   `changed_tick` + `Mut` guard, `changed_since` query.
2. ~~**NetSys headless**~~ — DONE: `NetServer`/`NetClient` in `net.rs`.
   Peer table with ack cursors, bytemuck delta pack/apply, removal
   replication, ack-gated recycling — proven by two `Pm` instances
   converging through in-memory queues under simulated packet loss.
   Snapshots are labeled with the last *completed* tick so correctness is
   independent of net-task priority. Event framing moved to step 3: events
   ride the QUIC reliable stream, so there is nothing to prove over a
   perfect in-memory channel.
3. ~~**QUIC transport**~~ — DONE: `QuicServer`/`QuicClient` in
   `transport.rs`, quinn-proto driven synchronously (no tokio, no net
   thread). Datagrams carry snapshots + acks; one bi stream carries the
   hello (peer id + schema, mismatches rejected at connect) and typed
   events (`>= EVENT_USER_BASE`). Self-signed certs, client skips
   verification (dev posture). Proven by loopback tests over real UDP and
   `examples/demo.rs`. Known limits: snapshots must fit one datagram
   (~1200 B; oversize counted in `oversize_drops`, fragmentation later),
   peer numbers not reused per server run.
4. **Client conventions** — IN PROGRESS. Done: the input channel
   (`QuicClient::input_send` — sequenced datagrams carrying the last 8
   inputs redundantly; `QuicServer::inputs_drain` delivers in-order and
   gap-tolerant; `NetServer::input_processed` echoes the consumed sequence
   in every snapshot header, surfaced as `Applied::input_seq`). Remaining:
   prediction ring + reconciliation replay, dead-reckoning task,
   fixed-timestep sim mode — these need real latency to matter and land
   with the first non-localhost game.
5. Later: interest management (per-peer entity filters), lag-compensation
   history, bandwidth budgeting/priority.

## Deferred (known, not built)

- **Per-peer delta gather is O(entities) per peer per net tick.** Fine
  until MMO-ish scale (~50 µs per 100k-entity scan). The fix if profiles
  ever demand it: per-tick dirty lists (push index on stamp; a packet is
  the concatenation of dirty lists newer than the ack) — O(changes)
  instead of O(entities). Interest management restructures this path
  anyway.
- Generation width (8 bits) and index width (16 bits) are constants, not
  architecture; revisit per game.
- Reconnect/peer-id reassignment policy: decided with the handshake in
  step 3.
