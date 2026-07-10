//! End-to-end over real UDP on localhost: QUIC handshake, schema check,
//! snapshot replication with acks, removal + recycling, and events on the
//! reliable stream.

//! The transport is deliberately not public, so these live in-crate.

use std::time::{Duration, Instant};

use crate::Pm;
use crate::net::{NetClient, NetServer};
use crate::transport::{QuicClient, QuicServer};

const DT: f32 = 1.0 / 60.0;

#[derive(Clone, Copy, PartialEq, Debug, Default, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
struct Pos {
    x: f32,
    y: f32,
}

#[test]
fn quic_loopback_full_stack() {
    // --- server world ---
    let mut spm = Pm::new();
    let s_pos = spm.pool::<Pos>("pos");
    let mut snet = NetServer::new(&mut spm);
    snet.pool_sync("pos", &s_pos);
    let mut squic = QuicServer::bind("127.0.0.1:0", &snet.schema()).expect("bind");
    let addr = squic.local_addr().unwrap().to_string();

    let ids: Vec<_> = (0..20)
        .map(|i| {
            let id = spm.id_add();
            s_pos.get_mut().add(
                id,
                Pos {
                    x: i as f32,
                    y: 2.0 * i as f32,
                },
            );
            id
        })
        .collect();

    // --- client world ---
    let mut cpm = Pm::new();
    let c_pos = cpm.pool::<Pos>("pos");
    let cnet = {
        let mut n = NetClient::new();
        n.pool_sync("pos", &c_pos);
        n
    };
    let mut cquic = QuicClient::connect(&addr, &cnet.schema()).expect("connect");

    let mut event_sent = false;
    let mut event_received = false;
    let mut removed = false;
    let mut input_sent = 0u32;
    let mut input_echo = 0u32;
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut converged_then_removed = false;

    while Instant::now() < deadline {
        // server side
        squic.pump();
        for p in squic.joined_drain() {
            snet.peer_add(p);
        }
        for p in squic.left_drain() {
            snet.peer_remove(p);
        }
        for (p, t) in squic.acks_drain() {
            snet.ack(p, t);
        }
        for (p, seq, payload) in squic.inputs_drain() {
            assert_eq!(payload, b"drv", "input payload corrupted");
            snet.input_processed(p, seq);
        }
        // Events are one-way client→server; the server just receives.
        for (_, ty, payload) in squic.events_drain() {
            if ty == 17 && payload == b"hello" {
                event_received = true;
            }
        }
        spm.loop_once(DT);
        let peers: Vec<u8> = snet.peers().collect();
        for p in peers {
            if let Some(snap) = snet.snapshot(&spm, p) {
                squic.snapshot_send(p, &snap);
            }
        }
        snet.prune(&mut spm);

        // client side
        cquic.pump();
        assert!(cquic.error().is_none(), "client error: {:?}", cquic.error());
        if let Some(peer) = cquic.handshake_done() {
            cpm.local_peer = peer;
        }
        if cquic.handshake_done().is_some() {
            if !event_sent {
                cquic.event_send(17, b"hello");
                event_sent = true;
            }
            input_sent = cquic.input_send(b"drv");
        }
        for snap in cquic.snapshots_drain() {
            let applied = cnet.apply(&mut cpm, &snap).expect("apply");
            cquic.ack_send(applied.tick);
            input_echo = applied.input_seq;
        }
        cpm.loop_once(DT);

        // once converged, remove an entity server-side (exactly once)
        let synced = c_pos.get().len() == 20
            && c_pos.get().get(ids[7]) == Some(&Pos { x: 7.0, y: 14.0 });
        if synced && !removed {
            spm.id_remove(ids[3]);
            removed = true;
        }
        if removed
            && !cpm.id_alive(ids[3])
            && c_pos.get().len() == 19
            && event_received
            && input_echo > 0
        {
            converged_then_removed = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(1));
    }

    assert!(
        converged_then_removed,
        "did not converge over QUIC within deadline"
    );
    assert!(
        input_sent > 0 && input_echo <= input_sent,
        "input echo out of range"
    );
    assert!(cpm.local_peer >= 1, "handshake should assign a peer id");
    assert_eq!(squic.oversize_drops, 0);
}

#[test]
fn schema_mismatch_is_rejected() {
    let mut spm = Pm::new();
    let s_pos = spm.pool::<Pos>("pos");
    let mut snet = NetServer::new(&mut spm);
    snet.pool_sync("pos", &s_pos);
    let mut squic = QuicServer::bind("127.0.0.1:0", &snet.schema()).expect("bind");
    let addr = squic.local_addr().unwrap().to_string();

    // Client registers a different schema (extra pool).
    let mut cpm = Pm::new();
    let c_pos = cpm.pool::<Pos>("pos");
    let c_hp = cpm.pool::<u32>("hp");
    let mut cnet = NetClient::new();
    cnet.pool_sync("pos", &c_pos);
    cnet.pool_sync("hp", &c_hp);
    let mut cquic = QuicClient::connect(&addr, &cnet.schema()).expect("connect");

    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        squic.pump();
        for p in squic.joined_drain() {
            snet.peer_add(p);
        }
        cquic.pump();
        if cquic.error().is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(cquic.error(), Some("schema mismatch with server"));
}

#[test]
fn converges_under_lag_and_loss() {
    let mut spm = Pm::new();
    let s_pos = spm.pool::<Pos>("pos");
    let mut snet = NetServer::new(&mut spm);
    snet.pool_sync("pos", &s_pos);
    let mut squic = QuicServer::bind("127.0.0.1:0", &snet.schema()).expect("bind");
    let addr = squic.local_addr().unwrap().to_string();

    let ids: Vec<_> = (0..15)
        .map(|i| {
            let id = spm.id_add();
            s_pos.get_mut().add(
                id,
                Pos {
                    x: i as f32,
                    y: -(i as f32),
                },
            );
            id
        })
        .collect();

    let mut cpm = Pm::new();
    let c_pos = cpm.pool::<Pos>("pos");
    let cnet = {
        let mut n = NetClient::new();
        n.pool_sync("pos", &c_pos);
        n
    };
    let mut cquic = QuicClient::connect(&addr, &cnet.schema()).expect("connect");
    cquic.link_lag_set(Duration::from_millis(15), 0.15);

    let deadline = Instant::now() + Duration::from_secs(15);
    let mut converged = false;
    while Instant::now() < deadline {
        squic.pump();
        for p in squic.joined_drain() {
            snet.peer_add(p);
        }
        for (p, t) in squic.acks_drain() {
            snet.ack(p, t);
        }
        spm.loop_once(DT);
        let peers: Vec<u8> = snet.peers().collect();
        for p in peers {
            if let Some(snap) = snet.snapshot(&spm, p) {
                squic.snapshot_send(p, &snap);
            }
        }
        snet.prune(&mut spm);

        cquic.pump();
        for snap in cquic.snapshots_drain() {
            if let Ok(applied) = cnet.apply(&mut cpm, &snap) {
                cquic.ack_send(applied.tick);
            }
        }
        cpm.loop_once(DT);

        if c_pos.get().len() == 15
            && ids
                .iter()
                .all(|&id| c_pos.get().get(id) == s_pos.get().get(id))
        {
            converged = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(converged, "did not converge under 15ms lag + 15% loss");
    assert!(
        cquic.rtt() >= Duration::from_millis(25),
        "RTT should reflect the simulated lag"
    );
}

#[test]
fn dead_clients_are_reaped_by_idle_timeout() {
    let mut spm = Pm::new();
    let s_pos = spm.pool::<Pos>("pos");
    let mut snet = NetServer::new(&mut spm);
    snet.pool_sync("pos", &s_pos);
    let mut squic = QuicServer::bind("127.0.0.1:0", &snet.schema()).expect("bind");
    let addr = squic.local_addr().unwrap().to_string();

    let mut cpm = Pm::new();
    let c_pos = cpm.pool::<Pos>("pos");
    let mut cnet = NetClient::new();
    cnet.pool_sync("pos", &c_pos);
    let mut cquic = Some(QuicClient::connect(&addr, &cnet.schema()).expect("connect"));

    let mut joined = None;
    let mut reaped = None;
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        squic.pump();
        if let Some(p) = squic.joined_drain().pop() {
            joined = Some(p);
            cquic = None; // client process "dies": no close, just silence
        }
        if let Some(p) = squic.left_drain().pop() {
            reaped = Some(p);
            break;
        }
        if let Some(c) = cquic.as_mut() {
            c.pump();
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(joined.is_some(), "client never connected");
    assert_eq!(reaped, joined, "dead client must be reaped by idle timeout");
}
