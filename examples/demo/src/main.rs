//! Networked demo: 8 clients connect to a QUIC server — 7 bots and you.
//! Every peer drives its own server-authoritative vehicle; input flows as
//! sequenced datagrams, state comes back as snapshot deltas.
//!
//! The player client runs full client-side prediction: your car is
//! simulated locally the moment you press a key, and reconciled against
//! the server's input-seq echo (rewind + replay on divergence). Remote
//! cars are dead-reckoned along their last known heading/speed.
//!
//! Renders in the terminal — the netcode reference that works over ssh.
//! For windowed clients on the same stack see drive (3D) and hellfire.
//!
//!   cargo run --release -p demo           # server + 7 bots + you
//!   cargo run --release -p demo server    # dedicated server on :47777
//!   cargo run --release -p demo client    # player client only
//!   cargo run --release -p demo bot       # one bot client only
//!
//! Controls: w go (latched), s reverse (latched), space coast, a/d steer,
//! p profiling panel, q quit. Your vehicle is the arrow; bots are digits.
//! (Throttle latches because terminals report key presses, not releases.)
//!
//! Simulate a real link on the player's connection:
//!
//!   PM_LAG_MS=80 PM_LOSS=0.05 cargo run --release -p demo

use std::collections::HashMap;
use std::io::{Read, Write};

use pm::{Id, InputTx, Pm, PmClient, pool_mirror};

const ADDR: &str = "127.0.0.1:47777";
const WORLD: f32 = 12.0; // world is [-WORLD, WORLD] on both axes

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

// --- server -------------------------------------------------------------

fn run_server(quiet: bool) {
    let mut pm = Pm::server(ADDR);
    let car = pm.sync_pool::<Car>("car");
    // The server surface: joins/leaves this tick, plus the built-in
    // peer→entity channel — `own_set` ships each peer's avatar id in the
    // snapshot header (the client reads it via `net.mine()`) and doubles
    // as our peer→vehicle lookup, no bespoke "here's your id" event.
    let net = pm.net();
    // THE continuous input channel: same name and pod on the clients (the
    // handshake schema enforces it).
    let inputs = pm.input::<Drive>("drive");
    if !quiet {
        eprintln!("pm demo server on {ADDR}");
    }

    pm.task_add("roster", 10.0, 0.0, {
        let car = car.clone();
        let net = net.clone();
        move |pm| {
            for p in net.joined() {
                let id = pm.id_add();
                let spread = p as f32 * 0.8;
                car.get_mut().add(
                    id,
                    Car {
                        x: 7.0 * spread.cos(),
                        y: 7.0 * spread.sin(),
                        heading: spread + 1.6,
                        speed: 0.0,
                    },
                );
                net.own_set(p, id);
                if !quiet {
                    eprintln!("peer {p} joined, vehicle index {}", id.index());
                }
            }
            for p in net.left() {
                if let Some(id) = net.own(p) {
                    pm.id_remove(id);
                }
                net.own_clear(p);
                if !quiet {
                    eprintln!("peer {p} left");
                }
            }
        }
    });

    pm.task_add("drive", 30.0, 0.0, {
        let car = car.clone();
        let net = net.clone();
        move |_pm| {
            let mut car = car.get_mut();
            for (peer, id) in net.owned() {
                // Command-frame consumption: one input per tick (one
                // prediction step on the client), hold-when-dry, bounded
                // skip-ahead. The applied seq echoes back automatically.
                let cmd = inputs.pop(peer);
                if let Some(mut c) = car.get_mut(id) {
                    drive_step(&mut c, cmd, FIXED_DT);
                }
            }
        }
    });

    // Dedicated-server profiling: task table every 5 seconds.
    if !quiet {
        pm.task_add("prof", 90.0, 5.0, {
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

    // Bind failure must kill the whole process loudly — a silent thread
    // panic here once let the demo piggyback a stale server on the port.
    pm.loop_rate = 60;
    pm.run().unwrap_or_else(|e| {
        eprintln!("cannot serve {ADDR}: {e}");
        eprintln!("(a previous demo may still be running: pkill -x demo)");
        std::process::exit(1);
    });
}

// --- bot client -----------------------------------------------------------

fn run_bot(phase: f32) {
    let mut pm = Pm::client(ADDR, 1.0 / FIXED_DT);
    pm.sync_pool::<Car>("car");

    let input = pm.input::<Drive>("drive");
    pm.task_add("bot", 4.0, 0.0, move |pm| {
        let t = pm.tick() as f32 / 60.0;
        input.set(Drive {
            thrust: 0.7 + 0.3 * (t * 0.6 + phase).sin(),
            turn: (t * 0.8 + phase * 2.0).sin(),
        });
    });

    pm.loop_rate = 60;
    let _ = pm.run(); // a bot with no server to reach just exits
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

#[derive(Default)]
struct Stats {
    acked: u32,
    input_echo: u32,
    corrections: u32,
    mine: Option<Id>,
    show_prof: bool,
    prof: Vec<String>,
}

fn heading_char(h: f32) -> u8 {
    const CHARS: [u8; 8] = [b'>', b'/', b'^', b'\\', b'<', b'/', b'v', b'\\'];
    let oct = (h / std::f32::consts::FRAC_PI_4).round().rem_euclid(8.0) as usize;
    CHARS[oct.min(7)]
}

/// Everything a player client needs except input and rendering: the
/// built-in prediction (`pm.predict_pool` — reconcile against the applied
/// snapshots' echo, replay unacked inputs, smooth-predict into the draw
/// pool), the dead-reckoning task for remote cars, and the profiling
/// collector. pm owns the transport (`PM_LAG_MS`/`PM_LOSS` simulate the
/// link). Returns the draw pool (`"car.draw"`) the renderer should read.
fn add_client_tasks(
    pm: &mut PmClient,
    car: &pm::PoolHandle<Car>,
    input: &InputTx<Drive>,
) -> pm::PoolHandle<Car> {
    let net = pm.net();
    // No connect here — `run` does that once the schema is complete.
    // Local avatar: the same `drive_step` the server runs is what makes
    // reconciliation byte-exact. The predictor writes the smooth-predicted
    // car into the draw sibling pool every tick.
    let pred = pm.predict_pool(
        car,
        input,
        drive_step,
        |a, b| {
            (a.x - b.x).abs()
                + (a.y - b.y).abs()
                + (a.heading - b.heading).abs()
                + (a.speed - b.speed).abs()
        },
        1e-4,
        FIXED_DT,
    );
    let draw = pm.pool::<Car>("car.draw");
    let stats = pm.single::<Stats>("stats");

    // HUD stats, right after the predictor's tick (prio 8): which car is
    // ours (from the snapshot header, not a bespoke event), the newest
    // ack/echo, and the predictor's correction count.
    pm.task_add("stats", 8.0, 0.0, {
        let net = net.clone();
        let pred = pred.clone();
        let stats = stats.clone();
        move |_pm| {
            let mut s = stats.get_mut();
            s.mine = net.mine();
            for a in net.applied() {
                s.acked = a.tick;
                s.input_echo = a.input_seq;
            }
            s.corrections = pred.get().corrections;
        }
    });

    // Display: own car comes straight from the prediction (instant input
    // response — predict_pool wrote it at prio 7, so keep that entry);
    // remote cars dead-reckon along their last known velocity and ease
    // toward fresh server state as it arrives. pm::pool_mirror handles
    // add/blend/stale-drop; the closure is just the blend math. A jump
    // wider than the world means a wrap — snap instead of streaking across.
    pm.task_add("smooth", 30.0, 0.0, {
        let car = car.clone();
        let draw = draw.clone();
        let stats = stats.clone();
        move |pm| {
            let _p = pm::probe::scope("smooth.reckon");
            let dt = pm.loop_dt();
            let mine = stats.get().mine;
            pool_mirror(&car, &draw, |id, mut d, a: &Car| {
                if Some(id) == mine {
                    return d; // keep the predictor's smooth-predicted value
                }
                coast_step(&mut d, dt);
                let wrapped = (a.x - d.x).abs() > WORLD || (a.y - d.y).abs() > WORLD;
                if wrapped {
                    *a
                } else {
                    Car {
                        x: d.x + (a.x - d.x) * 0.12,
                        y: d.y + (a.y - d.y) * 0.12,
                        ..*a
                    }
                }
            });
        }
    });

    // Profiling panel data: per-second deltas of the kernel task stats,
    // plus any drop-in probes on this thread.
    pm.task_add("prof", 55.0, 1.0, {
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
            stats.get_mut().prof = lines;
        }
    });

    draw
}

fn run_player() {
    let mut pm = Pm::client(ADDR, 1.0 / FIXED_DT);
    let car = pm.sync_pool::<Car>("car"); // net state from the server (synced)
    let input = pm.input::<Drive>("drive"); // THE continuous input channel
    let keys = pm.single::<Keys>("keys");
    let stats = pm.single::<Stats>("stats");
    let net = pm.net();

    eprintln!("connecting to {ADDR} ...");
    let _raw = RawTerm::enable();
    // `draw` ("car.draw") is the display state: predicted local car,
    // dead-reckoned remotes.
    let draw = add_client_tasks(&mut pm, &car, &input);

    // Keyboard first in the tick so this tick's input rides this tick's
    // datagram.
    pm.task_add("keys", 4.0, 0.0, {
        let keys = keys.clone();
        let stats = stats.clone();
        let input = input.clone();
        move |pm| {
            let dt = pm.loop_dt();
            let mut k = keys.get_mut();
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
                        let mut s = stats.get_mut();
                        s.show_prof = !s.show_prof;
                    }
                    b'q' | 3 => pm.loop_quit(), // q or ctrl-c
                    _ => {}
                }
            }
            input.set(Drive {
                thrust: k.throttle,
                turn: if k.left > 0.0 {
                    1.0
                } else if k.right > 0.0 {
                    -1.0
                } else {
                    0.0
                },
            });
        }
    });

    pm.task_add("render", 50.0, 1.0 / 30.0, {
        let draw = draw.clone();
        let stats = stats.clone();
        let net = net.clone();
        move |_pm| {
            let s = stats.get();
            let mut grid = vec![b' '; (COLS * ROWS) as usize];
            for (id, c) in draw.get().iter() {
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
                net.peer(),
                draw.get().len(),
                net.rtt_ms(),
                net.snapshots(),
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
    pm.run().expect("connect");
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
