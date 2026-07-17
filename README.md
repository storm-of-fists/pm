# pm

A data-oriented game framework in Rust: a flat task scheduler, sparse-set
component pools, and networking as a first-class core concern —
server-authoritative replication, client prediction, and dylib hot-reload
mods are built in, not bolted on. The reference lives in the crate docs
(on the types it describes, so it can't drift from the code); this file
just points you at them.

```bash
cargo doc -p pm-world --open   # the docs: API tour, design, netcode, 3D, mods
cargo test --workspace    # all tests, incl. doctests + QUIC loopback
cargo run --release -p hellfire   # the flagship example (try -p demo / drive / solids)
```

New here (or want the whole system explained start to finish)? Read
[docs/journey.md](docs/journey.md) — the guided tour: pools → tasks →
replication → prediction → lag comp → cosmetics → physics → rendering →
audio → the lag lab, each chapter with a hands-on experiment.
