//! hellfire — networked top-down wave shooter (Rust port of the C++
//! example). Authoritative server, replicated-pool clients, up to 8
//! players racing 5 monster waves to 8000 points.
//!
//!   cargo run --release -p hellfire           # play (server + 3 bots + you)
//!   cargo run --release -p hellfire server    # dedicated server
//!   cargo run --release -p hellfire client    # join 127.0.0.1
//!   cargo run --release -p hellfire bot [n]   # headless bot

mod client;
mod common;
mod sdl_client;
mod server;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("server") => server::run(false),
        Some("bot") => {
            let n = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(1);
            client::run_bot(n);
        }
        Some("client") => sdl_client::run(),
        None => {
            let server = std::thread::spawn(|| server::run(true));
            std::thread::sleep(std::time::Duration::from_millis(300));
            for n in 0..3 {
                std::thread::spawn(move || client::run_bot(n));
            }
            sdl_client::run();
            drop(server); // window closed: process exit tears the rest down
        }
        Some(other) => {
            eprintln!("unknown mode '{other}' (expected: server | client | bot)");
            std::process::exit(1);
        }
    }
}
