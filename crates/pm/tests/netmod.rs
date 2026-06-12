//! The net modules end-to-end over real UDP loopback: server and client
//! games built ONLY from the published "net.*" singles — no direct
//! transport access — must see joins, flow commands (with the applied-
//! seq echo), replicate pools, and exchange reliable events both ways.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use pm::{Id, NetClient, NetServer, Pm, QuicClient, QuicServer};

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
    event: bool,
    echo: u32,
}

#[test]
fn net_modules_loopback() {
    // --- server world: game logic reads/writes only "net.*" singles ---
    let mut spm = Pm::new();
    let s_pos = spm.pool::<Pos>("pos");
    let mut snet = NetServer::new(&mut spm);
    snet.pool_sync("pos", &s_pos);
    let squic = QuicServer::bind("127.0.0.1:0", &snet.schema()).expect("bind");
    let addr = squic.local_addr().unwrap().to_string();
    snet.serve::<Cmd>(&mut spm, squic);

    let garage = spm.single::<Garage>("garage");
    let sseen = spm.single::<ServerSeen>("seen");
    {
        let peers = spm.single::<pm::PeerEvents>("net.peers");
        let cmds = spm.single::<pm::Commands<Cmd>>("net.cmds");
        let sevents = spm.single::<pm::ServerEvents>("net.events");
        let sout = spm.single::<pm::ServerOutbox>("net.out");
        let garage = garage.clone();
        let sseen = sseen.clone();
        let s_pos = s_pos.clone();
        spm.task_add("game", 30.0, 0.0, move |pm| {
            for &p in &peers.borrow().joined {
                let id = pm.id_add();
                s_pos.borrow_mut().add(id, Pos::default());
                garage.borrow_mut().0.insert(p, id);
                sout.borrow_mut().send(p, 16, b"welcome");
                sseen.borrow_mut().joined = true;
            }
            for &p in &peers.borrow().left {
                if let Some(id) = garage.borrow_mut().0.remove(&p) {
                    pm.id_remove(id);
                }
            }
            let mut cs = cmds.borrow_mut();
            let mut pool = s_pos.borrow_mut();
            for (&p, &id) in &garage.borrow().0 {
                let c = cs.pop(p);
                if let Some(mut e) = pool.get_mut(id) {
                    let next = Pos { x: e.x + c.dx, ..*e };
                    *e = next;
                }
            }
            for (_, ty, payload) in &sevents.borrow().0 {
                if *ty == 17 && payload == b"hi" {
                    sseen.borrow_mut().event = true;
                }
            }
        });
    }

    // --- client world ---
    let mut cpm = Pm::new();
    let c_pos = cpm.pool::<Pos>("pos");
    let mut cnet = NetClient::new();
    cnet.pool_sync("pos", &c_pos);
    let cquic = QuicClient::connect(&addr, &cnet.schema()).expect("connect");
    cnet.connect::<Cmd>(&mut cpm, cquic, 60.0);

    // Queue a reliable event BEFORE the handshake exists — the module
    // holds it until connected.
    cpm.single::<pm::Outbox>("net.out").borrow_mut().send(17, b"hi");
    cpm.single::<pm::NetInput<Cmd>>("net.input").borrow_mut().0 = Cmd { dx: 1.0 };

    let cseen = cpm.single::<ClientSeen>("seen");
    {
        let events = cpm.single::<pm::ClientEvents>("net.events");
        let applied = cpm.single::<pm::AppliedLog>("net.applied");
        let cseen = cseen.clone();
        cpm.task_add("game", 30.0, 0.0, move |_pm| {
            for (ty, payload) in &events.borrow().0 {
                if *ty == 16 && payload == b"welcome" {
                    cseen.borrow_mut().event = true;
                }
            }
            let mut s = cseen.borrow_mut();
            for a in &applied.borrow().0 {
                s.echo = s.echo.max(a.input_seq);
            }
        });
    }

    // --- drive both worlds until everything has been observed ---
    let status = cpm.single::<pm::NetStatus>("net.status");
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut done = false;
    while Instant::now() < deadline {
        spm.loop_once(DT);
        cpm.loop_once(DT);
        std::thread::sleep(Duration::from_millis(1));

        let replicated = c_pos.borrow().values().iter().any(|p| p.x > 5.0);
        let s = sseen.borrow();
        let c = cseen.borrow();
        if status.borrow().connected && s.joined && s.event && c.event && c.echo > 0 && replicated
        {
            done = true;
            break;
        }
    }

    assert!(sseen.borrow().joined, "server never observed the join");
    assert!(status.borrow().connected, "client never connected");
    assert!(sseen.borrow().event, "client->server reliable event lost");
    assert!(cseen.borrow().event, "server->client reliable event lost");
    assert!(cseen.borrow().echo > 0, "applied input-seq echo never arrived");
    assert!(done, "replication never converged (cmd-driven pos.x > 5)");
    assert!(spm.task_faults().is_empty(), "server task faults: {:?}", spm.task_faults());
    assert!(cpm.task_faults().is_empty(), "client task faults: {:?}", cpm.task_faults());
}
