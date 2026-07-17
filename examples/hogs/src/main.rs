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
//! value to both, it also sets the server's lag-comp rewind). `day=SECS`
//! sets the day-night cycle length (default 480; try day=60 to preview
//! a full day fast). Game tuning (wave size, damages, hog speed…) lives
//! in the `hogs.params` file (`params=PATH` overrides; docs/params.md):
//! loaded here before any thread spawns, live-tunable from pm-watch
//! (`set hogs params.wave_base 300`), saved back with
//! `set hogs params.save 1`. Missing file = shipped defaults.

mod bot_client;
mod common;
mod player_client;
mod server;
mod sfx;
mod telemetry;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    // Link args: `lag=80` (one-way ms) and `loss=0.03`, any position.
    let kv = |key: &str| {
        args.iter()
            .find_map(|a| a.strip_prefix(key).and_then(|v| v.parse::<f32>().ok()))
    };
    let link = (kv("lag=").unwrap_or(0.0), kv("loss=").unwrap_or(0.0));
    // Game params: file-seeded, live-tunable, saved on demand
    // (docs/params.md). Loaded before any thread spawns so wave 1
    // already uses it in every mode.
    let params_path = args
        .iter()
        .find_map(|a| a.strip_prefix("params=").map(String::from))
        .unwrap_or_else(|| common::PARAMS_FILE.to_string());
    let params = common::params_load(&params_path);
    let flags = common::Flags {
        params,
        link,
        // Day-night cycle length, seconds (render-only; each client may
        // differ — it's cosmetic time, not shared time). Live-tunable
        // via telemetry after launch.
        day: kv("day=").unwrap_or(480.0).max(10.0),
        // Effective interp delay (for the telemetry report): the flag,
        // else the env, else the shipped 50 ms default.
        interp_ms: kv("interp=")
            .or_else(|| {
                std::env::var("PM_INTERP_MS")
                    .ok()
                    .and_then(|v| v.parse().ok())
            })
            .unwrap_or(50.0),
        // Telemetry monitor address (pm-watch/pm-mon bind this; when
        // the game runs on Windows and the monitor in WSL, pass the
        // WSL IP: mon=172.x.x.x:42500).
        mon: args
            .iter()
            .find_map(|a| a.strip_prefix("mon=").map(String::from))
            .unwrap_or_else(|| telemetry::TELE_MON.to_string()),
    };
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
        Some("server") => server::run(false, params, params_path),
        Some("bot") => {
            let n = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(1);
            bot_client::run_bot(n, link);
        }
        Some("client") => player_client::run(flags),
        None => {
            let server = std::thread::spawn(move || server::run(true, params, params_path));
            std::thread::sleep(std::time::Duration::from_millis(300));
            for n in 0..2 {
                std::thread::spawn(move || bot_client::run_bot(n, link));
            }
            player_client::run(flags);
            drop(server); // window closed: process exit tears the rest down
        }
        Some(other) => {
            eprintln!("unknown mode '{other}' (expected: server | client | bot | lag=MS loss=FRAC)");
            std::process::exit(1);
        }
    }
}
