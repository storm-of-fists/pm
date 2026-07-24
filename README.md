# pm

A data-oriented game framework in Rust: a flat task scheduler, sparse-set
component pools, and networking as a first-class core concern —
server-authoritative replication, client prediction, and dylib hot-reload
mods are built in, not bolted on. The reference lives in the crate docs
(on the types it describes, so it can't drift from the code); this file
just points you at them. Work that hasn't landed yet lives as a TODO
comment next to the code it will change, in tiers (split 2026-07-20,
when the goal became SHIP THE GAME): `TODO(ship)` is the shipping queue
— player-facing work, and the only list that gets touched by default;
`TODO(roadmap)` is engine work and scaling cliffs, touched only when a
ship item is actually blocked by one; `TODO(v2)` is the engine-v2
adoption list (GREENLIT 2026-07-22 — landing in-place, order 3→2→1→4;
the list and per-item status live at the top of pm-world's lib.rs);
`TODO(story)` is the game's lore, Connor's alone — capture, never
embellish.

```bash
cargo doc -p pm-world --open   # the docs: API tour, design, netcode, 3D, mods
cargo test --workspace    # all tests, incl. doctests + QUIC loopback
cargo run --release -p hogs   # THE game (solids = renderer scratchpad; the old demo/drive/hellfire examples were DELETED 2026-07-23 — hogs eclipses them, see git history)
grep -rn "TODO(ship)" crates/ examples/      # the shipping queue — work this
grep -rn "TODO(roadmap)" crates/ examples/   # engine queue — only if blocking
grep -rn "TODO(v2)" crates/                  # engine-v2 adoption — greenlit, in flight
grep -rn "TODO(story)" examples/             # the lore — Connor authors this
grep -rn "TODO(simplify)" crates/ examples/  # zoom-outs/generalizations noticed while working — the standing simplification track
grep -rn "TODO(BUG)" crates/ examples/       # known landmines — a fix in place that only holds by convention (load-bearing ordering, fragile contract); each one names the engine enforcement that retires it
grep -rn "TODO(box3d-move)" examples/        # hand-rolled geometry earmarked for the solver — NEXT UP: clients predict by stepping a local Box3D world (DECIDED 2026-07-23; master note atop examples/hogs/src/phys.rs)
grep -rn "STYLE(" crates/ examples/          # style notes — where a feel or design deliberately chases a named inspiration (STYLE(motorstorm), STYLE(source), ...): what we're mimicking and which parts of it we want
```

New here (or want the whole system explained start to finish)? Open the
crate docs and follow the **Guided tour** section at the top — the
module docs read in order (pools → tasks → replication → prediction →
lag comp → cosmetics → physics → the lag lab), with `examples/hogs` as
the worked example and hands-on experiments along the way.
