//! hogs — the replication stress lab: co-op trucks vs a server-driven
//! horde of biomod feral hogs, on a big map with buildings to weave
//! through. Everything drive proved (prediction, interp, lag comp, TTL
//! facts) pointed at what it didn't: NPC entities at horde scale (the
//! byte budget actually rotates), a mouse-aimed turret riding the
//! predicted pod, and lag-compensated projectiles you can see fly.
//!
//!   cargo run --release -p hogs           # play: server + 2 bots + you
//!   cargo run --release -p hogs server    # dedicated server
//!   cargo run --release -p hogs client    # join 127.0.0.1
//!   cargo run --release -p hogs bot [n]   # headless bot
//!
//! Link simulation rides as `lag=MS loss=FRAC` args in any position
//! (`hogs lag=80 loss=0.03`) — arguments, not env vars, so they work
//! from a Windows shortcut/cmd too. Applied per CLIENT (player and
//! bots); a dedicated server never lags itself. `interp=MS` A/B's the
//! interpolation delay (default 50 — lower trades remote smoothness
//! under loss for freshness; in split server/client runs pass the SAME
//! value to both, it also sets the server's lag-comp rewind). Stress
//! knob: PM_HOGS=300 sets the first-wave horde size.

mod bot_client;
mod common;
mod player_client;
mod server;
mod sfx;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    // Link args: `lag=80` (one-way ms) and `loss=0.03`, any position.
    let kv = |key: &str| {
        args.iter()
            .find_map(|a| a.strip_prefix(key).and_then(|v| v.parse::<f32>().ok()))
    };
    let link = (kv("lag=").unwrap_or(0.0), kv("loss=").unwrap_or(0.0));
    if let Some(ms) = kv("interp=") {
        // The interp delay is a shared const with a PM_INTERP_MS env
        // override read at startup (client render delay AND server
        // lag-comp rewind — common.rs interp_delay). Surface it as an
        // arg the same way: set the env before any thread exists.
        // SAFETY: main is still single-threaded here.
        unsafe { std::env::set_var("PM_INTERP_MS", format!("{ms}")) };
    }
    let mode = args.get(1).filter(|a| !a.contains('=')).map(String::as_str);
    match mode {
        Some("server") => server::run(false),
        Some("bot") => {
            let n = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(1);
            bot_client::run_bot(n, link);
        }
        Some("client") => player_client::run(link),
        None => {
            let server = std::thread::spawn(|| server::run(true));
            std::thread::sleep(std::time::Duration::from_millis(300));
            for n in 0..2 {
                std::thread::spawn(move || bot_client::run_bot(n, link));
            }
            player_client::run(link);
            drop(server); // window closed: process exit tears the rest down
        }
        Some(other) => {
            eprintln!("unknown mode '{other}' (expected: server | client | bot | lag=MS loss=FRAC)");
            std::process::exit(1);
        }
    }
}
