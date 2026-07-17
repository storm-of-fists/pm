# Collisions: from hit tests to a collider pool

Status: **built through stage 3** (2026-07-17, the same day the
survey folded in): collider + contact pools carry friendly fire, hog
hits, and bites; the probe registries, `ray_hit_hog`, and the brain's
shape-by-name code are gone. §11 records the as-built deltas; stage 4
(multi-part content) stays open — data entry now, not engineering.

This documents where the hit-testing code was, where it went, and the
questions the trip had to answer. The destination was set 2026-07-16,
reviewing the friendly-fire work: detection should be *data* the
simulation iterates, not functions that know what a helicopter is —
and this is the feature where pm first needed a real notion of
**relationships** between entities.

A five-engine prior-art survey (2026-07-17, §9) held the design up
against Box2D/Box3D, Quake/Source, Unreal, Unity, and Jolt/Rapier: it
survived wholesale. The survey answered every §8 question, grew the
pods by two fields (`cat`, `y`), and is folded in below.

Paths relative to the repo root; today's code lives in
`examples/hogs/src/common.rs` (geometry) and `server.rs` (the sweep).

---

## 1. Where we were, honestly (the pre-migration picture)

Three generations of hit test coexist in hogs right now:

1. **`ray_hit_hog`** — bullets vs the horde, lag-compensated against
   the `history_pool` ring. Special-cased to hogs-as-circles; carries
   the per-shooter forgiveness pad (`Shot::pad`).
2. **`hog_bites_truck`** (and the heli-leap check in the hog brain) —
   melee, present time. Knows both shapes by name.
3. **`Hull` + `ray_hits_hull`** — friendly fire and the bots'
   hold-fire gate. The *geometry* is generalized (every vehicle
   reduces to a ground capsule + altitude band, one sampled sweep
   judges them all), but registration is still **code**: the server
   keeps a `Vec` of id→Hull probe closures, `ClientWorld::hulls()`
   mirrors it, and each new vehicle adds a line to both.

Generation 3 was the honest scope of an afternoon, and it's fine at
two vehicles. But "add a probe line per side per vehicle" is a
type-branch wearing a trenchcoat, and generations 1–2 are still openly
special-cased. A jet with a fuselage and two wing tanks, or a hog that
should take double damage on the snout, has nowhere to live.

## 2. The destination

> A helicopter registers each of its parts into a pool. The collisions
> task iterates that pool. The collision gets reported back to the
> helicopter to be handled.

That's the whole design. Spelled out in pm terms:

- **`Collider` pool (server-local, not synced).** One entry per
  *part*. The pod carries: `owner: Id` (the vehicle/hog entity),
  a part tag (`u8` — CABIN / TAIL / ROTOR / SNOUT..., meaningful only
  to the owner's own code), `cat: u8` category bits (VEHICLE / HOG /
  BUILDING... — what this entry *is*; each sweep brings its own mask
  of what it *tests*, the `MASK_SHOT` / `b2QueryFilter` pattern, §9),
  and a world-space shape — the existing `Hull` (capsule endpoints,
  radius, altitude band) is the shape vocabulary, unchanged.
  Categories are what retire the friendly-fire `net.owned()` walk:
  the bullet sweep tests `cat & VEHICLE` entries and skips the
  shooter's own vehicle by `owner`.
- **Owners keep their parts current.** Each vehicle's step/update
  task writes its parts' world-space shapes every tick (the
  heli-rotor matrix habit, applied to shapes). The collisions task
  never looks up a pose and never knows a vehicle kind — entries
  arrive pre-posed.
- **One collisions task** sweeps the moving things against the
  collider pool (bullets today; bites can join, see §5) and **applies
  nothing**. It writes facts:
- **`Contact` pool (server-local, transient).** `{ owner, part,
  x, y, z, kind, source }` — following the contact-points rule already
  settled in the sync-modifiers design: *transient facts are pool
  entries with a lifetime*, not callbacks, not events. Drained (or
  TTL'd) same-tick by whoever cares. `y` because hits are 3D now (the
  band check already computes altitude-at-hit; a rotor strike should
  not report as a ground splash). No normal, no impulse — engines that
  store those need them for their *solver*; ours has no solver in the
  loop, and knockback direction derives from the shot heading. Add
  fields when a consumer demands them, not before.
- **Response is the owner's business.** The heli task drains contacts
  addressed to its entities: cabin hit → normal damage, tail →
  yaw kick, rotor → double damage and a prayer. The truck task does
  truck things. The bullets task stops touching `Health` entirely.
  Detection and response never meet in the same function again.

What this buys over generation 3: registration becomes *data* (spawn
writes entries; nothing enumerates kinds), multi-part vehicles and
per-part damage fall out for free, and the same contact stream can
feed sfx/vfx/knockback without the sweep growing branches.

## 3. The relationships problem (read this section twice)

Parts are entities: `id_add` per part, entries in the collider pool.
That makes **parent→child the first real relationship in pm**, and it
breaks an invariant we currently lean on: `id_remove(vehicle)` cleans
every pool keyed by *that* id — but the parts have *their own* ids.
Kill the heli and its rotor collider survives as a ghost.

Options, in doctrine order (convention first, engine feature only
after friction proves it):

1. **Owner-liveness cull (convention).** The collisions task (or a
   tiny janitor task) removes any collider whose `owner` no longer
   resolves in any owning pool... which is the kind-enumeration
   problem again. Better: cull colliders whose owner id fails
   `pm.id_alive(owner)` — which **exists** (`kernel.rs`, generational
   ids `[peer|gen|index]`, O(1) occupancy + generation compare), so
   "is this id current" is answerable without knowing pools. **The
   right first move**, and the survey (§9) blesses it from both ends:
   every engine's handle is index+generation with an O(1) validity
   check (`b2Body_IsValid`, Jolt's sequence number, Unity's
   `Entity.Version`, Rapier's generational arenas), and Box2D's
   end-touch events ship with a "this shape may have been destroyed —
   validate before use" warning. Adopt that discipline as a second
   layer: response tasks `id_alive`-check the owner when draining
   contacts too, so a mid-tick death never dangles.
2. **Owners clean up after themselves (convention).** The same task
   that spawns/despawns the vehicle removes its part ids. Works, but
   every despawn path (death, disconnect, vehicle swap) must remember
   — the vehicle-swap path already burned us once on cleanup ordering.
3. **Engine support: child ids.** `id_add_child(parent)` and
   `id_remove` cascades. The real thing, and the obvious eventual
   shape *if* more parent→child relationships appear (turrets as
   entities, trailer hitches, hog litters...). Do not build this for
   one caller — that's the kernel-decomposition lesson.

Recommendation: start with 1 (+ 2 where the owner task is already
touching the ids), and let the friction list decide whether child ids
earn engine support. The survey seconds the ordering: every engine
cascades shape lifetime *inside* its physics module (`b2DestroyBody`
destroys the body's shapes; Unity bakes children into one compound;
UE welds) and **none** generalizes it into an entity-relationship
feature. The janitor cull is that same cascade, spelled as a
convention — child ids stay parked until more parent→child users
show up.

## 4. Lag compensation gets *simpler*, not harder

`history_pool` works on any pool. History the **collider pool** and
the rewind becomes uniform: the bullet sweep tests the rewound
collider frame, and hogs stop being a special case — their body is
just a collider entry like everyone else's (per-shooter forgiveness
keeps riding `Shot::pad`, applied as a grow on the swept shapes).

Two consequences to decide deliberately:

- **Teammate rewind: favor the shooter (decided 2026-07-17).**
  Vehicles' colliders enter the ring and friendly fire is judged in
  the shooter's rewound frame, exactly like hogs — one timeline for
  every shot, no per-target special case. The Source guardrails come
  along: rewind is bounded by the ring depth, restore is exact for
  free (the sweep only *reads* an old frame, nothing is mutated), and
  the teleport guard falls out of id generations — a vehicle swap is
  a fresh entity, so its old id fails `id_alive` and stale frames
  simply miss. "I was behind cover on my own screen" loses to sim
  consistency, deliberately.
- **Ring cost.** The ring stores parts × history depth. At hogs
  scale (hundreds of single-part hogs, a handful of multi-part
  vehicles) this is fine; just don't give every hog four parts
  because it's suddenly easy.

Prior art on both counts (§9): Source rewinds *all* players —
favor-the-shooter is the genre default — but with guardrails worth
copying if vehicles do enter the ring: a bounded rewind age
(`sv_maxunlag` 1 s, corrections beyond 200 ms rejected), a teleport
guard (target moved too far → don't rewind, "just leave the player in
the new spot"), and exact restore after the shot. And it rewinds
**hitboxes only** — pose data, never the physics world — which is
precisely what historying the collider pool is. Unity DOTS ships the
uniform version wholesale (`PhysicsWorldHistorySingleton`, a ring of
past collision worlds for server-side lag-compensated queries), so
this section's plan has shipped precedent on both ends. Present-time
for teammates was the defensible alternative; we chose the shooter's
frame — one timeline for every shot beats a per-target special case,
and the guardrails above cap the harm.

## 5. Bites are collisions too

`hog_bites_truck` and the heli-leap check are generation-2 code:
shape-vs-shape by name, in the hog brain. Once vehicles are collider
entries, a bite is "hog mouth shape vs collider pool" → `Contact
{ kind: BITE }` — and the truck/heli tasks already own the response.
The hog brain sheds its geometry; the bite cooldown and flee behavior
stay behavioral (they're the hog's business, not the collision's).

## 6. What the client does

Nothing new. Colliders are server gameplay state and **do not
replicate** (bytes buy nothing — clients can't judge hits anyway).
The bots' hold-fire gate keeps its cheap approximation off the draw
pools (`ClientWorld::hulls`, grown for politeness); it's a courtesy
heuristic, not a judge, and approximate is fine. If some day a client
wants exact part shapes (damage UI, X-ray highlights), that's a new
conversation about a synced subset — have it then, not now.

## 7. Staging

1. **Collider + Contact pools, vehicles only** — **landed 2026-07-17.**
   Friendly fire migrates: probes delete, the `net.owned()` walk
   deletes (category bits carry it, §2), the sweep iterates the pool,
   damage moves out of the bullets task into per-vehicle response
   tasks. Lifecycle by owner-cull convention (§3). Single-part hulls
   at first — the pod design must allow multi-part, the content can
   wait.
2. **Hogs join** — **landed 2026-07-17.** `ray_hit_hog` retires;
   `history_pool` moves to the collider pool; vehicles enter the ring
   under the §4 decision (favor the shooter).
3. **Bites join** (§5) — **landed 2026-07-17.** The last
   shape-by-name code leaves the brain.
4. **Multi-part content** — tail/rotor hits, snout armor — only once
   1–3 are boring, because by then it's data entry, not engineering.

Each stage left the game playable and the tests green (soaked under
`lag=80 loss=0.03` per stage); none of them touched the wire.

## 8. Open questions — answered (2026-07-17, via §9)

- **Id-liveness: already have it.** `Pm::id_alive` (kernel.rs,
  generational, O(1)) — the §3 cull needs zero engine work. Same
  structure as `b2Body_IsValid` / Jolt's sequence number / Unity's
  `Entity.Version`; validate-at-use is the industry's answer to
  ghosts, so response tasks check the owner at drain too.
- **Contact lifetime: drained-same-tick.** Task priorities already
  guarantee ordering (sweep at 31, responses at 32). This is Box2D
  v3's contract verbatim — "the event data is transient... do not
  store a reference", read the arrays right after the step — and
  Unity DOTS's (streams valid until the next step). TTL stays the
  fallback if a consumer ever runs before the sweep.
- **`Shot::pad` grows the bullet.** Expansion is query-side
  everywhere: Quake 3 expands brush planes per-trace by *that
  trace's* extents; queries carry their own filter and proxy shape
  (`b2QueryFilter`); the world never knows who's asking.
  `Hull::grow` at test time, per shot — as the bots already do.
- **Naming: `collider` / `contact`, settled.** They're the industry's
  exact words (`ColliderSet`, `b2ContactEvents`, `ContactPoint`);
  part tags per owner kind, category bits per §2.

## 9. Prior art: the engine survey (2026-07-17)

Five parallel deep-dives — Box2D v3 + Box3D, Quake/Source, Unreal,
Unity classic + DOTS, Jolt/Rapier/Godot — against primary sources
(docs, migration guides, shipped source). The design predates the
survey; nothing in it had to move. Where everyone converged:

| This design | Who else landed there |
|---|---|
| Contacts as transient data drained after the sweep | Box2D v3 event arrays; Unity `Physics.ContactEvent` + DOTS event streams; Chaos events buffered physics→game thread; Jolt/Rapier buffer-and-drain; Source queues even VPhysics damage post-sim |
| Response is the owner's code; detection applies nothing | Quake touch/mover functions; Source post-sim damage queue; Rapier's `solver_groups` (detect without resolving) |
| Part identity = small data key in the contact | Unity `ColliderKey`, Jolt `SubShapeID`, UE `FHitResult.BoneName`/`.Component`, PhysX `thisCollider` |
| Parts attach flat to one owner; hierarchy stays game-level | Unity bakes child colliders into one compound; UE welds components; Box2D/Rapier/Jolt = N shapes per body. Nobody makes parts independent sim objects |
| Generational id + validate-at-use for lifetime | `b2Body_IsValid`, Jolt sequence numbers, Unity `Entity.Version`, Rapier arenas |
| History the collider pool for uniform lag comp | Unity DOTS `PhysicsWorldHistorySingleton` (ring of past collision worlds); Source hitbox-only rewind with caps |
| Bullets as per-tick sampled sweeps | Quake projectiles (one trace per tick); `MASK_SHOT` hitscan; Box2D bullet bodies |
| Colliders never replicate | Universal — collision worlds are sim-local everywhere; only poses go on the wire |

The single strongest citation: Box2D v2 had gameplay callbacks
(`BeginContact`/`PreSolve`...) fired mid-step; **v3 deleted them** for
event arrays drained after the step. Catto's migration guide:
*"Callbacks in multithreading are problematic... chance of race
conditions in user code, user code becomes non-deterministic,
uncertain performance impact."* Unity independently retrofitted the
same shape onto PhysX (batched `ContactEvent`, then rebuilt the old
callbacks *on top of it*, ~30% faster); Jolt kept callbacks but fires
them from worker threads under a contract so restrictive that the
documented pattern is "buffer them yourself, process after update."
Detection-as-data is not a pm eccentricity; it is where everyone
ended up.

Two more findings worth keeping:

- **Gameplay collision lives outside the solver, everywhere.** Quake
  /Source movement, UE CharacterMovement, Unity CharacterController,
  Jolt `CharacterVirtual`, Godot `move_and_slide` — all swept queries
  in game code, never solver-driven; Source explicitly leaves
  VPhysics unpredicted because solver state can't replay, traces can.
  Box2D 3.1 even added a *geometric* mover (`b2World_CastMover`).
  pm's three-tier physics stance is this, and the collider pool is
  the query world — the one part of a physics engine gameplay
  actually consumes.
- **Box3D exists now.** Open-sourced 2026-06-30 (MIT, C17, alpha) as
  Catto's day job, built for a **server-authoritative open world**;
  the architecture is Box2D v3 mirrored — event arrays, generational
  ids + `IsValid`, mover queries, doubles for large worlds,
  deterministic. If joints/stacking ever get demanded (physics plan's
  layer 3), it slots behind these same pod seams and speaks this
  design's native language.

## 10. Future notes (deliberately not now)

- **Opt-in contact reporting per collider.** Box2D v3.1 turned
  contact events *off by default* for perf. If contact volume ever
  matters (hundreds of hogs biting), a report-bit on the collider is
  the lever — not a redesign.
- **Enter/exit edges.** Bullets and bites are instant facts; if a
  sustained contact ever appears (standing on a heal pad), reconstruct
  begin/end owner-side from the contact stream — `pm::Births` on the
  contact pool, the same state→edge trick sfx already uses. Unity
  DOTS reconstructs stateful events in user space identically.
- **Broadphase.** Linear scan of the collider pool is correct at hogs
  scale (Quake shipped a 32-node area tree for 1996 hardware; we have
  dozens of entries). When profiling demands one: static/dynamic
  split first (Jolt's tree-per-layer, Box2D's tree-per-body-type),
  buildings and arena walls into the static side.
- **Exact casts.** The sampled sweep (step ≤ 0.8 × radius) is honest
  at current speeds; a capsule-vs-capsule cast is a drop-in behind
  the same pod if bullet speeds or hull sizes ever make sampling
  leaky.

## 11. As built (2026-07-17)

Stages 1–3 landed in three commits, tests green and the game soaked
under `lag=80 loss=0.03` at each step. Deltas from the letter of the
design, each in its spirit:

- **`source` became `source_peer: u8`** — peer attribution is all any
  response consumes (the FF log); no consumer wanted an entity id.
  `heading` joined the pod at stage 2 *with* its consumer (knockback),
  per §2's add-when-demanded rule.
- **A hog is its own part.** Single-part swarm entities key their
  collider by their OWN id (owner == entry id): death cleans the entry
  with the entity, the janitor never has work there, and the id space
  isn't doubled at horde scale. Child part ids + the `parts` link pool
  are the vehicles' shape — the degenerate case costs nothing and
  stage 4 changes nothing.
- **Ghosts eat rounds, deliberately.** A part outlives its owner by
  one tick plus one historical frame. The janitor culls the entry; a
  rewound hit on a ghost still ends the bullet — the shooter saw it
  there — but `id_alive` gates the contact write, so nothing hurts.
- **The stale-contact purge is changed-tick-based**, not
  is-empty-based: the hog brain writes bites at prio 28, before the
  sweep at 31 — same-tick facts pass, last-tick leftovers purge
  loudly (that path firing means a response task lost its owner).
- **Bites kept their behavior in the brain** (cooldown, break-off,
  leap ceiling) and moved their geometry to the target's collider
  entry (`hull_hits_circle`) and their consequences to the response
  tasks — the truck scrubs velocity, the heli just bleeds, both
  charge `BITE_COST`.
- **Nearest-along-ray fixed a latent misbehavior:** the old FF walk
  ran before the hog test, so a teammate could eat a round a hog
  between the muzzles should have caught. One frame, two masks
  (vehicle pad 0, hog pad `Shot::pad`), min by travel now.
