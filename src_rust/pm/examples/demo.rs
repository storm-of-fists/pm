//! Networked demo: 8 clients connect to a QUIC server — 7 bots and you.
//! Every peer drives its own server-authoritative vehicle; input flows as
//! sequenced datagrams, state comes back as snapshot deltas.
//!
//!   cargo run --release --example demo            # server + 7 bots + you
//!   cargo run --release --example demo -- server  # dedicated server on :47777
//!   cargo run --release --example demo -- client  # player client only
//!   cargo run --release --example demo -- bot     # one bot client only
//!
//! Controls: w thrust, s reverse, a/d turn, p profiling panel, q quit.
//! Your vehicle is the arrow (pointing where it's headed); bots are digits.
//!
//! Simulate a real link on the player's connection:
//!
//!   PM_LAG_MS=80 PM_LOSS=0.05 cargo run --release --example demo

use std::collections::HashMap;
use std::io::{Read, Write};
use std::time::Duration;

use pm::{Id, NetClient, NetServer, Pm, QuicClient, QuicServer};

const ADDR: &str = "127.0.0.1:47777";
const WORLD: f32 = 12.0; // world is [-WORLD, WORLD] on both axes
const EV_VEHICLE: u16 = 16; // server -> client: "this entity is yours"

/// Replicated vehicle state. Heading is synced so clients can render
/// which way a car points — and later, dead-reckon along it.
#[derive(Clone, Copy, PartialEq, Default, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
struct Car {
    x: f32,
    y: f32,
    heading: f32,
}

/// The input payload, client -> server.
#[derive(Clone, Copy, Default, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
struct Drive {
    thrust: f32, // -1..1
    turn: f32,   // -1..1, positive = counterclockwise
}

fn wrap(v: f32) -> f32 {
    if v > WORLD {
        v - 2.0 * WORLD
    } else if v < -WORLD {
        v + 2.0 * WORLD
    } else {
        v
    }
}

fn env_f32(name: &str) -> f32 {
    std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(0.0)
}

// --- server -------------------------------------------------------------

#[derive(Default)]
struct Cmds(HashMap<u8, Drive>); // latest input per peer
#[derive(Default)]
struct Garage(HashMap<u8, Id>); // peer -> vehicle

fn run_server(quiet: bool) {
    let mut pm = Pm::new();
    let car = pm.pool_get::<Car>("car");
    let speed = pm.pool_get::<f32>("speed"); // server-only
    let cmds = pm.state_get::<Cmds>("cmds");
    let garage = pm.state_get::<Garage>("garage");

    let mut net = NetServer::new(&mut pm);
    net.pool_sync("car", &car);
    let mut quic = QuicServer::bind(ADDR, &net.schema()).expect("bind server");
    if !quiet {
        eprintln!("pm demo server on {ADDR}");
    }

    pm.task_add("net", 5.0, {
        let car = car.clone();
        let speed = speed.clone();
        let cmds = cmds.clone();
        let garage = garage.clone();
        move |pm| {
            quic.pump();
            for p in quic.joined_drain() {
                net.peer_add(p);
                let id = pm.id_add();
                let spread = p as f32 * 0.8;
                car.borrow_mut().add(
                    id,
                    Car { x: 7.0 * spread.cos(), y: 7.0 * spread.sin(), heading: spread + 1.6 },
                );
                speed.borrow_mut().add(id, 0.0);
                garage.borrow_mut().0.insert(p, id);
                quic.event_send(p, EV_VEHICLE, &id.0.to_le_bytes());
                if !quiet {
                    eprintln!("peer {p} joined, vehicle index {}", id.index());
                }
            }
            for p in quic.left_drain() {
                net.peer_remove(p);
                cmds.borrow_mut().0.remove(&p);
                if let Some(id) = garage.borrow_mut().0.remove(&p) {
                    pm.id_remove(id);
                }
                if !quiet {
                    eprintln!("peer {p} left");
                }
            }
            for (p, seq, bytes) in quic.inputs_drain() {
                if bytes.len() == size_of::<Drive>() {
                    cmds.borrow_mut().0.insert(p, bytemuck::pod_read_unaligned(&bytes));
                    net.input_processed(p, seq);
                }
            }
            for (p, tick) in quic.acks_drain() {
                net.ack(p, tick);
            }
            let peers: Vec<u8> = net.peers().collect();
            for p in peers {
                if let Some(snap) = net.snapshot(pm, p) {
                    quic.snapshot_send(p, &snap);
                }
            }
            net.prune(pm);
        }
    });

    pm.task_add("drive", 30.0, {
        let car = car.clone();
        let speed = speed.clone();
        let cmds = cmds.clone();
        let garage = garage.clone();
        move |pm| {
            let dt = pm.loop_dt();
            let cmds = cmds.borrow();
            let mut car = car.borrow_mut();
            let mut speed = speed.borrow_mut();
            for (&peer, &id) in &garage.borrow().0 {
                let cmd = cmds.0.get(&peer).copied().unwrap_or_default();
                let (Some(mut c), Some(mut s)) = (car.get_mut(id), speed.get_mut(id)) else {
                    continue;
                };
                *s = ((*s + cmd.thrust * 9.0 * dt) * (1.0 - 1.5 * dt)).clamp(-3.0, 7.0);
                if cmd.turn != 0.0 {
                    c.heading += cmd.turn * 2.8 * dt;
                }
                if *s != 0.0 {
                    let (sin, cos) = c.heading.sin_cos();
                    c.x = wrap(c.x + cos * *s * dt);
                    c.y = wrap(c.y + sin * *s * dt);
                }
            }
        }
    });

    // Dedicated-server profiling: task table every 5 seconds.
    if !quiet {
        pm.task_add_every("prof", 90.0, 5.0, {
            let mut prev: HashMap<String, pm::TaskStat> = HashMap::new();
            move |pm| {
                eprintln!("-- task stats (last 5s) --");
                for (name, s) in pm.task_stats() {
                    let p = prev.get(&name).cloned().unwrap_or_default();
                    let calls = s.calls - p.calls;
                    if calls > 0 {
                        let avg_us = (s.ns_total - p.ns_total) as f32 / calls as f32 / 1000.0;
                        eprintln!(
                            "  {name:<8} {avg_us:>8.1} us/call  {calls:>5} calls  max {:>8.1} us",
                            s.ns_max as f32 / 1000.0
                        );
                    }
                    prev.insert(name, s);
                }
            }
        });
    }

    pm.loop_rate = 60;
    pm.loop_run();
}

// --- bot client -----------------------------------------------------------

fn run_bot(phase: f32) {
    let mut pm = Pm::new();
    let car = pm.pool_get::<Car>("car");
    let mut net = NetClient::new();
    net.pool_sync("car", &car);
    let Ok(mut quic) = QuicClient::connect(ADDR, &net.schema()) else { return };

    pm.task_add("net", 5.0, move |pm| {
        quic.pump();
        if quic.is_gone() {
            pm.loop_quit();
            return;
        }
        if let Some(peer) = quic.handshake_done() {
            pm.local_peer = peer;
            let t = pm.tick() as f32 / 60.0;
            let cmd = Drive {
                thrust: 0.7 + 0.3 * (t * 0.6 + phase).sin(),
                turn: (t * 0.8 + phase * 2.0).sin(),
            };
            quic.input_send(bytemuck::bytes_of(&cmd));
        }
        for snap in quic.snapshots_drain() {
            if let Ok(applied) = net.apply(pm, &snap) {
                quic.ack_send(applied.tick);
            }
        }
    });

    pm.loop_rate = 60;
    pm.loop_run();
}

// --- player client ----------------------------------------------------------

const COLS: i32 = 56;
const ROWS: i32 = 24;

/// Raw terminal for the lifetime of the player client; restores on drop.
struct RawTerm;

impl RawTerm {
    fn enable() -> Self {
        let _ = std::process::Command::new("stty")
            .args(["-icanon", "-echo", "-isig", "min", "0", "time", "0"])
            .status();
        RawTerm
    }
}

impl Drop for RawTerm {
    fn drop(&mut self) {
        let _ = std::process::Command::new("stty").arg("sane").status();
        println!();
    }
}

/// Keypress hold timers — terminals report presses, not releases, so each
/// press holds its control for a short window (key autorepeat refreshes it).
#[derive(Default)]
struct Keys {
    thrust: f32,
    brake: f32,
    left: f32,
    right: f32,
}

#[derive(Default)]
struct Stats {
    snapshots: u32,
    acked: u32,
    input_echo: u32,
    rtt_ms: f32,
    mine: Option<Id>,
    show_prof: bool,
    prof: Vec<String>,
}

fn heading_char(h: f32) -> u8 {
    const CHARS: [u8; 8] = [b'>', b'/', b'^', b'\\', b'<', b'/', b'v', b'\\'];
    let oct = (h / std::f32::consts::FRAC_PI_4).round().rem_euclid(8.0) as usize;
    CHARS[oct.min(7)]
}

fn run_player() {
    let mut pm = Pm::new();
    let car = pm.pool_get::<Car>("car"); // net target state (synced)
    let draw = pm.pool_get::<Car>("car_draw"); // smoothed display state (local)
    let keys = pm.state_get::<Keys>("keys");
    let stats = pm.state_get::<Stats>("stats");

    let mut net = NetClient::new();
    net.pool_sync("car", &car);
    let mut quic = QuicClient::connect(ADDR, &net.schema()).expect("connect");

    let lag_ms = env_f32("PM_LAG_MS");
    let loss = env_f32("PM_LOSS");
    if lag_ms > 0.0 || loss > 0.0 {
        quic.link_lag_set(Duration::from_secs_f32(lag_ms / 1000.0), loss);
    }
    eprintln!("connecting to {ADDR} ...");
    let _raw = RawTerm::enable();

    // Keyboard first in the tick so this tick's input rides this tick's
    // datagram.
    pm.task_add("keys", 4.0, {
        let keys = keys.clone();
        let stats = stats.clone();
        move |pm| {
            let dt = pm.loop_dt();
            let mut k = keys.borrow_mut();
            k.thrust -= dt;
            k.brake -= dt;
            k.left -= dt;
            k.right -= dt;
            let mut buf = [0u8; 64];
            let n = std::io::stdin().read(&mut buf).unwrap_or(0);
            for &b in &buf[..n] {
                const HOLD: f32 = 0.18;
                match b {
                    b'w' => k.thrust = HOLD,
                    b's' => k.brake = HOLD,
                    b'a' => k.left = HOLD,
                    b'd' => k.right = HOLD,
                    b'p' => {
                        let mut s = stats.borrow_mut();
                        s.show_prof = !s.show_prof;
                    }
                    b'q' | 3 => pm.loop_quit(), // q or ctrl-c
                    _ => {}
                }
            }
        }
    });

    pm.task_add("net", 5.0, {
        let keys = keys.clone();
        let stats = stats.clone();
        move |pm| {
            quic.pump();
            if let Some(err) = quic.error() {
                eprintln!("\r\ndisconnected: {err}");
                pm.loop_quit();
                return;
            }
            if quic.is_gone() {
                eprintln!("\r\nserver closed the connection");
                pm.loop_quit();
                return;
            }
            if let Some(peer) = quic.handshake_done() {
                pm.local_peer = peer;
                let k = keys.borrow();
                let cmd = Drive {
                    thrust: if k.thrust > 0.0 {
                        1.0
                    } else if k.brake > 0.0 {
                        -0.7
                    } else {
                        0.0
                    },
                    turn: if k.left > 0.0 {
                        1.0
                    } else if k.right > 0.0 {
                        -1.0
                    } else {
                        0.0
                    },
                };
                quic.input_send(bytemuck::bytes_of(&cmd));
            }
            for (ty, payload) in quic.events_drain() {
                if ty == EV_VEHICLE && payload.len() == 4 {
                    stats.borrow_mut().mine =
                        Some(Id(u32::from_le_bytes(payload.as_slice().try_into().unwrap())));
                }
            }
            for snap in quic.snapshots_drain() {
                if let Ok(applied) = net.apply(pm, &snap) {
                    quic.ack_send(applied.tick);
                    let mut s = stats.borrow_mut();
                    s.snapshots += 1;
                    s.acked = applied.tick;
                    s.input_echo = applied.input_seq;
                }
            }
            stats.borrow_mut().rtt_ms = quic.rtt().as_secs_f32() * 1e3;
        }
    });

    // Design convention live: deltas land in "car", display eases toward
    // it. A jump wider than the world means the target wrapped an edge —
    // snap instead of streaking across the screen.
    pm.task_add("smooth", 30.0, {
        let car = car.clone();
        let draw = draw.clone();
        move |pm| {
            let _p = pm::probe::scope("smooth.lerp");
            let target = car.borrow();
            let mut draw = draw.borrow_mut();
            for (id, t) in target.iter() {
                let cur = draw.get(id).copied().unwrap_or(*t);
                let wrapped = (t.x - cur.x).abs() > WORLD || (t.y - cur.y).abs() > WORLD;
                let next = if wrapped {
                    *t
                } else {
                    Car {
                        x: cur.x + (t.x - cur.x) * 0.35,
                        y: cur.y + (t.y - cur.y) * 0.35,
                        heading: t.heading,
                    }
                };
                draw.add(id, next);
            }
            let dead: Vec<Id> =
                draw.iter().map(|(id, _)| id).filter(|&id| !pm.id_alive(id)).collect();
            for id in dead {
                draw.remove(id);
            }
        }
    });

    // Profiling panel data: per-second deltas of the kernel task stats,
    // plus any drop-in probes on this thread.
    pm.task_add_every("prof", 55.0, 1.0, {
        let stats = stats.clone();
        let mut prev: HashMap<String, pm::TaskStat> = HashMap::new();
        move |pm| {
            let mut lines = Vec::new();
            for (name, s) in pm.task_stats() {
                let p = prev.get(&name).cloned().unwrap_or_default();
                let calls = s.calls - p.calls;
                if calls > 0 {
                    let avg_us = (s.ns_total - p.ns_total) as f32 / calls as f32 / 1000.0;
                    lines.push(format!(
                        "{name:<8} {avg_us:>8.1} us/call  {calls:>4}/s  max {:>8.1} us",
                        s.ns_max as f32 / 1000.0
                    ));
                }
                prev.insert(name, s);
            }
            for (name, s) in pm::probe::stats() {
                let avg_us = s.ns_total as f32 / s.calls.max(1) as f32 / 1000.0;
                lines.push(format!(
                    "~{name:<20} {avg_us:>6.1} us avg  max {:>8.1} us",
                    s.ns_max as f32 / 1000.0
                ));
            }
            stats.borrow_mut().prof = lines;
        }
    });

    pm.task_add_every("render", 50.0, 1.0 / 30.0, {
        let draw = draw.clone();
        let stats = stats.clone();
        move |pm| {
            let s = stats.borrow();
            let mut grid = vec![b' '; (COLS * ROWS) as usize];
            for (id, c) in draw.borrow().iter() {
                let col = ((c.x / WORLD) * (COLS as f32 / 2.0 - 1.0) + COLS as f32 / 2.0) as i32;
                let row = ((-c.y / WORLD) * (ROWS as f32 / 2.0 - 1.0) + ROWS as f32 / 2.0) as i32;
                if (0..COLS).contains(&col) && (0..ROWS).contains(&row) {
                    grid[(row * COLS + col) as usize] = if Some(id) == s.mine {
                        heading_char(c.heading)
                    } else {
                        b'0' + (id.index() % 10) as u8
                    };
                }
            }
            let mut out = String::from("\x1b[2J\x1b[H");
            let edge = format!("+{}+\r\n", "-".repeat(COLS as usize));
            out.push_str(&edge);
            for row in 0..ROWS {
                out.push('|');
                let line = &grid[(row * COLS) as usize..((row + 1) * COLS) as usize];
                out.push_str(std::str::from_utf8(line).unwrap());
                out.push_str("|\r\n");
            }
            out.push_str(&edge);
            out.push_str(&format!(
                "you are the arrow (peer {})  wasd drive, p profiling, q quit\r\n\
                 vehicles {}  rtt {:.1}ms  snapshots {}  tick {}  input echo {}\r\n",
                pm.local_peer,
                draw.borrow().len(),
                s.rtt_ms,
                s.snapshots,
                s.acked,
                s.input_echo,
            ));
            if s.show_prof {
                out.push_str("-- profiling (1s window) --\r\n");
                for line in &s.prof {
                    out.push_str(line);
                    out.push_str("\r\n");
                }
            }
            print!("{out}");
            let _ = std::io::stdout().flush();
        }
    });

    pm.loop_rate = 60;
    pm.loop_run();
}

fn main() {
    match std::env::args().nth(1).as_deref() {
        Some("server") => run_server(false),
        Some("client") => run_player(),
        Some("bot") => run_bot(0.0),
        _ => {
            std::thread::spawn(|| run_server(true));
            std::thread::sleep(std::time::Duration::from_millis(200));
            for i in 0..7 {
                std::thread::spawn(move || run_bot(i as f32 * 0.9));
            }
            std::thread::sleep(std::time::Duration::from_millis(200));
            run_player();
        }
    }
}
