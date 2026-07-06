//! The net modules end-to-end over real UDP loopback: server and client
//! games built from typed channel handles — no transport access in the
//! game tasks — must see joins, flow input (with the applied-seq echo),
//! replicate pools, and deliver a reliable client→server event. (Events
//! are one-way client→server; there is no server→client channel.) Lives
//! in-crate because the manual bind/connect split (port 0, two kernels in
//! one thread) needs the non-public seams.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::net::{NetClient, NetServer, Outbox};
use crate::netmod::{PeerEvents, PeerStats, ServerEvents, ServerOwn, input_rx};
use crate::transport::{QuicClient, QuicServer};
use crate::{Id, Pm};

const DT: f32 = 1.0 / 60.0;

#[derive(Clone, Copy, PartialEq, Debug, Default, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
struct Pos {
    x: f32,
    y: f32,
}

#[derive(Clone, Copy, PartialEq, Debug, Default, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
struct Cmd {
    dx: f32,
}

#[derive(Default)]
struct Garage(HashMap<u8, Id>);

#[derive(Default)]
struct ServerSeen {
    joined: bool,
    event: bool,
}

#[derive(Default)]
struct ClientSeen {
    echo: u32,
}

#[test]
fn net_modules_loopback() {
    // --- server world: game logic holds only channel handles ---
    let mut spm = Pm::new();
    let s_pos = spm.pool::<Pos>("pos");
    let mut snet = NetServer::new(&mut spm);
    snet.pool_sync("pos", &s_pos);
    let squic = QuicServer::bind("127.0.0.1:0", &snet.schema()).expect("bind");
    let addr = squic.local_addr().unwrap().to_string();
    // Register the input channel before serve() — the net task captures
    // the erased sink at install time.
    let cmds = input_rx::<Cmd>(&mut spm, "cmd");
    snet.serve(&mut spm, squic);

    let garage = spm.single::<Garage>("garage");
    let sseen = spm.single::<ServerSeen>("seen");
    {
        let peers = spm.single::<PeerEvents>("net.peers");
        let sevents = spm.single::<ServerEvents>("net.events");
        let garage = garage.clone();
        let sseen = sseen.clone();
        let s_pos = s_pos.clone();
        let cmds = cmds.clone();
        spm.task_add("game", 30.0, 0.0, move |pm| {
            for &p in &peers.get().joined {
                let id = pm.id_add();
                s_pos.get_mut().add(id, Pos::default());
                garage.get_mut().0.insert(p, id);
                sseen.get_mut().joined = true;
            }
            for &p in &peers.get().left {
                if let Some(id) = garage.get_mut().0.remove(&p) {
                    pm.id_remove(id);
                }
            }
            let mut pool = s_pos.get_mut();
            for (&p, &id) in &garage.get().0 {
                let c = cmds.pop(p);
                if let Some(mut e) = pool.get_mut(id) {
                    let next = Pos {
                        x: e.x + c.dx,
                        ..*e
                    };
                    *e = next;
                }
            }
            for (_, ty, payload) in &sevents.get().0 {
                if *ty == 17 && payload == b"hi" {
                    sseen.get_mut().event = true;
                }
            }
        });
    }

    // --- client world: built via the role wrapper (the only public
    // construction path); the transport is still driven manually below,
    // through the Deref to the kernel.
    let mut cpm = Pm::client("127.0.0.1:0", 60.0);
    let cnet_status = cpm.net();
    let input = cpm.input::<Cmd>("cmd");
    let c_pos = cpm.pool::<Pos>("pos");
    let mut cnet = NetClient::new();
    cnet.pool_sync("pos", &c_pos);
    let cquic = QuicClient::connect(&addr, &cnet.schema()).expect("connect");
    cnet.connect(&mut cpm, cquic, 60.0);

    // Queue a reliable client→server event BEFORE the handshake exists —
    // the module holds it until connected.
    cpm.single::<Outbox>("net.out").get_mut().send(17, b"hi");
    input.set(Cmd { dx: 1.0 });

    let cseen = cpm.single::<ClientSeen>("seen");
    {
        let net = cnet_status.clone();
        let cseen = cseen.clone();
        cpm.task_add("game", 30.0, 0.0, move |_pm| {
            let mut s = cseen.get_mut();
            for a in net.applied() {
                s.echo = s.echo.max(a.input_seq);
            }
        });
    }

    // --- drive both worlds until everything has been observed ---
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut done = false;
    while Instant::now() < deadline {
        spm.loop_once(DT);
        cpm.loop_once(DT);
        std::thread::sleep(Duration::from_millis(1));

        let replicated = c_pos.get().values().iter().any(|p| p.x > 5.0);
        let connected = cnet_status.connected();
        let s = sseen.get();
        let c = cseen.get();
        if connected && s.joined && s.event && c.echo > 0 && replicated {
            done = true;
            break;
        }
    }

    assert!(sseen.get().joined, "server never observed the join");
    assert!(cnet_status.connected(), "client never connected");
    assert!(sseen.get().event, "client->server reliable event lost");
    assert!(
        cseen.get().echo > 0,
        "applied input-seq echo never arrived"
    );
    assert!(done, "replication never converged (cmd-driven pos.x > 5)");
    // Per-peer link stats rode along: the client acked snapshots, so its
    // acked_tick advanced (the lag-comp rewind anchor), and quinn has an
    // RTT estimate from the moment the connection exists.
    {
        let p = *garage.get().0.keys().next().expect("a joined peer");
        let stats = spm.single::<PeerStats>("net.peerstat");
        let st = stats.get();
        let ps = st.0.get(&p).expect("peerstat row for the joined peer");
        assert!(ps.acked_tick > 0, "acks flowed but acked_tick never advanced");
        assert!(ps.rtt_ms > 0.0, "rtt_ms never measured");
    }
    assert!(
        spm.task_faults().is_empty(),
        "server task faults: {:?}",
        spm.task_faults()
    );
    assert!(
        cpm.task_faults().is_empty(),
        "client task faults: {:?}",
        cpm.task_faults()
    );
}

/// `ttl_pool`: a synced entry not written for the lifetime is removed —
/// entity and all — with no game-side timer. Runs on an unbound PmServer:
/// the duration modifiers are tasks, not transport.
#[test]
fn ttl_pool_expires_stale_entries() {
    let mut pm = Pm::server("127.0.0.1:0");
    pm.loop_rate = 60;
    let pool = pm.sync_pool::<Pos>("pos");
    pm.ttl_pool(&pool, 0.1); // 6 ticks at 60 Hz

    let a = pm.id_add();
    pool.get_mut().add(a, Pos { x: 1.0, y: 0.0 });
    for _ in 0..3 {
        pm.loop_once(DT);
    }
    assert!(pool.get().contains(a), "still inside the lifetime");
    for _ in 0..10 {
        pm.loop_once(DT);
    }
    assert!(!pool.get().contains(a), "expired after the ttl");
    assert!(!pm.id_alive(a), "expiry removes the entity, not just the entry");
}

/// `history_pool` frames carry the snapshot label convention (`tick - 1`):
/// the frame labeled T holds the state the tick-T snapshot packed. The
/// window clamps — a rewind past its edges gets the nearest kept frame.
#[test]
fn history_pool_rewinds_past_ticks() {
    let mut pm = Pm::server("127.0.0.1:0");
    pm.loop_rate = 60;
    let pool = pm.sync_pool::<Pos>("pos");
    let hist = pm.history_pool(&pool, 0.5); // 30 frames

    let id = pm.id_add();
    pool.get_mut().add(id, Pos::default());
    for i in 0..40 {
        if let Some(mut e) = pool.get_mut().get_mut(id) {
            e.x = i as f32;
        }
        pm.loop_once(DT);
    }
    // The kernel is born at tick 1, so iteration i runs as tick i+2 and
    // the frame it records is labeled i+1 holding x == i: frame L reads
    // L-1 — exactly the state a snapshot labeled L would have packed
    // (both copy the pool as the tick begins).
    let probe = |label: u32| hist.frame(label).expect("frame")[0].1.x;
    assert_eq!(probe(35), 34.0, "exact rewind");
    assert_eq!(probe(12), 11.0, "exact rewind near the window edge");
    assert_eq!(probe(0), 10.0, "older than the window clamps to oldest");
    assert_eq!(probe(1000), 39.0, "future clamps to newest");
}

/// Ownership auto-clears ONE tick after the leave is reported: the game's
/// leave handler (running above NET_PRIO in the same tick) must still see
/// `own(p)` to despawn by, and by the next tick the entry must be gone —
/// peer ids recycle, so a stale entry would hand the next player on this
/// id the departed player's entity.
#[test]
fn ownership_auto_clears_after_leave_tick() {
    #[derive(Default)]
    struct Seen {
        peer: Option<u8>,
        id: Option<Id>,
        own_at_leave: Option<Option<Id>>,
        cleared_next_tick: bool,
    }

    let mut spm = Pm::new();
    let s_pos = spm.pool::<Pos>("pos");
    let mut snet = NetServer::new(&mut spm);
    snet.pool_sync("pos", &s_pos);
    let squic = QuicServer::bind("127.0.0.1:0", &snet.schema()).expect("bind");
    let addr = squic.local_addr().unwrap().to_string();
    snet.serve(&mut spm, squic);

    let seen = spm.single::<Seen>("seen");
    {
        let peers = spm.single::<PeerEvents>("net.peers");
        let own = spm.single::<ServerOwn>("net.own");
        let s_pos = s_pos.clone();
        let seen = seen.clone();
        spm.task_add("game", 30.0, 0.0, move |pm| {
            let mut s = seen.get_mut();
            for &p in &peers.get().joined {
                let id = pm.id_add();
                s_pos.get_mut().add(id, Pos::default());
                own.get_mut().set(p, id);
                s.peer = Some(p);
                s.id = Some(id);
            }
            for &p in &peers.get().left {
                // The leave tick: capture what the game can still see.
                s.own_at_leave = Some(own.get().get(p));
            }
            // Any tick after the leave was reported: the entry must be gone.
            if let (Some(p), Some(_)) = (s.peer, s.own_at_leave)
                && peers.get().left.is_empty()
                && own.get().get(p).is_none()
            {
                s.cleared_next_tick = true;
            }
        });
    }

    let mut cnet = NetClient::new();
    let mut cpm = Pm::new();
    let c_pos = cpm.pool::<Pos>("pos");
    cnet.pool_sync("pos", &c_pos);
    let mut cquic = Some(QuicClient::connect(&addr, &cnet.schema()).expect("connect"));

    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline && !seen.get().cleared_next_tick {
        spm.loop_once(DT);
        if seen.get().peer.is_some() {
            cquic = None; // client "dies" silently: reaped by idle timeout
        }
        if let Some(c) = cquic.as_mut() {
            c.pump();
        }
        std::thread::sleep(Duration::from_millis(2));
    }

    let s = seen.get();
    assert!(s.peer.is_some(), "client never joined");
    assert_eq!(
        s.own_at_leave,
        Some(s.id),
        "leave tick must still see the owned entity (games despawn via own(p))"
    );
    assert!(
        s.cleared_next_tick,
        "ownership entry must be gone after the leave tick (peer ids recycle)"
    );
    assert!(
        spm.task_faults().is_empty(),
        "server task faults: {:?}",
        spm.task_faults()
    );
}
