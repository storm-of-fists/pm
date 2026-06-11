//! Networked demo: 8 clients connect to a QUIC server — 7 bots and you.
//! Every peer drives its own server-authoritative vehicle; input flows as
//! sequenced datagrams, state comes back as snapshot deltas.
//!
//! The player client runs full client-side prediction: your car is
//! simulated locally the moment you press a key, and reconciled against
//! the server's input-seq echo (rewind + replay on divergence). Remote
//! cars are dead-reckoned along their last known heading/speed.
//!
//!   cargo run --release --example demo            # server + 7 bots + you
//!   cargo run --release --example demo -- server  # dedicated server on :47777
//!   cargo run --release --example demo -- client  # player client only
//!   cargo run --release --example demo -- bot     # one bot client only
//!
//! Controls: w go (latched), s reverse (latched), space coast, a/d steer,
//! p profiling panel, q quit. Your vehicle is the arrow; bots are digits.
//! (Throttle latches because terminals report key presses, not releases.)
//!
//! Simulate a real link on the player's connection:
//!
//!   PM_LAG_MS=80 PM_LOSS=0.05 cargo run --release --example demo

#[cfg(feature = "sdl")]
mod sdl_client;

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::rc::Rc;
use std::time::Duration;

use pm::{Id, NetClient, NetServer, Pm, Pool, QuicClient, QuicServer};

const ADDR: &str = "127.0.0.1:47777";
const WORLD: f32 = 12.0; // world is [-WORLD, WORLD] on both axes
const EV_VEHICLE: u16 = 16; // server -> client: "this entity is yours"

/// Simulation runs on a fixed dt on BOTH sides — reconciliation replays
/// inputs and must reproduce the server's exact arithmetic.
const FIXED_DT: f32 = 1.0 / 60.0;

/// Replicated vehicle state. Full dynamic state is synced (not just the
/// pose) because prediction needs everything the step function reads.
#[derive(Clone, Copy, PartialEq, Default, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
struct Car {
    x: f32,
    y: f32,
    heading: f32,
    speed: f32,
}

/// The input payload, client -> server.
#[derive(Clone, Copy, Default, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
struct Drive {
    thrust: f32, // -1..1
    turn: f32,   // -1..1, positive = counterclockwise
}

/// THE step function: server simulation, client prediction, and
/// reconciliation replay all call exactly this.
fn drive_step(c: &mut Car, cmd: Drive, dt: f32) {
    c.speed = ((c.speed + cmd.thrust * 9.0 * dt) * (1.0 - 1.5 * dt)).clamp(-3.0, 7.0);
    c.heading += cmd.turn * 2.8 * dt;
    let (sin, cos) = c.heading.sin_cos();
    c.x = wrap(c.x + cos * c.speed * dt);
    c.y = wrap(c.y + sin * c.speed * dt);
}

/// Dead-reckoning projection for remote cars: constant heading + speed.
fn coast_step(c: &mut Car, dt: f32) {
    let (sin, cos) = c.heading.sin_cos();
    c.x = wrap(c.x + cos * c.speed * dt);
    c.y = wrap(c.y + sin * c.speed * dt);
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

/// Per-peer input queue: the command-frame model. The drive task consumes
/// ONE input per tick (matching the one prediction step the client took
/// for it), holds the last command when the queue runs dry, and skips
/// ahead if the queue backs up.
#[derive(Default)]
struct Inbox(HashMap<u8, VecDeque<(u32, Drive)>>);
#[derive(Default)]
struct LastCmd(HashMap<u8, (u32, Drive)>); // last applied (seq, cmd)
#[derive(Default)]
struct Garage(HashMap<u8, Id>); // peer -> vehicle

fn run_server(quiet: bool) {
    let mut pm = Pm::new();
    let car = pm.pool_get::<Car>("car");
    let inbox = pm.state_get::<Inbox>("inbox");
    let last_cmd = pm.state_get::<LastCmd>("last_cmd");
    let garage = pm.state_get::<Garage>("garage");

    let mut net = NetServer::new(&mut pm);
    net.pool_sync("car", &car);
    // Bind failure must kill the whole process loudly — a silent thread
    // panic here once let the demo piggyback a stale server on the port.
    let mut quic = QuicServer::bind(ADDR, &net.schema()).unwrap_or_else(|e| {
        eprintln!("cannot bind {ADDR}: {e}");
        eprintln!("(a previous demo may still be running: pkill -x demo)");
        std::process::exit(1);
    });
    if !quiet {
        eprintln!("pm demo server on {ADDR}");
    }

    pm.task_add("net", 5.0, {
        let car = car.clone();
        let inbox = inbox.clone();
        let last_cmd = last_cmd.clone();
        let garage = garage.clone();
        move |pm| {
            quic.pump();
            for p in quic.joined_drain() {
                net.peer_add(p);
                let id = pm.id_add();
                let spread = p as f32 * 0.8;
                car.borrow_mut().add(
                    id,
                    Car {
                        x: 7.0 * spread.cos(),
                        y: 7.0 * spread.sin(),
                        heading: spread + 1.6,
                        speed: 0.0,
                    },
                );
                garage.borrow_mut().0.insert(p, id);
                quic.event_send(p, EV_VEHICLE, &id.0.to_le_bytes());
                if !quiet {
                    eprintln!("peer {p} joined, vehicle index {}", id.index());
                }
            }
            for p in quic.left_drain() {
                net.peer_remove(p);
                inbox.borrow_mut().0.remove(&p);
                last_cmd.borrow_mut().0.remove(&p);
                if let Some(id) = garage.borrow_mut().0.remove(&p) {
                    pm.id_remove(id);
                }
                if !quiet {
                    eprintln!("peer {p} left");
                }
            }
            for (p, seq, bytes) in quic.inputs_drain() {
                if bytes.len() == size_of::<Drive>() {
                    inbox
                        .borrow_mut()
                        .0
                        .entry(p)
                        .or_default()
                        .push_back((seq, bytemuck::pod_read_unaligned(&bytes)));
                }
            }
            for (p, tick) in quic.acks_drain() {
                net.ack(p, tick);
            }
            // Echo what the drive task actually APPLIED (last tick) — the
            // client reconciles its prediction against exactly this.
            for (&p, &(seq, _)) in &last_cmd.borrow().0 {
                net.input_processed(p, seq);
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
        let inbox = inbox.clone();
        let last_cmd = last_cmd.clone();
        let garage = garage.clone();
        move |_pm| {
            let mut inbox = inbox.borrow_mut();
            let mut last_cmd = last_cmd.borrow_mut();
            let mut car = car.borrow_mut();
            for (&peer, &id) in &garage.borrow().0 {
                let q = inbox.0.entry(peer).or_default();
                // Bound queue-induced input latency to ~2 ticks.
                while q.len() > 2 {
                    let skipped = q.pop_front().unwrap();
                    last_cmd.0.insert(peer, skipped);
                }
                if let Some(next) = q.pop_front() {
                    last_cmd.0.insert(peer, next);
                }
                let (_, cmd) = last_cmd.0.get(&peer).copied().unwrap_or_default();
                if let Some(mut c) = car.get_mut(id) {
                    drive_step(&mut c, cmd, FIXED_DT);
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

/// Throttle is LATCHED (terminals only report presses, and only the most
/// recent key autorepeats — a hold-timer throttle dies the moment you
/// steer). Steering stays momentary on a short hold window.
#[derive(Default)]
struct Keys {
    throttle: f32, // latched: 1.0 forward / -0.7 reverse / 0.0 coast
    left: f32,
    right: f32,
}

/// The command the input layer wants sent this tick — written by the
/// input task (terminal keys or SDL keyboard), read by the net task.
#[derive(Default)]
struct CurCmd(Drive);

/// Client-side prediction state for the player's own car.
#[derive(Default)]
struct Pred {
    have: bool,
    car: Car,
    /// (input seq, cmd, predicted state after applying it)
    ring: VecDeque<(u32, Drive, Car)>,
}

#[derive(Default)]
struct Stats {
    snapshots: u32,
    acked: u32,
    input_echo: u32,
    corrections: u32,
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

/// Build the QUIC client with the PM_LAG_MS / PM_LOSS link simulation.
fn client_connect(net: &NetClient) -> QuicClient {
    let mut quic = QuicClient::connect(ADDR, &net.schema()).expect("connect");
    let lag_ms = env_f32("PM_LAG_MS");
    let loss = env_f32("PM_LOSS");
    if lag_ms > 0.0 || loss > 0.0 {
        quic.link_lag_set(Duration::from_secs_f32(lag_ms / 1000.0), loss);
    }
    quic
}

/// Everything a player client needs except input and rendering: the net
/// task (pump, snapshot apply, prediction + reconciliation, input send),
/// the smoothing/dead-reckoning task, and the profiling collector. Shared
/// by the terminal and SDL clients — the input layer writes `CurCmd`, the
/// renderer reads `car_draw` and `Stats`.
fn add_client_tasks(
    pm: &mut Pm,
    mut quic: QuicClient,
    net: NetClient,
    car: &Rc<RefCell<Pool<Car>>>,
    draw: &Rc<RefCell<Pool<Car>>>,
) {
    let cmd = pm.state_get::<CurCmd>("cmd");
    let pred = pm.state_get::<Pred>("pred");
    let stats = pm.state_get::<Stats>("stats");
    let car = car.clone();
    let draw = draw.clone();

    pm.task_add("net", 5.0, {
        let cmd = cmd.clone();
        let pred = pred.clone();
        let car = car.clone();
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
            for (ty, payload) in quic.events_drain() {
                if ty == EV_VEHICLE && payload.len() == 4 {
                    stats.borrow_mut().mine =
                        Some(Id(u32::from_le_bytes(payload.as_slice().try_into().unwrap())));
                }
            }

            // 1) Apply snapshots, reconciling the prediction against the
            //    server's echo of the last input it actually applied.
            for snap in quic.snapshots_drain() {
                let Ok(applied) = net.apply(pm, &snap) else { continue };
                quic.ack_send(applied.tick);
                let mut s = stats.borrow_mut();
                s.snapshots += 1;
                s.acked = applied.tick;
                s.input_echo = applied.input_seq;
                let mine = s.mine;
                drop(s);

                let auth = mine.and_then(|id| car.borrow().get(id).copied());
                let Some(auth) = auth else { continue };
                let mut p = pred.borrow_mut();
                if !p.have {
                    p.have = true;
                    p.car = auth;
                    p.ring.clear();
                    continue;
                }
                // Drop ring entries the server has consumed, keeping the
                // one matching the echo for comparison.
                let mut predicted_then: Option<Car> = None;
                while let Some(&(seq, _, state)) = p.ring.front() {
                    if seq > applied.input_seq {
                        break;
                    }
                    if seq == applied.input_seq {
                        predicted_then = Some(state);
                    }
                    p.ring.pop_front();
                }
                if let Some(was) = predicted_then {
                    let err = (was.x - auth.x).abs()
                        + (was.y - auth.y).abs()
                        + (was.heading - auth.heading).abs()
                        + (was.speed - auth.speed).abs();
                    if err > 1e-4 {
                        // Rewind to authority and replay unacked inputs.
                        p.car = auth;
                        let mut replayed = auth;
                        for entry in p.ring.iter_mut() {
                            drive_step(&mut replayed, entry.1, FIXED_DT);
                            entry.2 = replayed;
                        }
                        p.car = replayed;
                        stats.borrow_mut().corrections += 1;
                    }
                } else if p.ring.is_empty() && applied.input_seq > 0 {
                    p.car = auth; // long stall: adopt authority
                }
            }

            // 2) Send this tick's input and predict its result instantly.
            if let Some(peer) = quic.handshake_done() {
                pm.local_peer = peer;
                let cmd = cmd.borrow().0;
                let seq = quic.input_send(bytemuck::bytes_of(&cmd));
                let mut p = pred.borrow_mut();
                if p.have {
                    let mut next = p.car;
                    drive_step(&mut next, cmd, FIXED_DT);
                    p.car = next;
                    p.ring.push_back((seq, cmd, next));
                    if p.ring.len() > 240 {
                        p.ring.pop_front(); // ~4 s of unacked input: cap
                    }
                }
            }
            stats.borrow_mut().rtt_ms = quic.rtt().as_secs_f32() * 1e3;
        }
    });

    // Display: own car comes straight from the prediction (instant input
    // response); remote cars dead-reckon along their last known velocity
    // and ease toward fresh server state as it arrives. A jump wider than
    // the world means a wrap — snap instead of streaking across.
    pm.task_add("smooth", 30.0, {
        let car = car.clone();
        let draw = draw.clone();
        let pred = pred.clone();
        let stats = stats.clone();
        move |pm| {
            let _p = pm::probe::scope("smooth.reckon");
            let mine = stats.borrow().mine;
            let net_car = car.borrow();
            let mut draw = draw.borrow_mut();
            for (id, t) in net_car.iter() {
                if Some(id) == mine {
                    continue;
                }
                let mut cur = draw.get(id).copied().unwrap_or(*t);
                coast_step(&mut cur, FIXED_DT);
                let wrapped = (t.x - cur.x).abs() > WORLD || (t.y - cur.y).abs() > WORLD;
                let next = if wrapped {
                    *t
                } else {
                    Car {
                        x: cur.x + (t.x - cur.x) * 0.12,
                        y: cur.y + (t.y - cur.y) * 0.12,
                        heading: t.heading,
                        speed: t.speed,
                    }
                };
                draw.add(id, next);
            }
            if let (Some(id), p) = (mine, pred.borrow())
                && p.have {
                    draw.add(id, p.car);
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
}

fn run_player() {
    let mut pm = Pm::new();
    let car = pm.pool_get::<Car>("car"); // net state from the server (synced)
    let draw = pm.pool_get::<Car>("car_draw"); // display state (local)
    let keys = pm.state_get::<Keys>("keys");
    let cmd = pm.state_get::<CurCmd>("cmd");
    let stats = pm.state_get::<Stats>("stats");

    let mut net = NetClient::new();
    net.pool_sync("car", &car);
    let quic = client_connect(&net);
    eprintln!("connecting to {ADDR} ...");
    let _raw = RawTerm::enable();
    add_client_tasks(&mut pm, quic, net, &car, &draw);

    // Keyboard first in the tick so this tick's input rides this tick's
    // datagram.
    pm.task_add("keys", 4.0, {
        let keys = keys.clone();
        let cmd = cmd.clone();
        let stats = stats.clone();
        move |pm| {
            let dt = pm.loop_dt();
            let mut k = keys.borrow_mut();
            k.left -= dt;
            k.right -= dt;
            let mut buf = [0u8; 64];
            let n = std::io::stdin().read(&mut buf).unwrap_or(0);
            for &b in &buf[..n] {
                const HOLD: f32 = 0.18;
                match b {
                    b'w' => k.throttle = 1.0,
                    b's' => k.throttle = -0.7,
                    b' ' | b'x' => k.throttle = 0.0,
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
            cmd.borrow_mut().0 = Drive {
                thrust: k.throttle,
                turn: if k.left > 0.0 {
                    1.0
                } else if k.right > 0.0 {
                    -1.0
                } else {
                    0.0
                },
            };
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
                "you are the arrow (peer {})  w go, s reverse, space coast, a/d steer, q quit\r\n\
                 vehicles {}  rtt {:.1}ms  snapshots {}  tick {}  echo {}  corrections {}\r\n",
                pm.local_peer,
                draw.borrow().len(),
                s.rtt_ms,
                s.snapshots,
                s.acked,
                s.input_echo,
                s.corrections,
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

/// Default player: SDL window when compiled with `--features sdl`,
/// terminal renderer otherwise.
fn run_player_default() {
    #[cfg(feature = "sdl")]
    sdl_client::run();
    #[cfg(not(feature = "sdl"))]
    run_player();
}

fn main() {
    match std::env::args().nth(1).as_deref() {
        Some("server") => run_server(false),
        Some("client") => run_player(),
        #[cfg(feature = "sdl")]
        Some("sdl") => sdl_client::run(),
        #[cfg(not(feature = "sdl"))]
        Some("sdl") => eprintln!("rebuild with: cargo run --release --features sdl --example demo"),
        Some("bot") => run_bot(0.0),
        _ => {
            std::thread::spawn(|| run_server(true));
            std::thread::sleep(std::time::Duration::from_millis(200));
            for i in 0..7 {
                std::thread::spawn(move || run_bot(i as f32 * 0.9));
            }
            std::thread::sleep(std::time::Duration::from_millis(200));
            run_player_default();
        }
    }
}
