//! The net modules end-to-end over real UDP loopback: server and client
//! games built ONLY from the published "net.*" singles — no direct
//! transport access — must see joins, flow commands (with the applied-
//! seq echo), replicate pools, and deliver a reliable client→server event.
//! (Events are one-way client→server; there is no server→client channel.)

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
        let garage = garage.clone();
        let sseen = sseen.clone();
        let s_pos = s_pos.clone();
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
            let mut cs = cmds.get_mut();
            let mut pool = s_pos.get_mut();
            for (&p, &id) in &garage.get().0 {
                let c = cs.pop(p);
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
    let cnet_status = cpm.net::<Cmd>();
    let c_pos = cpm.pool::<Pos>("pos");
    let mut cnet = NetClient::new();
    cnet.pool_sync("pos", &c_pos);
    let cquic = QuicClient::connect(&addr, &cnet.schema()).expect("connect");
    cnet.connect::<Cmd>(&mut cpm, cquic, 60.0);

    // Queue a reliable client→server event BEFORE the handshake exists —
    // the module holds it until connected.
    cpm.single::<pm::Outbox>("net.out")
        .get_mut()
        .send(17, b"hi");
    cnet_status.input(Cmd { dx: 1.0 });

    let cseen = cpm.single::<ClientSeen>("seen");
    {
        let applied = cpm.single::<pm::AppliedLog>("net.applied");
        let cseen = cseen.clone();
        cpm.task_add("game", 30.0, 0.0, move |_pm| {
            let mut s = cseen.get_mut();
            for a in &applied.get().0 {
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
