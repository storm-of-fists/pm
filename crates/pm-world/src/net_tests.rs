//! Sync-layer tests (snapshot deltas, acks, budgets) against the
//! crate-internal `NetServer`/`NetClient` — the sync layer is deliberately
//! not public, so these live in-crate.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;

use crate::Pm;
use crate::net::{NetClient, NetError, NetServer};

const DT: f32 = 1.0 / 60.0;

#[derive(Clone, Copy, PartialEq, Debug, Default, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
struct Pos {
    x: f32,
    y: f32,
}

fn server_client_pair() -> (Pm, NetServer, Pm, NetClient) {
    let mut server = Pm::new();
    let s_pos = server.pool::<Pos>("pos");
    let mut snet = NetServer::new(&mut server);
    snet.pool_sync("pos", &s_pos);
    snet.peer_add(1);

    let mut client = Pm::new();
    client.local_peer = 1;
    let c_pos = client.pool::<Pos>("pos");
    let mut cnet = NetClient::new();
    cnet.pool_sync("pos", &c_pos);

    assert_eq!(snet.schema(), cnet.schema());
    (server, snet, client, cnet)
}

#[test]
fn snapshot_delta_replicates_state() {
    let (mut server, mut snet, mut client, cnet) = server_client_pair();
    let s_pos = server.pool::<Pos>("pos");
    let c_pos = client.pool::<Pos>("pos");

    let ids: Vec<_> = (0..3)
        .map(|i| {
            let id = server.id_add();
            s_pos.get_mut().add(
                id,
                Pos {
                    x: i as f32,
                    y: 0.0,
                },
            );
            id
        })
        .collect();
    server.loop_once(DT);

    // First snapshot carries everything.
    let snap = snet.snapshot(&server, 1).unwrap();
    let a = cnet.apply(&mut client, &snap).unwrap();
    client.loop_once(DT);
    assert_eq!(c_pos.get().len(), 3);
    assert_eq!(c_pos.get().get(ids[1]), Some(&Pos { x: 1.0, y: 0.0 }));
    assert!(client.id_alive(ids[1]));
    snet.ack(1, a.tick, a.seq);

    // After the ack, only the mutated entity rides the next delta.
    *s_pos.get_mut().get_mut(ids[0]).unwrap() = Pos { x: 10.0, y: 0.0 };
    server.loop_once(DT);
    let snap = snet.snapshot(&server, 1).unwrap();
    let header_entities = c_pos.get().len();
    let a = cnet.apply(&mut client, &snap).unwrap();
    snet.ack(1, a.tick, a.seq);
    assert_eq!(c_pos.get().len(), header_entities); // upsert, no dup
    assert_eq!(c_pos.get().get(ids[0]), Some(&Pos { x: 10.0, y: 0.0 }));

    // Fully acked: the delta is empty (label only).
    server.loop_once(DT);
    let snap = snet.snapshot(&server, 1).unwrap();
    let before = c_pos.get().get(ids[0]).copied();
    cnet.apply(&mut client, &snap).unwrap();
    assert_eq!(c_pos.get().get(ids[0]).copied(), before);
}

#[test]
fn lost_state_resends_once_a_later_ack_reveals_the_loss() {
    let (mut server, mut snet, mut client, cnet) = server_client_pair();
    let s_pos = server.pool::<Pos>("pos");
    let c_pos = client.pool::<Pos>("pos");

    let id = server.id_add();
    s_pos.get_mut().add(id, Pos { x: 5.0, y: 5.0 });
    server.loop_once(DT);

    let lost = snet.snapshot(&server, 1).unwrap();
    drop(lost); // packet loss: never applied, never acked

    // The entry is in flight, so the next snapshot doesn't re-carry it
    // (no blind resends — that's the bandwidth point of in-flight
    // tracking). The client receives and acks this one.
    server.loop_once(DT);
    let empty = snet.snapshot(&server, 1).unwrap();
    let applied = cnet.apply(&mut client, &empty).unwrap();
    assert_eq!(c_pos.get().get(id), None);

    // Acking a *later* send than the lost snapshot's proves the loss;
    // the entry becomes resendable and the next snapshot converges.
    snet.ack(1, applied.tick, applied.seq);
    server.loop_once(DT);
    let retry = snet.snapshot(&server, 1).unwrap();
    cnet.apply(&mut client, &retry).unwrap();
    assert_eq!(c_pos.get().get(id), Some(&Pos { x: 5.0, y: 5.0 }));
}

#[test]
fn silent_ack_gap_expires_in_flight_state_and_resends() {
    let (mut server, mut snet, mut client, cnet) = server_client_pair();
    let s_pos = server.pool::<Pos>("pos");
    let c_pos = client.pool::<Pos>("pos");

    let id = server.id_add();
    s_pos.get_mut().add(id, Pos { x: 5.0, y: 5.0 });
    server.loop_once(DT);
    let lost = snet.snapshot(&server, 1).unwrap();
    drop(lost);

    // No acks at all (ack datagrams lost too). After the in-flight
    // expiry window the server gives up on the snapshot and resends.
    for _ in 0..70 {
        server.loop_once(DT);
    }
    let retry = snet.snapshot(&server, 1).unwrap();
    cnet.apply(&mut client, &retry).unwrap();
    assert_eq!(c_pos.get().get(id), Some(&Pos { x: 5.0, y: 5.0 }));
}

#[test]
fn removal_replicates_and_recycling_waits_for_ack() {
    let (mut server, mut snet, mut client, cnet) = server_client_pair();
    let s_pos = server.pool::<Pos>("pos");
    let c_pos = client.pool::<Pos>("pos");

    let a = server.id_add();
    s_pos.get_mut().add(a, Pos { x: 1.0, y: 0.0 });
    server.loop_once(DT);
    let snap = snet.snapshot(&server, 1).unwrap();
    let ap = cnet.apply(&mut client, &snap).unwrap();
    snet.ack(1, ap.tick, ap.seq);
    client.loop_once(DT);
    assert!(client.id_alive(a));

    // Remove on the server. Index must NOT recycle yet: peer 1 hasn't
    // acked the removal.
    server.id_remove(a);
    server.loop_once(DT);
    snet.prune(&mut server);
    let b = server.id_add();
    assert_ne!(
        b.index(),
        a.index(),
        "recycle before ack would race the wire"
    );

    // Removal rides the delta; client applies it through the normal
    // deferred path.
    server.loop_once(DT);
    let snap = snet.snapshot(&server, 1).unwrap();
    let ap = cnet.apply(&mut client, &snap).unwrap();
    snet.ack(1, ap.tick, ap.seq);
    client.loop_once(DT);
    assert!(!client.id_alive(a));
    assert!(!c_pos.get().contains(a));

    // Acked by every peer: now the index recycles, with a bumped gen.
    snet.prune(&mut server);
    let c = server.id_add();
    assert_eq!(c.index(), a.index());
    assert_eq!(c.generation(), a.generation() + 1);
}

#[test]
fn client_local_entities_coexist_with_replicated_ones() {
    let (mut server, mut snet, mut client, cnet) = server_client_pair();
    let s_pos = server.pool::<Pos>("pos");
    let c_pos = client.pool::<Pos>("pos");

    // Client spawns a local-only entity (e.g. a predicted cosmetic).
    let local = client.id_add();
    assert_eq!(local.peer(), 1, "client allocates in its own peer space");
    c_pos.get_mut().add(local, Pos { x: -1.0, y: -1.0 });

    let remote = server.id_add();
    s_pos.get_mut().add(remote, Pos { x: 1.0, y: 1.0 });
    server.loop_once(DT);
    let snap = snet.snapshot(&server, 1).unwrap();
    cnet.apply(&mut client, &snap).unwrap();

    assert_eq!(c_pos.get().len(), 2);
    assert_eq!(c_pos.get().get(local), Some(&Pos { x: -1.0, y: -1.0 }));
    assert_eq!(c_pos.get().get(remote), Some(&Pos { x: 1.0, y: 1.0 }));
}

/// Pools are addressed on the wire by a hash of their name, not by
/// registration order — so the two ends may `sync` them in different
/// orders and replication still lands each section in the right pool.
/// Two pools with *different* value sizes make a misalignment fatal
/// (mismatched byte counts), so this only passes if the keying is correct.
#[test]
fn pool_order_is_irrelevant_keyed_by_name() {
    let mut server = Pm::new();
    let s_pos = server.pool::<Pos>("pos");
    let s_tag = server.pool::<u32>("tag");
    let mut snet = NetServer::new(&mut server);
    // Server registers pos then tag...
    snet.pool_sync("pos", &s_pos);
    snet.pool_sync("tag", &s_tag);
    snet.peer_add(1);

    let mut client = Pm::new();
    client.local_peer = 1;
    let c_tag = client.pool::<u32>("tag");
    let c_pos = client.pool::<Pos>("pos");
    let mut cnet = NetClient::new();
    // ...client registers tag then pos (reversed).
    cnet.pool_sync("tag", &c_tag);
    cnet.pool_sync("pos", &c_pos);

    let id = server.id_add();
    s_pos.get_mut().add(id, Pos { x: 3.0, y: 4.0 });
    s_tag.get_mut().add(id, 0xABCD);
    server.loop_once(DT);

    let snap = snet.snapshot(&server, 1).unwrap();
    cnet.apply(&mut client, &snap).unwrap();

    assert_eq!(c_pos.get().get(id), Some(&Pos { x: 3.0, y: 4.0 }));
    assert_eq!(c_tag.get().get(id), Some(&0xABCD));
}

#[test]
fn malformed_snapshots_error_instead_of_panicking() {
    let (mut server, mut snet, mut client, cnet) = server_client_pair();
    let s_pos = server.pool::<Pos>("pos");
    let id = server.id_add();
    s_pos.get_mut().add(id, Pos { x: 1.0, y: 2.0 });
    server.loop_once(DT);

    let snap = snet.snapshot(&server, 1).unwrap();
    assert_eq!(
        cnet.apply(&mut client, &snap[..3]),
        Err(NetError::Truncated)
    );
    assert_eq!(
        cnet.apply(&mut client, &snap[..snap.len() - 1]),
        Err(NetError::Truncated)
    );
}

/// The milestone test: two live kernels, net logic running as ordinary pm
/// tasks, state flowing through in-memory queues with acks coming back.
#[test]
fn two_pms_converge_through_tasked_net_loop() {
    let s2c: Rc<RefCell<VecDeque<Vec<u8>>>> = Rc::default();
    let c2s_acks: Rc<RefCell<VecDeque<(u32, u32)>>> = Rc::default();

    // --- server ---
    let mut server = Pm::new();
    let s_pos = server.pool::<Pos>("pos");
    let mut snet = NetServer::new(&mut server);
    snet.pool_sync("pos", &s_pos);
    snet.peer_add(1);

    let ids: Vec<_> = (0..50)
        .map(|i| {
            let id = server.id_add();
            s_pos.get_mut().add(
                id,
                Pos {
                    x: i as f32,
                    y: 0.0,
                },
            );
            id
        })
        .collect();

    // Physics runs for 30 ticks, then holds still so both sides settle.
    server.task_add("physics", 30.0, 0.0, {
        let pos = s_pos.clone();
        move |pm| {
            if pm.tick() <= 30 {
                for (_, mut p) in pos.get_mut().iter_mut() {
                    p.x += 1.0;
                    p.y += 0.5;
                }
            }
        }
    });

    // Net-send first in the tick (low priority): drain acks, snapshot, prune.
    server.task_add("net_send", 5.0, 0.0, {
        let s2c = s2c.clone();
        let acks = c2s_acks.clone();
        move |pm| {
            while let Some((tick, seq)) = acks.borrow_mut().pop_front() {
                snet.ack(1, tick, seq);
            }
            s2c.borrow_mut().push_back(snet.snapshot(pm, 1).unwrap());
            snet.prune(pm);
        }
    });

    // --- client ---
    let mut client = Pm::new();
    client.local_peer = 1;
    let c_pos = client.pool::<Pos>("pos");
    let mut cnet_setup = NetClient::new();
    cnet_setup.pool_sync("pos", &c_pos);
    let cnet = cnet_setup;

    client.task_add("net_recv", 5.0, 0.0, {
        let s2c = s2c.clone();
        let acks = c2s_acks.clone();
        move |pm| {
            while let Some(snap) = s2c.borrow_mut().pop_front() {
                let a = cnet.apply(pm, &snap).unwrap();
                acks.borrow_mut().push_back((a.tick, a.seq));
            }
        }
    });

    // Pump both worlds, dropping every 4th snapshot to simulate loss.
    for round in 0..40 {
        server.loop_once(DT);
        if round % 4 == 0 {
            s2c.borrow_mut().pop_front();
        }
        client.loop_once(DT);
    }

    // Physics stopped at tick 30 and later rounds flushed the queues:
    // the client must hold the exact server state.
    assert_eq!(c_pos.get().len(), 50);
    for &id in &ids {
        assert_eq!(
            c_pos.get().get(id),
            s_pos.get().get(id),
            "entity {id:?} diverged"
        );
    }
    // Kernel ticks start at 1, so "tick <= 30" fires on ticks 2..=30.
    assert_eq!(s_pos.get().get(ids[0]), Some(&Pos { x: 29.0, y: 14.5 }));
}

#[test]
fn dense_pool_streams_through_a_byte_budget() {
    // The hellfire shape: many entities, ALL changed every tick, with a
    // snapshot budget an order of magnitude too small for one tick's
    // changes. The old tick-cursor delta model livelocked here; the
    // per-entity model must rotate everyone through within N/K snapshots.
    let (mut server, mut snet, mut client, cnet) = server_client_pair();
    let s_pos = server.pool::<Pos>("pos");
    let c_pos = client.pool::<Pos>("pos");

    let mut ids = Vec::new();
    for i in 0..300 {
        let id = server.id_add();
        s_pos.get_mut().add(
            id,
            Pos {
                x: i as f32,
                y: 0.0,
            },
        );
        ids.push(id);
    }

    const BUDGET: usize = 500; // ~40 entries of 12 bytes
    let mut rounds = 0;
    while c_pos.get().len() < 300 {
        rounds += 1;
        assert!(rounds <= 12, "rotation failed to cover all entities");
        server.loop_once(DT);
        // Everything moves every tick — worst case for delta models.
        for (_, mut p) in s_pos.get_mut().iter_mut() {
            p.y += 1.0;
        }
        let snap = snet.snapshot_budgeted(&server, 1, BUDGET).unwrap();
        assert!(
            snap.bytes.len() <= BUDGET,
            "snapshot {} > budget",
            snap.bytes.len()
        );
        assert!(snap.more, "a dirty horde always outweighs this budget");
        let applied = cnet.apply(&mut client, &snap.bytes).unwrap();
        snet.ack(1, applied.tick, applied.seq);
    }
    // ceil(300/40) = 8 snapshots to cover everyone.
    assert!(rounds >= 8, "budget should force rotation, got {rounds}");

    // And the same mechanism converges to silence when changes stop.
    for _ in 0..70 {
        server.loop_once(DT);
        let snap = snet.snapshot_budgeted(&server, 1, BUDGET).unwrap();
        let applied = cnet.apply(&mut client, &snap.bytes).unwrap();
        snet.ack(1, applied.tick, applied.seq);
    }
    server.loop_once(DT);
    let quiet = snet.snapshot_budgeted(&server, 1, BUDGET).unwrap();
    // Header (13: tick + send seq + input-seq + empty owner table) +
    // removal count (4) + section count (2) + one section header (6) and
    // zero entries.
    assert_eq!(
        quiet.bytes.len(),
        25,
        "fully-confirmed world should pack empty"
    );
    assert_eq!(quiet.entries, 0);
    assert!(!quiet.more, "a converged world reports a drained backlog");
}

#[test]
fn dense_pool_does_not_starve_pools_registered_after_it() {
    // The hogs-at-200 shape: a change-dense pool registered FIRST whose
    // per-tick dirty set alone outweighs the whole budget, with a sparse
    // pool (the scoreboard single, the bullet burst) registered AFTER it.
    // Registration-order packing starves the sparse pool forever;
    // smallest-dirty-first packing must deliver it promptly.
    let mut server = Pm::new();
    let s_horde = server.pool::<Pos>("horde");
    let s_score = server.pool::<Pos>("score");
    let mut snet = NetServer::new(&mut server);
    snet.pool_sync("horde", &s_horde);
    snet.pool_sync("score", &s_score);
    snet.peer_add(1);

    let mut client = Pm::new();
    client.local_peer = 1;
    let c_horde = client.pool::<Pos>("horde");
    let c_score = client.pool::<Pos>("score");
    let cnet = {
        let mut c = NetClient::new();
        c.pool_sync("horde", &c_horde);
        c.pool_sync("score", &c_score);
        c
    };

    for i in 0..200 {
        let id = server.id_add();
        s_horde.get_mut().add(id, Pos { x: i as f32, y: 0.0 });
    }
    let score_id = server.id_add();
    s_score.get_mut().add(score_id, Pos { x: 0.0, y: 0.0 });
    server.loop_once(DT);

    // ~40 entries fit; the horde alone re-dirties 200 every tick.
    const BUDGET: usize = 500;
    for round in 0..30 {
        server.loop_once(DT);
        for (_, mut p) in s_horde.get_mut().iter_mut() {
            p.y += 1.0; // whole horde moves every tick
        }
        *s_score.get_mut().get_mut(score_id).unwrap() = Pos {
            x: round as f32 + 1.0,
            y: 0.0,
        }; // scoreboard also changes every tick
        let snap = snet.snapshot_budgeted(&server, 1, BUDGET).unwrap();
        assert!(
            snap.bytes.len() <= BUDGET,
            "snapshot {} > budget",
            snap.bytes.len()
        );
        let applied = cnet.apply(&mut client, &snap.bytes).unwrap();
        snet.ack(1, applied.tick, applied.seq);
        client.loop_once(DT);
    }

    // The sparse pool stayed fresh (within an ack round-trip of the last
    // write), and the horde still streamed through the leftover budget.
    let score = c_score.get().get(score_id).copied().expect("score never arrived");
    assert!(
        score.x >= 28.0,
        "scoreboard stale: saw {}, server wrote 30",
        score.x
    );
    assert!(
        c_horde.get().len() == 200,
        "horde should still stream through leftovers, got {}",
        c_horde.get().len()
    );
}

/// One tick, several sends — the multi-datagram flight. Each call packs
/// the NEXT chunk (packed entries go in-flight, so nothing repeats),
/// `more` reports when the backlog drains, and the whole dirty set lands
/// within the tick where the single-datagram cadence took ceil(N/K)
/// ticks of rotation.
#[test]
fn same_tick_flight_drains_the_backlog() {
    let (mut server, mut snet, mut client, cnet) = server_client_pair();
    let s_pos = server.pool::<Pos>("pos");
    let c_pos = client.pool::<Pos>("pos");
    for i in 0..300 {
        let id = server.id_add();
        s_pos.get_mut().add(
            id,
            Pos {
                x: i as f32,
                y: 0.0,
            },
        );
    }
    server.loop_once(DT);

    const BUDGET: usize = 500; // ~39 entries per datagram
    let mut sends = 0;
    loop {
        sends += 1;
        assert!(sends <= 12, "flight failed to drain the backlog");
        let snap = snet.snapshot_budgeted(&server, 1, BUDGET).unwrap();
        assert!(snap.bytes.len() <= BUDGET);
        let a = cnet.apply(&mut client, &snap.bytes).unwrap();
        snet.ack(1, a.tick, a.seq);
        if !snap.more {
            break;
        }
    }
    assert!(sends >= 8, "300 dirty entities need several datagrams, took {sends}");
    assert_eq!(c_pos.get().len(), 300, "one tick's flight carried everyone");

    // Fully acked flight: the next tick opens with a drained backlog.
    server.loop_once(DT);
    let quiet = snet.snapshot_budgeted(&server, 1, BUDGET).unwrap();
    assert_eq!(quiet.entries, 0);
    assert!(!quiet.more);
}

/// Two same-tick sends share a tick label — only the per-send seq tells
/// their acks apart (the ambiguity that blocked flights). Losing the
/// FIRST send and acking the SECOND must declare exactly the first
/// send's entries lost (they resend), while the second's stay confirmed
/// (no blind repeat).
#[test]
fn same_tick_sends_ack_by_seq_not_label() {
    let (mut server, mut snet, mut client, cnet) = server_client_pair();
    let s_pos = server.pool::<Pos>("pos");
    let c_pos = client.pool::<Pos>("pos");
    for i in 0..60 {
        let id = server.id_add();
        s_pos.get_mut().add(
            id,
            Pos {
                x: i as f32,
                y: 0.0,
            },
        );
    }
    server.loop_once(DT);

    const BUDGET: usize = 400; // ~31 entries: 60 dirty = two sends
    let first = snet.snapshot_budgeted(&server, 1, BUDGET).unwrap();
    assert!(first.more, "60 entries must not fit one 400 B budget");
    let second = snet.snapshot_budgeted(&server, 1, BUDGET).unwrap();
    assert!(second.entries > 0);
    assert!(!second.more, "two sends cover 60 entries");

    drop(first.bytes); // lost on the wire; only the second arrives
    let a = cnet.apply(&mut client, &second.bytes).unwrap();
    snet.ack(1, a.tick, a.seq);

    // The ack settled the second send as received and the first as lost:
    // the next send carries exactly the lost chunk, nothing more.
    let third = snet.snapshot_budgeted(&server, 1, BUDGET).unwrap();
    assert_eq!(
        third.entries, first.entries,
        "exactly the lost send's entries resend"
    );
    let a = cnet.apply(&mut client, &third.bytes).unwrap();
    snet.ack(1, a.tick, a.seq);
    assert_eq!(c_pos.get().len(), 60);

    // Everything confirmed: a fresh send packs empty.
    let quiet = snet.snapshot_budgeted(&server, 1, BUDGET).unwrap();
    assert_eq!(quiet.entries, 0);
}

#[test]
fn owner_table_rides_every_header() {
    let (mut server, mut snet, mut client, cnet) = server_client_pair();
    let id1 = server.id_add();
    let id2 = server.id_add();
    server.loop_once(DT);

    // Unsorted input: the wire table comes back sorted by peer.
    snet.owners_set(vec![(2, id2.0), (1, id1.0)]);
    let snap = snet.snapshot(&server, 1).unwrap();
    let applied = cnet.apply(&mut client, &snap).unwrap();
    assert_eq!(applied.owners, vec![(1, id1), (2, id2)]);

    // The table rides EVERY header, not just the first — a lost snapshot
    // costs nothing.
    server.loop_once(DT);
    let snap = snet.snapshot(&server, 1).unwrap();
    let applied = cnet.apply(&mut client, &snap).unwrap();
    assert_eq!(applied.owners, vec![(1, id1), (2, id2)]);

    // A cleared table packs (and parses) empty.
    snet.owners_set(Vec::new());
    server.loop_once(DT);
    let snap = snet.snapshot(&server, 1).unwrap();
    assert!(cnet.apply(&mut client, &snap).unwrap().owners.is_empty());
}

/// The pack/apply micro-bench that used to live in the public `bench`
/// example — in-crate now that the sync layer isn't public. Run release
/// or the numbers are fiction:
///
///     cargo test --release -p pm --lib -- --ignored net_bench --nocapture
#[test]
#[ignore]
fn net_bench_pack_apply() {
    use std::hint::black_box;
    use std::time::Instant;

    fn time(label: &str, ops: u64, f: impl FnOnce()) {
        let t = Instant::now();
        f();
        let ns = t.elapsed().as_nanos() as f64 / ops as f64;
        println!("  {label:<42} {ns:>9.2} ns/op");
    }

    println!("-- net sync (10k entities, one peer) --");
    const M: u32 = 10_000;
    let mut spm = Pm::new();
    let s_pos = spm.pool::<Pos>("pos");
    let mut net = NetServer::new(&mut spm);
    net.pool_sync("pos", &s_pos);
    net.peer_add(1);
    for i in 0..M {
        let id = spm.id_add();
        s_pos.get_mut().add(
            id,
            Pos {
                x: i as f32,
                y: 0.0,
            },
        );
    }
    spm.loop_once(1.0 / 60.0);

    let mut cpm = Pm::new();
    let c_pos = cpm.pool::<Pos>("pos");
    let mut cnet = NetClient::new();
    cnet.pool_sync("pos", &c_pos);

    let mut snap = Vec::new();
    time("pack, all dirty (uncapped)", M as u64, || {
        snap = net.snapshot(&spm, 1).unwrap();
    });
    println!("    ({} KiB on the wire)", snap.len() / 1024);
    time("apply, all entries", M as u64, || {
        cnet.apply(&mut cpm, &snap).unwrap();
    });
    let label = u32::from_le_bytes(snap[0..4].try_into().unwrap());
    let seq = u32::from_le_bytes(snap[4..8].try_into().unwrap());
    net.ack(1, label, seq);
    spm.loop_once(1.0 / 60.0);
    // The documented known limit: a converged pool still costs a full
    // per-peer scan every net tick. This is that scan, per entity.
    time("pack, converged (idle scan)", M as u64, || {
        black_box(net.snapshot_budgeted(&spm, 1, 1200));
    });
}

// --- wire pools (quantized reprs) ------------------------------------------

/// Manual `Wire` impl mirroring what `#[derive(pm::Wire)]` generates (the
/// derive's generated code references `::pm`, which doesn't resolve
/// in-crate — the derive itself is covered in `tests/wire.rs`).
#[derive(Clone, Copy, PartialEq, Debug, Default, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
struct QPos {
    x: f32,
    y: f32,
}

#[repr(C, packed)]
#[derive(Clone, Copy)]
struct QPosWire {
    x: i16,
    y: i16,
}

unsafe impl bytemuck::Zeroable for QPosWire {}
unsafe impl bytemuck::Pod for QPosWire {}

const QSCALE: f32 = 64.0;

impl crate::net::Wire for QPos {
    type Repr = QPosWire;
    fn to_repr(&self) -> QPosWire {
        QPosWire {
            x: (self.x * QSCALE).round() as i16,
            y: (self.y * QSCALE).round() as i16,
        }
    }
    fn from_repr(repr: QPosWire) -> QPos {
        QPos {
            x: repr.x as f32 / QSCALE,
            y: repr.y as f32 / QSCALE,
        }
    }
}

#[test]
fn wire_pool_replicates_quantized() {
    let mut server = Pm::new();
    let s_pos = server.pool::<QPos>("qpos");
    let mut snet = NetServer::new(&mut server);
    snet.pool_wire("qpos", &s_pos);
    snet.peer_add(1);

    let mut client = Pm::new();
    client.local_peer = 1;
    let c_pos = client.pool::<QPos>("qpos");
    let mut cnet = NetClient::new();
    cnet.pool_wire("qpos", &c_pos);

    // The handshake compares the REPR size (4 B), not the pool's (8 B).
    assert_eq!(snet.schema(), cnet.schema());
    assert_eq!(snet.schema()[0].2, size_of::<QPosWire>());

    let id = server.id_add();
    s_pos.get_mut().add(
        id,
        QPos {
            x: 1.23456,
            y: -3.14159,
        },
    );
    server.loop_once(DT);

    let snap = snet.snapshot(&server, 1).unwrap();
    let a = cnet.apply(&mut client, &snap).unwrap();
    snet.ack(1, a.tick, a.seq);

    // The client sees the quantized-back value: round(v * 64) / 64.
    let got = *c_pos.get().get(id).unwrap();
    assert_eq!(got.x, (1.23456f32 * QSCALE).round() / QSCALE);
    assert_eq!(got.y, (-3.14159f32 * QSCALE).round() / QSCALE);
    assert!((got.x - 1.23456).abs() < 1.0 / QSCALE);

    // Converged: the next delta carries no entries for this pool.
    server.loop_once(DT);
    let quiet = snet.snapshot(&server, 1).unwrap();
    assert!(quiet.len() < snap.len());

    // Change again: the delta entry is 4 B id + 4 B repr, not 8 B pod.
    s_pos.get_mut().get_mut(id).unwrap().x = 500.0;
    server.loop_once(DT);
    let delta = snet.snapshot(&server, 1).unwrap();
    assert_eq!(delta.len(), quiet.len() + 4 + size_of::<QPosWire>());
    let a = cnet.apply(&mut client, &delta).unwrap();
    snet.ack(1, a.tick, a.seq);
    // 500 * 64 = 32000 still fits i16; exact after roundtrip.
    assert_eq!(c_pos.get().get(id).unwrap().x, 500.0);
}
