use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;

use pm::{NetClient, NetError, NetServer, Pm};

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
            s_pos.borrow_mut().add(id, Pos { x: i as f32, y: 0.0 });
            id
        })
        .collect();
    server.loop_once(DT);

    // First snapshot carries everything.
    let snap = snet.snapshot(&server, 1).unwrap();
    let ack = cnet.apply(&mut client, &snap).unwrap().tick;
    client.loop_once(DT);
    assert_eq!(c_pos.borrow().len(), 3);
    assert_eq!(c_pos.borrow().get(ids[1]), Some(&Pos { x: 1.0, y: 0.0 }));
    assert!(client.id_alive(ids[1]));
    snet.ack(1, ack);

    // After the ack, only the mutated entity rides the next delta.
    *s_pos.borrow_mut().get_mut(ids[0]).unwrap() = Pos { x: 10.0, y: 0.0 };
    server.loop_once(DT);
    let snap = snet.snapshot(&server, 1).unwrap();
    let header_entities = c_pos.borrow().len();
    let ack = cnet.apply(&mut client, &snap).unwrap().tick;
    snet.ack(1, ack);
    assert_eq!(c_pos.borrow().len(), header_entities); // upsert, no dup
    assert_eq!(c_pos.borrow().get(ids[0]), Some(&Pos { x: 10.0, y: 0.0 }));

    // Fully acked: the delta is empty (label only).
    server.loop_once(DT);
    let snap = snet.snapshot(&server, 1).unwrap();
    let before = c_pos.borrow().get(ids[0]).copied();
    cnet.apply(&mut client, &snap).unwrap();
    assert_eq!(c_pos.borrow().get(ids[0]).copied(), before);
}

#[test]
fn lost_state_resends_once_a_later_ack_reveals_the_loss() {
    let (mut server, mut snet, mut client, cnet) = server_client_pair();
    let s_pos = server.pool::<Pos>("pos");
    let c_pos = client.pool::<Pos>("pos");

    let id = server.id_add();
    s_pos.borrow_mut().add(id, Pos { x: 5.0, y: 5.0 });
    server.loop_once(DT);

    let lost = snet.snapshot(&server, 1).unwrap();
    drop(lost); // packet loss: never applied, never acked

    // The entry is in flight, so the next snapshot doesn't re-carry it
    // (no blind resends — that's the bandwidth point of in-flight
    // tracking). The client receives and acks this one.
    server.loop_once(DT);
    let empty = snet.snapshot(&server, 1).unwrap();
    let applied = cnet.apply(&mut client, &empty).unwrap();
    assert_eq!(c_pos.borrow().get(id), None);

    // Acking a *later* label than the lost snapshot's proves the loss;
    // the entry becomes resendable and the next snapshot converges.
    snet.ack(1, applied.tick);
    server.loop_once(DT);
    let retry = snet.snapshot(&server, 1).unwrap();
    cnet.apply(&mut client, &retry).unwrap();
    assert_eq!(c_pos.borrow().get(id), Some(&Pos { x: 5.0, y: 5.0 }));
}

#[test]
fn silent_ack_gap_expires_in_flight_state_and_resends() {
    let (mut server, mut snet, mut client, cnet) = server_client_pair();
    let s_pos = server.pool::<Pos>("pos");
    let c_pos = client.pool::<Pos>("pos");

    let id = server.id_add();
    s_pos.borrow_mut().add(id, Pos { x: 5.0, y: 5.0 });
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
    assert_eq!(c_pos.borrow().get(id), Some(&Pos { x: 5.0, y: 5.0 }));
}

#[test]
fn removal_replicates_and_recycling_waits_for_ack() {
    let (mut server, mut snet, mut client, cnet) = server_client_pair();
    let s_pos = server.pool::<Pos>("pos");
    let c_pos = client.pool::<Pos>("pos");

    let a = server.id_add();
    s_pos.borrow_mut().add(a, Pos { x: 1.0, y: 0.0 });
    server.loop_once(DT);
    let snap = snet.snapshot(&server, 1).unwrap();
    snet.ack(1, cnet.apply(&mut client, &snap).unwrap().tick);
    client.loop_once(DT);
    assert!(client.id_alive(a));

    // Remove on the server. Index must NOT recycle yet: peer 1 hasn't
    // acked the removal.
    server.id_remove(a);
    server.loop_once(DT);
    snet.prune(&mut server);
    let b = server.id_add();
    assert_ne!(b.index(), a.index(), "recycle before ack would race the wire");

    // Removal rides the delta; client applies it through the normal
    // deferred path.
    server.loop_once(DT);
    let snap = snet.snapshot(&server, 1).unwrap();
    snet.ack(1, cnet.apply(&mut client, &snap).unwrap().tick);
    client.loop_once(DT);
    assert!(!client.id_alive(a));
    assert!(!c_pos.borrow().contains(a));

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
    c_pos.borrow_mut().add(local, Pos { x: -1.0, y: -1.0 });

    let remote = server.id_add();
    s_pos.borrow_mut().add(remote, Pos { x: 1.0, y: 1.0 });
    server.loop_once(DT);
    let snap = snet.snapshot(&server, 1).unwrap();
    cnet.apply(&mut client, &snap).unwrap();

    assert_eq!(c_pos.borrow().len(), 2);
    assert_eq!(c_pos.borrow().get(local), Some(&Pos { x: -1.0, y: -1.0 }));
    assert_eq!(c_pos.borrow().get(remote), Some(&Pos { x: 1.0, y: 1.0 }));
}

#[test]
fn malformed_snapshots_error_instead_of_panicking() {
    let (mut server, mut snet, mut client, cnet) = server_client_pair();
    let s_pos = server.pool::<Pos>("pos");
    let id = server.id_add();
    s_pos.borrow_mut().add(id, Pos { x: 1.0, y: 2.0 });
    server.loop_once(DT);

    let snap = snet.snapshot(&server, 1).unwrap();
    assert_eq!(cnet.apply(&mut client, &snap[..3]), Err(NetError::Truncated));
    assert_eq!(cnet.apply(&mut client, &snap[..snap.len() - 1]), Err(NetError::Truncated));
}

/// The milestone test: two live kernels, net logic running as ordinary pm
/// tasks, state flowing through in-memory queues with acks coming back.
#[test]
fn two_pms_converge_through_tasked_net_loop() {
    let s2c: Rc<RefCell<VecDeque<Vec<u8>>>> = Rc::default();
    let c2s_acks: Rc<RefCell<VecDeque<u32>>> = Rc::default();

    // --- server ---
    let mut server = Pm::new();
    let s_pos = server.pool::<Pos>("pos");
    let mut snet = NetServer::new(&mut server);
    snet.pool_sync("pos", &s_pos);
    snet.peer_add(1);

    let ids: Vec<_> = (0..50)
        .map(|i| {
            let id = server.id_add();
            s_pos.borrow_mut().add(id, Pos { x: i as f32, y: 0.0 });
            id
        })
        .collect();

    // Physics runs for 30 ticks, then holds still so both sides settle.
    server.task_add("physics", 30.0, 0.0, {
        let pos = s_pos.clone();
        move |pm| {
            if pm.tick() <= 30 {
                for (_, mut p) in pos.borrow_mut().iter_mut() {
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
            while let Some(tick) = acks.borrow_mut().pop_front() {
                snet.ack(1, tick);
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
                let tick = cnet.apply(pm, &snap).unwrap().tick;
                acks.borrow_mut().push_back(tick);
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
    assert_eq!(c_pos.borrow().len(), 50);
    for &id in &ids {
        assert_eq!(c_pos.borrow().get(id), s_pos.borrow().get(id), "entity {id:?} diverged");
    }
    // Kernel ticks start at 1, so "tick <= 30" fires on ticks 2..=30.
    assert_eq!(s_pos.borrow().get(ids[0]), Some(&Pos { x: 29.0, y: 14.5 }));
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
        s_pos.borrow_mut().add(id, Pos { x: i as f32, y: 0.0 });
        ids.push(id);
    }

    const BUDGET: usize = 500; // ~40 entries of 12 bytes
    let mut rounds = 0;
    while c_pos.borrow().len() < 300 {
        rounds += 1;
        assert!(rounds <= 12, "rotation failed to cover all entities");
        server.loop_once(DT);
        // Everything moves every tick — worst case for delta models.
        for (_, mut p) in s_pos.borrow_mut().iter_mut() {
            p.y += 1.0;
        }
        let snap = snet.snapshot_budgeted(&server, 1, BUDGET).unwrap();
        assert!(snap.len() <= BUDGET, "snapshot {} > budget", snap.len());
        let applied = cnet.apply(&mut client, &snap).unwrap();
        snet.ack(1, applied.tick);
    }
    // ceil(300/40) = 8 snapshots to cover everyone.
    assert!(rounds >= 8, "budget should force rotation, got {rounds}");

    // And the same mechanism converges to silence when changes stop.
    for _ in 0..70 {
        server.loop_once(DT);
        let snap = snet.snapshot_budgeted(&server, 1, BUDGET).unwrap();
        let applied = cnet.apply(&mut client, &snap).unwrap();
        snet.ack(1, applied.tick);
    }
    server.loop_once(DT);
    let quiet = snet.snapshot_budgeted(&server, 1, BUDGET).unwrap();
    // Header (8) + removal count (4) + section count (2) + one section
    // header (6) and zero entries.
    assert_eq!(quiet.len(), 20, "fully-confirmed world should pack empty");
}
