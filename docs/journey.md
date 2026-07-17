# The pm journey — how this engine works, from first tick to photon

This is the guided tour: every layer of pm and the hogs game built on it,
in the order the machine actually does things, written to be *learned
from*, not just referenced. The crate docs (`cargo doc -p pm-world --open`)
stay the precise reference on each type; this document is the story that
connects them. Each chapter ends with **Try this** — a small experiment
that proves you understood it. Do them. Reading about netcode is like
reading about swimming.

Paths are relative to the repo root. When a chapter says "look at X",
actually open it — every claim here is checkable in a file within
arm's reach, and the code comments carry the deeper why.

---

## 1. One process, one world: `Pm`

Everything in pm lives in a `Pm` value: a **store** of typed data, a
**clock**, and a **scheduler**. There is no scene graph, no object
hierarchy, no `GameObject`. There are exactly three kinds of state:

- **Pools** (`pm.pool::<T>("name")`) — a keyed collection of plain-data
  structs ("pods"), one entry per entity that has that aspect. A truck
  is not an object; it's an entry in the `Truck` pool, an entry in the
  `Health` pool, and nothing else.
- **Singles** (`pm.single::<T>("name")`) — exactly one value: the
  scoreboard, the camera, the net status.
- **Tasks** — closures registered with a priority, run in order every
  tick. All game logic is tasks reading and writing pools.

Why this shape? Three reasons that compound:

1. **Cache honesty.** A pool is a dense array. Stepping 300 hogs touches
   300 consecutive pod entries, not 300 heap objects behind pointers.
2. **Replication falls out.** A pool of `#[pm::pod]` structs is *bytes
   with names*. "Send the world to a client" becomes "diff some arrays"
   — no serialization framework, no reflection.
3. **Determinism is auditable.** State lives in enumerable places. When
   prediction replays inputs (chapter 6), you can name everything it
   must restore.

> **Rust note — handles and `RefCell`.** `pm.pool::<T>()` returns a
> `PoolHandle<T>` you can clone into any task closure. Inside, it's
> shared ownership (`Rc`) of a `RefCell` — Rust's "check borrowing at
> runtime instead of compile time" escape hatch. `handle.get()` borrows
> read-only, `handle.get_mut()` borrows writable, and two overlapping
> `get_mut()`s panic instead of corrupting memory. That panic is the
> single-threaded cousin of a data race — pm chose runtime checks here
> because tasks are closures that all need access to the same store,
> and threading lifetimes through every closure was tried and rejected
> (the kernel-decomposition experiment; the borrow ceremony ate the
> ergonomics).

Where to look: `crates/pm/src/kernel.rs` (the loop), `pool.rs` (the
sparse set), and the crate-level doc in `crates/pm/src/lib.rs`.

**Try this:** in `examples/hogs/src/server.rs`, add a task at priority
99 that prints `hog.get().len()` once a second (`if pm.tick() % 60 == 0`).
Run `cargo run --release -p hogs`. You just wrote a system.

---

## 2. Entities are ids; components are pool entries

An entity is an `Id` — an index plus a generation counter, allocated by
`pm.id_add()` and released by `pm.id_remove(id)` (deferred to end of
tick, so removing mid-iteration is safe — the server's bullet sweep
relies on this). The generation counter is why a stale `Id` in your
hand can't accidentally address the new tenant of a recycled slot:
`get_id` checks the generation and returns `None`.

A pool is a **sparse set**: a dense array of pods plus an index that
maps `Id → slot`. Iteration is dense (fast); lookup by id is O(1);
removal swaps the last entry down (order is *not* stable — never
assume it).

One thing that will bite you if you forget it: **`id.peer()` tells you
which peer's index space the id was allocated in — recycling, not
control.** "Whose truck is this?" is answered by the replicated
ownership table (`net.owner_of(id)`), never by the id's bits. See the
comment on `owner_of` in `crates/pm/src/netmod.rs`.

**Try this:** in the hogs server's wave task, print a few spawned hog
ids with `{:?}`. Kill a wave, let the next spawn, print again — watch
slots recycle with bumped generations.

---

## 3. The clock: fixed steps, ordered tasks

`pm.loop_run()` ticks at a fixed rate (hogs: 60 Hz, `FIXED_DT`). Each
tick runs every task in **priority order** — a single flat list, no
dependency graph. The ordering is load-bearing and worth memorizing
for hogs:

```
4   input      (player client samples SDL — before net, so the
                 freshest command ships THIS tick, not next)
5   net        (pump the transport: receive state, send input)
30  drive      (server: step vehicles, spawn bullets)
31  bullets    (server: sweep flights, judge hits)
33  wave       (server: respawn the horde)
70  render     (client: draw everything)
```

The client's whole loop is paced by the display (`pm.loop_rate =
refresh`), the server's by the tick rate. Same kernel, different
tempo — the *role* decides (chapter 4).

> **Rust note — `move` closures.** Every task is a `move` closure: it
> takes ownership of the handles you clone before registering it. This
> is the pattern you'll see everywhere:
> `let hog = hog.clone(); pm.task_add("x", 50.0, 0.0, move |pm| { ... })`.
> The clone is cheap (it's an `Rc` bump), and after the `move` the task
> owns everything it touches — no lifetimes to annotate.

**Try this:** change the input task's priority from 4.0 to 6.0 (after
net) and play under `lag=80`. You just added a tick of input latency —
can you feel it? Put it back.

---

## 4. Networking, part 1: the doctrine

pm's networking is Quake-lineage server-authoritative, and one sentence
carries most of it: **clients send channels; the server replicates
pools.**

- Downstream (server → client) is **state**: synced pools and singles,
  snapshotted every tick, delta'd against what each peer has acked.
  Never events. If something transient must be seen ("a shot landed
  here"), it becomes a short-lived *fact* in a TTL'd pool (`Impact`) —
  existence means recency, clients render whatever exists, nothing
  needs cleanup messages.
- Upstream (client → server) is **intent**, on exactly two kinds of
  channel: one *continuous* input pod, sampled and sent every tick
  with redundancy (loss-tolerant — a lost input packet is covered by
  the next one carrying the last 8 frames), and *reliable events* for
  discrete must-arrive intents (`Respawn`).

Connection setup enforces a **schema handshake**: both ends register
pools/channels by name and pod size, and the handshake requires exact
equality — a version-skewed client is rejected at connect, not
corrupted mid-game.

The role split is explicit in the API: `Pm::server(addr)` gives a
`PmServer` (owns `input::<C>() → InputRx`, `sync_single`, `ttl_pool`,
`history_pool`), `Pm::client(addr, hz)` gives a `PmClient` (owns
`input() → InputTx`, `predict_pool`, `interp_pool`). Everything the
wire touches goes through these role handles; the raw transport
(`QuicClient`/`QuicServer`, `crates/pm/src/transport.rs`) is
`pub(crate)` — games *cannot* reach the socket.

Why QUIC underneath? Datagrams for state (unreliable, unordered —
newest wins), one reliable stream for the handshake and events, TLS
for free, and one port. We measured the alternative (chapter 12): raw
UDP through the same conditions performed identically. The transport
was never the bottleneck; the config was.

Where to look: `crates/pm/src/netmod.rs` — the module doc is the
doctrine written down.

---

## 5. Networking, part 2: snapshots, acks, and bytes

Every tick, the server builds each peer a snapshot: for every synced
pool, the entries that changed since that peer's last **ack**, packed
into ≤1200-byte datagrams. The client acks the tick it applied; the
server tracks unacked entries per peer per pool and *resends them* in
later snapshots until acked. This is the crucial subtlety: **pool sync
is already reliable** — not because packets can't be lost, but because
state convergence retries content until acknowledged. It's a
convergence protocol, not a change stream. (That's why there is no
"reliable pool" modifier — it would be redundant.)

Bandwidth is a budget you spend per *entry*, and `#[pm::pod]` with
`#[wire(...)]` attributes is the knife: the `Hog` pod is 20 bytes of
f32s in memory but rides the wire as 9 (positions quantized to 1/64
unit as i16, angles to 1e-4 rad, hp to u8). Registration via
`wire_pool` instead of `sync_pool` is the only difference. Look at
`Hog` and `Bullet` in `examples/hogs/src/common.rs` — the attributes
document their own ranges, and the doc comments record the arithmetic
(90 entities per datagram instead of 45).

Two deliberate asymmetries to internalize:

- Predicted pools (`Truck`, `Heli`) are **not** quantized: reconcile
  error must be able to reach zero, and a quantization step would leave
  prediction permanently correcting against rounded truth.
- The `interp=`/`lag=`/`loss=` knobs (chapter 12) are *client-side* —
  the server never simulates a bad link on itself.

**Try this:** in `common.rs`, change `Hog`'s x/z wire scale from 64 to
8 and play. That's 1/8-unit position steps — watch the horde jitter.
Quantization is a *visible* budget decision. Put it back.

---

## 6. Prediction: your vehicle answers your hands

Without prediction, pressing W does nothing for a round trip. With it,
the client applies its own input to a local copy *immediately*, and the
server's authoritative answer arrives later to check the work.

Mechanics (`crates/pm/src/predict.rs`, used via
`pm.predict_pool(...)`): the client keeps a ring of the command frames
it sent. When a snapshot arrives carrying the server's state for *your*
entity at input-sequence N, the predictor rewinds to that state and
**replays** every command after N through the step function. If the
replayed present disagrees with what it was showing, that's a
**correction** (the counter in the hogs title bar).

This only works because the step functions are *shared and pure*:
`truck_step`/`heli_step` in `examples/hogs/src/common.rs` are the same
code on both ends, `(state, command, dt) → state`, no hidden inputs.
The engine can't enforce purity, so the pods enforce a **contract**
instead, spelled out in a comment inside `truck_step` — read it now,
it's the most important invariant in the game layer:

1. Every field in a predicted pod is evolved by the step from the
   command (the exhaustive destructure at the top makes adding a field
   a compile error until you name it — and the error message sends you
   to the rule).
2. Anything the server writes *outside* the step (damage, pickups) must
   live in its own pool (`Health`), because a non-predicted field
   inside a predicted pod **freezes between corrections** — it only
   changes when a correction happens to deliver it. Read server-owned
   facts raw from their synced pool, never through `Predictor::state()`.

**Try this:** in your client's `truck_step` call path there is no way
to diverge — so make one. Multiply `accel` by 1.1 in the step *only
when `cfg!(debug_assertions)`*... then run a debug client against a
release server. Watch the corrections counter climb as the server
keeps overruling your optimistic physics. (Then delete it — and now
you know why the step must be byte-identical on both ends.)

---

## 7. Interpolation and lag compensation: everyone else

Remote entities (the horde, other players) are *not* predicted — you
don't know their inputs. Drawing them raw off snapshots looks like a
strobe under loss: entries update in bursts. `interp_pool` fixes this
by drawing them **on purpose in the past**: it buffers snapshots and
renders the state as of `now − interp delay`, interpolating between
the two snapshots that bracket that moment. The delay buys smoothness:
a lost snapshot's gap is bridged by the next one arriving before the
draw clock reaches it.

The cost is staleness, and the tuning story is this session's history:
the default was 50 ms; the `interp=` argument (`hogs interp=33`) landed
so it could be A/B'd under `lag=80 loss=0.03`, and 33 felt dramatically
better — fresher world, still smooth at 3% loss (one lost snapshot =
16.7 ms gap, covered twice over).

But drawing the past creates an aiming injustice: you shoot at where
you *see* a hog, which is where it *was*. **Lag compensation**
(`history_pool` server-side + the bullet sweep in
`examples/hogs/src/server.rs`) repays it: the server keeps a ring of
recent hog frames, and when your bullet flies, each tick of its flight
is tested against the frame your view actually showed when you pulled
the trigger — anchored to the *fire input's arrival ack*, then advanced
one frame per flight tick (steady timeline; re-reading the live ack
each tick made hits flicker with ack burstiness — the comment on `Shot`
tells that story). Damage lands on the present hog; only the *hit test*
rewinds.

Source engine players know this trade: it's "favor the shooter",
scaled to a PvE game where the hogs don't file complaints.

**Try this:** `hogs interp=200 lag=80 loss=0.03`. The world turns to
soup, but shots still land — lag comp rewinds deeper. Now `interp=8`:
fresh but strobing under loss. 33 is a *choice*, not a law.

---

## 8. The cosmetic layer: lying honestly at 0 ms

Some feedback can't wait for a round trip and doesn't need authority.
pm's pattern: a **client-local pool** — created with `pm.pool()` and
simply never synced — plus `pm::Births`, which turns "an entry appeared
in this pool since I last looked" into an iterable edge (replication
converges *state*; `Births` recovers the *event* a one-shot effect
wants).

The gun is the flagship (this was the single biggest "it feels laggy"
fix — worth ~230 ms of perceived latency at 80 ms lag):

- **Click:** the input task spawns a `Tracer` into the client-local
  `tracer.local` pool, from the *predicted* muzzle
  (`truck_muzzle`/`heli_muzzle` in `common.rs` — one definition shared
  with the server, so the cosmetic and the real bullet leave the same
  barrel). The sfx task's `Births` on that pool plays the bang the
  same frame. 0 ms.
- **~RTT later:** the authoritative `Bullet` replicates in, carrying
  `owner`. The render and sfx tasks skip bullets whose owner is you —
  your shot already happened, visually and audibly. Everyone else's
  bullets draw and bang off replication as always.
- The cosmetic tracer flies the same speed and dies on the same walls
  (`tracer_step`) but tests no hogs — hit *consequences* (damage, kill
  flash, points) remain the server's word alone, arriving when they
  arrive. Feel is predicted; truth is not.

Ragdoll corpses and muzzle-flash lights are the same idea (death edges
detected client-side, pure visuals); so is the whole sfx module —
`examples/hogs/src/sfx.rs` is a one-page masterclass in the pattern.

**Try this:** set `left: GUN_RANGE * 0.3` in the input task's tracer
spawn, play under `lag=150`, and watch your tracer die early while
distant hit flashes still bloom where the *real* bullet reached. The
seam between cosmetic and authoritative becomes visible — that seam is
the design.

---

## 9. Physics: three tiers, force-based, no engine

pm's physics stance ("no components" — you asked): physics is
**library functions, not a system**. There is no rigid-body world to
register into; there are `Body` (pos/vel/quat), `Quat`, and step
functions that games call from their own tasks. Three tiers:

1. **Predicted-kinematic** — player vehicles. Deterministic pure steps
   (chapter 6). This is where the force-based pass lives:
   - The **truck** (`truck_step`): steering turns the *chassis*;
     momentum follows through **tire grip** — world-frame velocity is
     decomposed against the new heading, engine force + rolling drag
     act along forward, and the lateral component decays at
     `TRUCK_GRIP` (8/s planted; 3.2/s while boosting = powerslide). A
     server shove (bite scrub, knockback) is just momentum the tires
     then grip out — friction, not scripting.
   - The **heli** (`heli_step`): ONE thrust vector along body-up.
     Fly-by-wire collective trims to hover at centered stick (thrust =
     g / up.y — tilt sheds vertical lift and the trim compensates, so
     hover is hands-off), the lift stick adds burn on top, gravity is
     real, and the tilt *direction* does everything else: nose-down
     vectors the same newtons forward, banking slides you into the
     turn.
2. **Server-dynamic** — NPC impulses (hog knockback, shoves): server
   writes velocities, replication carries the result, nobody predicts
   it.
3. **Client-cosmetic** — corpse tumbles, tracer flight: never on the
   wire at all. (GPU physics, if ever, lives ONLY here — the
   authoritative loop must replay deterministically on headless
   servers.)

A real constraint solver (Box3D-style) becomes tier 2's backend *if*
stacking/joints ever get demanded; `Body` is the seam it would slot
behind.

**Try this:** set `TRUCK_GRIP` to 1.5 and drive. Ice. Now 30: rails.
Grip is the entire personality of the truck, one constant.

---

## 10. Rendering: one pipeline, two walls, and light

`pm_sdl::gpu3d` is deliberately *not* an engine — one vertex format
(pos/normal/color), one standard pipeline, meshes you bake yourself
from triangles (`bake`, `box_tris`, vertex colors — no textures). But
it has opinions worth knowing:

- **Shaders are WGSL compiled by naga to SPIR-V at build time**
  (`crates/pm_sdl/build.rs`), fed to SDL_gpu. Two walls were hit and
  are now load-bearing knowledge, both documented in
  `crates/pm_sdl/src/gpu3d.rs`:
  1. SDL_gpu's Vulkan backend binds fragment samplers as COMBINED
     image-samplers, which naga's SPIR-V backend can't emit → anything
     that samples a texture (HUD text, the panini warp) is a **compute
     pass** instead.
  2. Dispatches inside one compute pass are **unsynchronized** →
     overlapping HUD quads raced their read-modify-write blends and
     flickered on D3D12 (WSLg's Vulkan happened to serialize them).
     The fix batches quads into a pass until one overlaps, then splits
     — the pass boundary is the barrier.
- **Panini projection** is the house look: render rectilinear
  oversized, warp in a compute post-pass. Wide FOV without the
  peripheral smear; verticals stay straight. `P` toggles it live.
- **Lighting** (the newest layer) is per-pixel in `basic3d.wgsl`:
  - a **sun** (direction = `frame()`'s arg; color on
    `r3d.sun_color`),
  - **hemisphere ambient** (`ambient_sky` falling on up-facing
    surfaces, `ambient_ground` bouncing onto down-facing — equal
    values reproduce the old flat ambient, which is exactly the
    default so other examples render unchanged),
  - up to **8 point lights** per frame (`r3d.point_light(pos, color,
    radius)`, cleared each frame — hogs pushes muzzle flashes first,
    then nearest headlights, because extras drop silently),
  - an **emissive flag** rides `tint.w` (`frame.draw_emissive`):
    tracers, hit flashes, and blob shadows opt out of lighting
    entirely (a shadow is a *dark* emissive — no sun can wash it out).
  - Per-pixel matters for one concrete reason: the arena ground is a
    handful of huge triangles, and a Gouraud headlight pool between
    vertices simply vanishes.
- **Time-of-day** (hogs render task) is a pure function of the tick —
  sun color, hemisphere, and horizon all derive from one angle, so it
  costs zero bytes of wire and every session opens at dawn.

**Try this:** in the hogs render task, set `DAY_SECS` to 60 and watch
a full day in a minute — dawn ember, white noon, dusk, moonlit night
with headlights carving the horde. Then find the one line that would
make kill flashes light up the hogs around them. (Run with `day=60` to preview a full cycle in a minute.) (You already have the
`flashes` Vec. It's a `point_light` push away — oh wait, it's already
there. Read that block until you could have written it.)

---

## 11. Audio: twelve voices and one hard-won lesson

`pm_sdl::audio` is fire-and-forget: 12 SDL audio streams ("voices"),
each its own logical device, SDL mixes them. `play(clip, vol, rate)`
finds an idle voice (or steals the oldest) and pushes samples — no
audio thread of ours, no ring buffer. Clips convert ONCE at load to
mono f32 @ 48 kHz (`Clip::from_wav`); the `rate` knob cheaply
de-machine-guns repeated shots (`rng.rfr(0.92, 1.12)`).

The lesson: after long sessions on WSLg, audio drifted seconds late.
The code *couldn't* queue more than one clip per voice — the backlog
was below SDL, in WSLg's PulseAudio-over-RDP bridge, whose sink buffer
creeps. `PULSE_LATENCY_MSEC=60` caps it. Native Windows (WASAPI) never
drifts. Debugging morals, both earned in this repo: *know which layer
you're standing on before you fix the wrong one*, and *WSLg is a
different platform than Windows* — it masks GPU races and invents
audio latency; test native before believing either verdict.

Still open by design: no looping voice yet (engine hum wants one), and
WAVs drop into `examples/hogs/assets/` to replace the synth
placeholders with zero code.

---

## 12. The lag lab: simulate, measure, disbelieve

You cannot feel-test netcode on localhost. The tooling:

- **`LagSocket`** (`crates/pm/src/transport.rs`): wraps the UDP socket,
  applies one-way delay + loss in both directions *inside the client
  process*. QUIC experiences it as real.
- **Args, not envs** (Windows shortcuts can't set envs):
  `hogs lag=80 loss=0.03 interp=33`. Programmatic seam:
  `PmClient::link_lag(ms, loss)`, env `PM_LAG_MS`/`PM_LOSS` as
  fallback. Client-only — a doubled-up sim (both roles lagging) once
  quadrupled RTT and burned a day; the CODA in the lag-sim memory, and
  the reason `PmServer::run`'s doc now says the server never lags
  itself.
- **`PM_NETDBG=1`** — the net doctor: link vitals every second from
  both roles.
- The **title bar** is the always-on dashboard: rtt, corrections,
  speed.

And the honest open item, so you don't rediscover it: under
`lag=80 loss=0.03`, `rtt_ms` (quinn's own smoothed estimate) reads
~350 ms on a 160 ms link — a consistent ~2.2× inflation, tracked by the
FIXME above `LagSocket`. The prime suspect is quinn's **pacer**
interacting with the once-per-tick pump: `poll_transmit` is only asked
for packets 60 times a second, so anything the pacer holds waits a full
16.7 ms, timers (ACK delay included) quantize to tick boundaries, and
inflated sRTT lowers the pacing rate further (rate ∝ cwnd/sRTT) — a
mild feedback loop. The planned experiment is a three-way A/B: raw UDP
echo through the same LagSocket + pump cadence (isolates sim+pump),
QUIC as configured (reproduce ~350), QUIC with a huge fixed congestion
window (defuses the pacer; quinn 0.11 has no pacing switch, but
`initial_window` on Cubic is one). If raw UDP reads ~180 and big-window
QUIC collapses to match, the diagnosis is confirmed and the fix is
config, not architecture. *(The interp=33 + cosmetic-gun work made the
game feel great despite this number — but the number is still wrong,
and wrong numbers eventually send someone down a false trail.)*

**Try this:** `PM_NETDBG=1 cargo run --release -p hogs lag=80
loss=0.03` and read the doctor's output against this chapter.

---

## 13. Telemetry: the glass cockpit

The `pm-control` crates (merged into `crates/` from their own repo)
are a second, older lineage living beside the game engine: an
industrial-control signals framework — dotted-path **signals** declared
with `pm_group!`, **faults** with debounce/latch semantics, a wire
protocol with discovery, and a **Monitor** that shadows any node it
hears. Naming note while we're here: the *packages* are `pm-world`,
`pm-world-derive`, `pm-world-sdl` (what you type after `cargo -p`),
but the *libs* keep their short names — code still says `use pm::` and
`#[pm::pod]`.

The hogs client embeds a telemetry **node**
(`examples/hogs/src/telemetry.rs`): a task at priority 95 that copies
game truth into signals each tick (`rtt_ms`, `corrections`, `frame`
prof, wave/horde/points off the replicated `Hunt`), raises an
`overrun_flt` fault when frames sustain past 40 ms — and reads
**knobs** back: `link_lag_ms`, `link_loss`, and `day_secs` are
writable from a monitor, seeded from the CLI flags. A knob write flows
through the same seams the flags used: the engine's `LinkTune` single
(the net task re-applies the link sim when its `seq` bumps) and the
game's `Tune` single (the render task reads day length every frame).
Live-tuning the simulated link *while you drive* is the whole point:
feel 80 ms become 160 ms without restarting.

One hard rule, learned from the core's design: **one node, one
thread.** Signals are `Rc` (not `Send`) and the scan clock is a
process-global cell that's only sound single-threaded — that's why the
node lives in the player client and *publishes the server's story via
replicated state* instead of a second node on the server thread.

Two windows onto it:

- `cargo run -p pm-control-host --bin pm-mon` — the TUI. Regex search,
  pin signals, **Ctrl-U** to unlock-and-tune (leases renew while held;
  Ctrl-U again releases), Tab for the fault page.
- `cargo run -p pm-control-host --bin pm-watch -- --bind
  127.0.0.1:42500 127.0.0.1:42501` — the headless sibling: one summary
  line per second, fault edges as they stamp, and `set hogs day_secs
  60` / `lock hogs day_secs` on stdin. Built so a second pair of eyes
  (human or agent) can watch a session from a plain terminal and tune
  it mid-flight.

**Try this:** start hogs, start pm-watch, then `set hogs link_lag_ms
150` while driving. Feel the gun stay instant (chapter 8) while the
world goes soupy (chapter 7) — every netcode lesson in this document,
now with a dial.

---

## 14. Where this goes next

The standing order of work, with reasons:

1. ~~Lighting~~ (landed — you're soaking in it).
2. **Play with real humans:** an `addr=` arg (`ADDR` is still
   hardcoded localhost), then reconnect-in-place — real sessions have
   drops, and a drop currently costs your truck.
3. **Hogs that look alive:** segmented parts + procedural gait (the
   heli-rotor matrix trick, no engine work), then glTF import when it
   proves out.
4. **Sound pass:** real WAVs, a looping voice for engine/rotor hum.
5. **Netcode science, background thread:** the rtt A/B above;
   staleness measurement (the residual kill-rate gap under lag);
   input-map when bindings outgrow the union pod; recordings.
6. **Content, only after look/feel:** hog variants, pickups,
   objectives — new things must *read* on screen before they're worth
   adding.

The doctrine one more time, because every future decision in this repo
gets tested against it: **clients send channels; the server replicates
pools; feel is predicted; truth is not.**
