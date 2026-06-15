//! drive — networked 3D driving: authoritative server, predicted local
//! car, dead-reckoned remote cars, chase camera, pm_sdl::gpu3d
//! rendering. The whole stack in one example.
//!
//!   cargo run --release -p drive           # play: server + 3 bots + you
//!   cargo run --release -p drive server    # dedicated server
//!   cargo run --release -p drive client    # join 127.0.0.1
//!   cargo run --release -p drive bot [n]   # headless bot
//!
//! Feel a real link: PM_LAG_MS=80 PM_LOSS=0.03 cargo run --release -p drive client

mod bot_client;
mod common;
mod player_client;
mod server;

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
            for n in 0..3 {
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
