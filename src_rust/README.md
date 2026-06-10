# pm (Rust)

A ground-up restart of the pm kernel in Rust. Not a transliteration — same
philosophy (data-oriented, flat scheduler, sparse-set ECS, noun_verb API),
redesigned where C++ idioms don't survive the borrow checker.

## Build & test

```bash
cd src_rust
cargo test                          # all tests (incl. QUIC loopback)
cargo run --release --example sim   # headless perf sanity check
cargo run --release --example demo  # networked demo: server + client over QUIC
cargo clippy --all-targets          # lints
```

The demo connects 8 clients to a QUIC server — 7 bots and you. Every peer
drives its own server-authoritative vehicle (input as sequenced datagrams,
state back as snapshot deltas); yours renders as an arrow showing its
heading, steered with WASD; `p` toggles a live profiling panel; `q` quits.
Run roles separately with `-- server` / `-- client` / `-- bot` (default is
everything in one process). Feel a real link on the player's connection:

```bash
PM_LAG_MS=80 PM_LOSS=0.05 cargo run --release --example demo
```

## Profiling

- `pm.task_stats()` — always-on per-task timing (calls / total / max ns),
  collected by `loop_once` at ~80 ns overhead per task call;
  `task_stats_reset()` to window it. The demo's `p` panel and the
  dedicated server's 5-second log are built on this.
- `pm::probe::scope("name")` — drop-in scoped probe for hot spots *inside*
  a task; thread-local, read with `pm::probe::stats()`.
- `QuicServer/QuicClient::link_lag_set(delay, loss)` — simulate link
  conditions both directions; QUIC's RTT/loss handling reacts as if real.

## Mapping from C++

| C++ | Rust | Notes |
|-----|------|-------|
| `pm.pool_get<T>("name")` → `Pool<T>*` | `pm.pool_get::<T>("name")` → `Rc<RefCell<Pool<T>>>` | Clone the handle into the task closure at init, `borrow_mut()` inside the task |
| `pm.state_get<T>("name")` → `T*` | `pm.state_get::<T>("name")` → `Rc<RefCell<T>>` | Same singleton-on-refetch behavior; `T: Default` |
| `pm.task_add(name, prio, lambda)` | `pm.task_add(name, prio, closure)` | Same flat priority scheduler, lowest first |
| `pm.task_add(name, prio, interval, fn)` | `pm.task_add_every(name, prio, interval, fn)` | |
| `pool->each(fn)` + `PoolEntry::get_mut()` | `pool.iter()` (reads) / `pool.iter_mut()` (writes) | `iter_mut` yields `Mut` handles — exact `PoolEntry` semantics: stamped changed only when written through |
| `pool->get(id)` → `PoolEntry` | `pool.get(id)` / `pool.get_mut(id)` → `Option<&T>` / `Option<Mut<T>>` | `Mut` derefs like `&mut T`, stamps the changed-tick on mutable deref only |
| `pool->change_count(id)` | `pool.changed_tick(id)` / `pool.changed_since(tick)` | Tick stamps instead of counters — a peer's whole view state is one acked tick (see [SYNC_DESIGN.md](SYNC_DESIGN.md)) |
| TaskFault on bad access | `RefCell` borrow panic / `Option` | A borrow panic means two tasks held the same pool mutably — a real bug either way |
| `pm.id_add(peer)` | `pm.id_add()` / `pm.id_add_for(peer)` | **Diverges:** generational `[peer:8 \| gen:8 \| index:16]`, FIFO-recycled, recycling gated by the removal log — bounded memory, stale handles fail the gen check |
| `pm.id_remove(id)` | same | Deferred, flushed across all pools at end of `loop_once`, logged for sync |
| `NetSys` (`pm_udp.hpp`) | `NetServer` / `NetClient` | Snapshot-delta replication, headless so far — transport (QUIC via quinn-proto) is the next phase |
| `loop_run`, `loop_dt`, `loop_quit`, `loop_rate` | same | `loop_once(dt)` is public for headless/test driving |

## Design decisions

- **`Rc<RefCell<...>>` handles instead of raw pointers.** The C++ pattern
  ("fetch during init, capture the pointer in the lambda") maps directly:
  fetch during init, clone the `Rc` into the closure. Borrows are checked at
  runtime; the cost is one counter check per `borrow_mut()`, done once per
  task per tick — not per entity — so it doesn't show up in the hot loop.
- **Single-threaded kernel for now.** Parallel `each` needs a different
  mechanism in Rust (rayon over dense slices, or task-declared pool access).
  Decide after the kernel feels right, not before.
- **Tasks take `&mut Pm`.** The scheduler moves the task list out of `Pm`
  during a tick so tasks can borrow the kernel mutably (`id_add`, `loop_dt`,
  `task_add`, `loop_quit`). Tasks added mid-tick start next tick.

## Perf

`cargo run --release --example sim`: 100k entities × 600 ticks,
velocity→position join (dense iteration + sparse lookup per entity):
**~2.3 ns per entity-update** on the same 20-core WSL reference machine.
(~1.2 ns before sync foundations; the difference is the generation check
per lookup and the `Mut` guard's changed-tick stamps — the price of
network-ready change tracking, still <1% of a 60 Hz frame at 100k
entities.)

## Roadmap

Multiplayer sync is the core concern and comes first — see
[SYNC_DESIGN.md](SYNC_DESIGN.md) for the full design (tick-versioned change
tracking, `Mut` guard, snapshot-delta over UDP).

1. ~~**Sync foundations**~~ — DONE: kernel tick + removal-log-gated id
   recycling, generational ids, `changed_tick` + `Mut` guard,
   `changed_since(tick)` query
2. ~~**NetSys headless**~~ — DONE: `NetServer`/`NetClient`, ack-cursor
   deltas, removal replication, ack-gated recycling; proven by two `Pm`
   instances converging through in-memory queues under packet loss
3. ~~**QUIC transport**~~ — DONE: `QuicServer`/`QuicClient` over
   quinn-proto (sans-IO, no async runtime), pumped by an ordinary net
   task; schema-checked handshake, snapshot datagrams + acks, typed
   events on the reliable stream. Try it: `cargo run --release
   --example demo`
4. **Client conventions** — input channel DONE (sequenced redundant
   datagrams, in-order gap-tolerant delivery, input-seq echo in snapshot
   headers); remaining: prediction + reconciliation replay, dead
   reckoning, fixed-timestep sim mode
5. **Kernel polish** — `remove_all` (deferred), `clear_world`, typed event
   queues, join iterator (`each_with`)
6. **Math + util** — Vec2, Rng (xorshift32), Hysteresis/Cooldown/Latch/edges
7. **Benchmarks** — threshold-based regression gates like the C++ suite
8. **Parallel iteration** — rayon over dense slices behind an explicit opt-in
9. **SDL3** — evaluate `sdl3` crate maturity vs wgpu/winit when we get there
10. **Mod hot-reload** — the hard one. No stable Rust ABI, so the mod
    boundary must be `extern "C"` or a scripting layer (wasm?). Decide late.
