//! hellfire — networked top-down wave shooter (Rust port of the C++
//! example). Authoritative server, replicated-pool clients, up to 8
//! players racing 5 monster waves to 8000 points.
//!
//!   cargo run --release --features sdl --example hellfire           # play (server + 3 bots + you)
//!   cargo run --release --example hellfire server                   # dedicated server
//!   cargo run --release --features sdl --example hellfire client    # join 127.0.0.1
//!   cargo run --release --example hellfire bot [n]                  # headless bot

mod client;
mod common;
mod server;
#[cfg(feature = "sdl")]
mod sdl_client;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("server") => server::run(false),
        Some("bot") => {
            let n = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(1);
            client::run_bot(n);
        }
        #[cfg(feature = "sdl")]
        Some("client") => sdl_client::run(),
        #[cfg(not(feature = "sdl"))]
        Some("client") => {
            eprintln!("the window client needs the sdl feature:");
            eprintln!("  cargo run --release --features sdl --example hellfire client");
            std::process::exit(1);
        }
        None => {
            let server = std::thread::spawn(|| server::run(true));
            std::thread::sleep(std::time::Duration::from_millis(300));
            for n in 0..3 {
                std::thread::spawn(move || client::run_bot(n));
            }
            #[cfg(feature = "sdl")]
            sdl_client::run();
            #[cfg(not(feature = "sdl"))]
            {
                eprintln!("no sdl feature: running headless (server + 3 bots). ctrl-c to stop.");
                eprintln!("for the window: cargo run --release --features sdl --example hellfire");
                let _ = server.join();
            }
            #[cfg(feature = "sdl")]
            drop(server); // window closed: process exit tears the rest down
        }
        Some(other) => {
            eprintln!("unknown mode '{other}' (expected: server | client | bot)");
            std::process::exit(1);
        }
    }
}
