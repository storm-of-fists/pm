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
        for (p, t, sq) in squic.acks_drain() {
            snet.ack(p, t, sq);
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
            cquic.ack_send(applied.tick, applied.seq);
            input_echo = applied.input_seq;
        }
        cpm.loop_once(DT);

        // once converged, remove an entity server-side (exactly once)
        let synced =
            c_pos.get().len() == 20 && c_pos.get().get(ids[7]) == Some(&Pos { x: 7.0, y: 14.0 });
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
        for (p, t, sq) in squic.acks_drain() {
            snet.ack(p, t, sq);
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
                cquic.ack_send(applied.tick, applied.seq);
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

/// The password door (FRAME_AUTH): a right password is admitted and
/// replicates; a wrong one is closed with the reason on the client; a
/// silent client (no password set) against a locked server is bounced
/// too. Admission gating means the bounced clients never appear in
/// joined_drain — the roster never learns they existed.
#[test]
fn password_gates_admission() {
    let mut spm = Pm::new();
    let s_pos = spm.pool::<Pos>("pos");
    let mut snet = NetServer::new(&mut spm);
    snet.pool_sync("pos", &s_pos);
    let mut squic = QuicServer::bind("127.0.0.1:0", &snet.schema()).expect("bind");
    squic.password_set("hogwild");
    let addr = squic.local_addr().unwrap().to_string();
    let id = spm.id_add();
    s_pos.get_mut().add(id, Pos { x: 5.0, y: 7.0 });

    let client = |pw: Option<&str>| {
        let mut cpm = Pm::new();
        let c_pos = cpm.pool::<Pos>("pos");
        let mut n = NetClient::new();
        n.pool_sync("pos", &c_pos);
        let mut q = QuicClient::connect(&addr, &n.schema()).expect("connect");
        if let Some(pw) = pw {
            q.password_set(pw);
        }
        (cpm, c_pos, n, q)
    };
    let (mut good_pm, good_pos, good_net, mut good) = client(Some("hogwild"));
    let (_bad_pm, bad_pos, _bad_net, mut bad) = client(Some("letmein"));
    let (_mute_pm, mute_pos, _mute_net, mut mute) = client(None);

    let deadline = Instant::now() + Duration::from_secs(10);
    let mut admitted = Vec::new();
    let (mut good_synced, mut bad_closed) = (false, false);
    while Instant::now() < deadline {
        squic.pump();
        admitted.extend(squic.joined_drain());
        for p in &admitted {
            if let Some(snap) = snet.snapshot(&spm, *p) {
                squic.snapshot_send(*p, &snap);
            }
        }
        for p in squic.left_drain() {
            snet.peer_remove(p);
        }
        for p in admitted.iter().copied().collect::<Vec<_>>() {
            snet.peer_add(p); // idempotent enough for the test loop
        }
        spm.loop_once(DT);

        good.pump();
        for snap in good.snapshots_drain() {
            let applied = good_net.apply(&mut good_pm, &snap).expect("apply");
            good.ack_send(applied.tick, applied.seq);
        }
        bad.pump();
        mute.pump();

        good_synced = good_pos.get().get(id) == Some(&Pos { x: 5.0, y: 7.0 });
        bad_closed = bad.error().is_some() || bad.is_gone();
        if good_synced && bad_closed {
            break;
        }
    }
    assert!(good_synced, "right password must replicate");
    assert!(bad_closed, "wrong password must be disconnected");
    let reason = bad.error().unwrap_or_default().to_string();
    assert!(
        reason.contains("bad password"),
        "close reason should say why: {reason:?}"
    );
    // The wrong/silent clients were never admitted: exactly one join.
    assert_eq!(admitted.len(), 1, "only the right password joins");
    assert!(bad_pos.get().is_empty() && mute_pos.get().is_empty(), "no state leaked pre-admission");
}

/// The reconnect handshake (pm/3): a session token parked at disconnect
/// reclaims its peer id inside the grace window, and the fresh
/// connection reconverges from zero — delta cursors ARE the baseline
/// mechanism, so a rejoiner needs no keyframe machinery.
#[test]
fn reconnect_token_reclaims_peer_id() {
    let mut spm = Pm::new();
    let s_pos = spm.pool::<Pos>("pos");
    let mut snet = NetServer::new(&mut spm);
    snet.pool_sync("pos", &s_pos);
    let mut squic = QuicServer::bind("127.0.0.1:0", &snet.schema()).expect("bind");
    let addr = squic.local_addr().unwrap().to_string();
    for i in 0..10 {
        let id = spm.id_add();
        s_pos.get_mut().add(id, Pos { x: i as f32, y: 0.0 });
    }

    let token = [7u8; 16];
    let schema = {
        let mut n = NetClient::new();
        n.pool_sync("pos", &Pm::new().pool::<Pos>("pos"));
        n.schema()
    };
    let dial = |addr: &str, token: [u8; 16]| {
        let mut c = QuicClient::connect(addr, &schema).expect("connect");
        c.session_token_set(token);
        c
    };

    let mut cquic = Some(dial(&addr, token));
    let mut first_peer = None;
    let mut left_peer = None;
    let mut second_peer = None;
    let mut phase = 0; // 0 join → 1 die+reap → 2 rejoin
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        squic.pump();
        for p in squic.joined_drain() {
            snet.peer_add(p);
            if phase == 0 {
                first_peer = Some(p);
                cquic = None; // process dies mid-game: silence, no close
                phase = 1;
            } else {
                second_peer = Some(p);
            }
        }
        for p in squic.left_drain() {
            snet.peer_remove(p);
            if phase == 1 {
                left_peer = Some(p);
                // Rejoin INSIDE the grace window with the same token.
                cquic = Some(dial(&addr, token));
                phase = 2;
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
        if let Some(c) = cquic.as_mut() {
            c.pump();
        }
        if second_peer.is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let first = first_peer.expect("first connect never joined");
    assert_eq!(left_peer, Some(first), "the drop must reap the first session");
    assert_eq!(
        second_peer,
        Some(first),
        "the same token inside grace must reclaim the same peer id"
    );

    // And the reclaimed session converges to the world from nothing.
    let mut cpm = Pm::new();
    let c_pos = cpm.pool::<Pos>("pos");
    let mut cnet = NetClient::new();
    cnet.pool_sync("pos", &c_pos);
    let mut cquic = cquic.unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && c_pos.get().len() < 10 {
        squic.pump();
        for p in squic.joined_drain() {
            snet.peer_add(p);
        }
        for (p, t, sq) in squic.acks_drain() {
            snet.ack(p, t, sq);
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
            let applied = cnet.apply(&mut cpm, &snap).expect("apply");
            cquic.ack_send(applied.tick, applied.seq);
        }
        cpm.loop_once(DT);
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(c_pos.get().len(), 10, "reclaimed session must reconverge");
}

/// A redial while the old connection is still live (crash + fast
/// restart, route flap) SUPERSEDES it quietly: the new connection
/// inherits the peer id, the old one closes, and no `left` ever fires —
/// so the game never parks the avatar, and a different token still gets
/// a fresh id.
#[test]
fn redial_supersedes_live_connection_quietly() {
    let mut spm = Pm::new();
    let s_pos = spm.pool::<Pos>("pos");
    let mut snet = NetServer::new(&mut spm);
    snet.pool_sync("pos", &s_pos);
    let mut squic = QuicServer::bind("127.0.0.1:0", &snet.schema()).expect("bind");
    let addr = squic.local_addr().unwrap().to_string();

    let schema = {
        let mut n = NetClient::new();
        n.pool_sync("pos", &Pm::new().pool::<Pos>("pos"));
        n.schema()
    };
    let dial = |addr: &str, token: [u8; 16]| {
        let mut c = QuicClient::connect(addr, &schema).expect("connect");
        c.session_token_set(token);
        c
    };

    let token = [9u8; 16];
    let mut a = dial(&addr, token);
    let mut b = None;
    let mut c = None;
    let mut joins = Vec::new();
    let mut lefts = Vec::new();
    let mut b_peer = None;
    let mut c_peer = None;
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        squic.pump();
        for p in squic.joined_drain() {
            snet.peer_remove(p);
            snet.peer_add(p);
            joins.push(p);
            if joins.len() == 1 {
                b = Some(dial(&addr, token)); // same token, A still live
            }
        }
        for p in squic.left_drain() {
            snet.peer_remove(p);
            lefts.push(p);
        }
        a.pump();
        if let Some(bq) = b.as_mut() {
            bq.pump();
            if b_peer.is_none() {
                b_peer = bq.handshake_done();
                if b_peer.is_some() {
                    c = Some(dial(&addr, [11u8; 16])); // different token
                }
            }
        }
        if let Some(cq) = c.as_mut() {
            cq.pump();
            c_peer = cq.handshake_done();
            if c_peer.is_some() {
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    let first = *joins.first().expect("A never joined");
    assert_eq!(b_peer, Some(first), "same live token must inherit the peer id");
    assert!(
        a.is_gone(),
        "the superseded connection must be closed by the server"
    );
    assert!(
        !lefts.contains(&first),
        "supersede is quiet: no left for the inherited id, ownership stands"
    );
    assert_eq!(
        joins.iter().filter(|&&p| p == first).count(),
        2,
        "the reclaim announces itself as a fresh join"
    );
    let cp = c_peer.expect("fresh-token client never admitted");
    assert_ne!(cp, first, "a different token draws a fresh id");
}

/// The tick journal: one ring per pool no matter how many consumers,
/// the second registration widening the shared window (v2 item 2).
#[test]
fn journal_pool_shares_one_ring_per_pool() {
    let mut pm = Pm::server("127.0.0.1:0");
    let pool = pm.sync_pool::<Pos>("pos");
    let j1 = pm.journal_pool(&pool, 0.05); // ~3 ticks
    let j2 = pm.journal_pool(&pool, 0.2); // same ring, wider window
    let id = pm.id_add();
    pool.get_mut().add(id, Pos { x: 0.0, y: 0.0 });
    for i in 0..12 {
        if let Some(mut e) = pool.get_mut().get_mut(id) {
            e.x = i as f32;
        }
        pm.loop_once(DT);
    }
    assert_eq!(j1.newest(), j2.newest(), "handles share one ring");
    let newest = j1.newest().expect("frames recorded");
    assert_eq!(j1.frame(newest).unwrap().len(), 1);
    assert!(
        newest - j1.oldest().unwrap() >= 8,
        "second registration must widen the shared window: {:?}..{:?}",
        j1.oldest(),
        j1.newest()
    );
}

/// Interest (v2 item 4): a scored pool fills each peer's budget
/// nearest-first FOR THAT PEER — the same tick, the same dirty set,
/// two different snapshot orders. No sockets needed: this is packer
/// behavior.
#[test]
fn interest_packs_nearest_first_per_peer() {
    use std::rc::Rc;

    let mut pm = Pm::new();
    let pos = pm.pool::<Pos>("pos");
    let mut net = NetServer::new(&mut pm);
    net.pool_sync("pos", &pos);
    // Peer 1 lives at x=0, peer 2 at x=190 (a stand-in for "their
    // avatar's position" — hogs reads it off the vehicle pools).
    net.interest::<Pos>(
        "pos",
        Rc::new(|peer, _id, p| {
            let home = if peer == 1 { 0.0 } else { 190.0 };
            1.0 / (1.0 + (p.x - home).abs())
        }),
    );
    let ids: Vec<_> = (0..20)
        .map(|i| {
            let id = pm.id_add();
            pos.get_mut().add(id, Pos { x: i as f32 * 10.0, y: 0.0 });
            id
        })
        .collect();
    net.peer_add(1);
    net.peer_add(2);
    pm.loop_once(DT);

    // Budget for exactly 5 entries: 19B header + 6B section + 5×12B.
    let first_id = |snap: &[u8]| {
        let entries = u32::from_le_bytes(snap[21..25].try_into().unwrap());
        assert!(entries >= 1 && entries <= 5, "budget should cap entries, got {entries}");
        u32::from_le_bytes(snap[25..29].try_into().unwrap())
    };
    let s1 = net.snapshot_budgeted(&pm, 1, 85).expect("peer 1 snapshot");
    let s2 = net.snapshot_budgeted(&pm, 2, 85).expect("peer 2 snapshot");
    assert_eq!(
        first_id(&s1.bytes),
        ids[0].0,
        "peer 1's first entry is the entity at ITS doorstep"
    );
    assert_eq!(
        first_id(&s2.bytes),
        ids[19].0,
        "peer 2's first entry is the entity at the OTHER end"
    );
    assert!(s1.more && s2.more, "the rest is backlog, not dropped");
}
