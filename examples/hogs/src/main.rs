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
//! Stress knobs: PM_HOGS=300 sets the first-wave horde size;
//! PM_LAG_MS=80 PM_LOSS=0.03 makes the link honest.

mod bot_client;
mod common;
mod player_client;
mod server;
mod sfx;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("server") => server::run(false),
        Some("bot") => {
            let n = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(1);
            bot_client::run_bot(n);
        }
        Some("client") => player_client::run(),
        None => {
            let server = std::thread::spawn(|| server::run(true));
            std::thread::sleep(std::time::Duration::from_millis(300));
            for n in 0..2 {
                std::thread::spawn(move || bot_client::run_bot(n));
            }
            player_client::run();
            drop(server); // window closed: process exit tears the rest down
        }
        Some(other) => {
            eprintln!("unknown mode '{other}' (expected: server | client | bot)");
            std::process::exit(1);
        }
    }
}
