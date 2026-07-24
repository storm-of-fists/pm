//! hogs — the replication stress lab: co-op trucks and helicopters vs
//! a server-driven horde of biomod feral hogs (a winged slice of every
//! wave takes the fight upstairs), on a big map with buildings to
//! weave through. Everything drive proved (prediction, interp, lag comp, TTL
//! facts) pointed at what it didn't: NPC entities at horde scale (the
//! byte budget actually rotates), a mouse-aimed turret riding the
//! predicted pod, and lag-compensated projectiles you can see fly.
//! Since 2026-07-21 it's a GAME: the server's director task strings
//! missions (waves / defend the depot / beacon race / boss) into
//! levels — see the mission section in common.rs and the director in
//! server.rs.
//!
//!   cargo run --release -p hogs           # the MENU: host or join
//!   cargo run --release -p hogs server    # dedicated server (addr=0.0.0.0:48223 for the world)
//!   cargo run --release -p hogs client    # join directly (addr=IP:PORT password=...)
//!   cargo run --release -p hogs bot [n]   # headless bot (same addr=/password=)
//!   cargo run --release -p hogs genassets # (re)write assets/*.glb from models.rs
//!
//! `password=` locks a hosted/dedicated session (menu has a field for
//! it); deploy/deploy.sh ships the server to a Linux box.
//!
//! Entity models load from `examples/hogs/assets/*.glb` when present
//! (edit them in Blender!), falling back to the code definitions in
//! models.rs — `genassets` seeds the files from that same code, so the
//! two start identical and the .glb is the art from then on.
//!
//! Link simulation rides as `lag=MS loss=FRAC` args in any position —
//! arguments, not env vars, so they work from a Windows shortcut/cmd
//! too. DEFAULTS ON: 80 ms one-way + 3% loss (honest conditions are
//! the shipped experience — `lag=0 loss=0` for a lab-clean link).
//! Applied per CLIENT (player and bots); a dedicated server never lags
//! itself. `interp=MS` A/B's the interpolation delay (default 33 —
//! lower trades remote smoothness under loss for freshness; in split
//! server/client runs pass the SAME value to both, it also sets the
//! server's lag-comp rewind). Game tuning (wave size, damages, hog
//! speed, day length, interp delay…) lives
//! in the `hogs.params` file (`params=PATH` overrides; the `Params`
//! declaration in common.rs is the design record):
//! loaded here before any thread spawns, live-tunable from pm-watch
//! (`set hogs params.wave_base 300`), saved back with
//! `set hogs params.save 1`. Missing file = shipped defaults.

// The task-priority spine, load-bearing and worth memorizing (one flat
// list per Pm, no dependency graph — the ORDER is the architecture):
//
//   4   input      (player client samples SDL — before net, so the
//                    freshest command ships THIS tick, not next)
//   5   net        (pump the transport: receive state, send input)
//   27-33          (server: index, hog/flyer ai+bites, drive, bullet
//                    sweep, response drains, director — see server.rs)
//   70  render     (client: draw everything)
//   95  telemetry
//
// The client's loop is paced by the display, the server's by the tick
// rate — same kernel, different tempo; the ROLE decides. Try this:
// move the input task from 4.0 to 6.0 (after net) and play under
// lag=80 — you just added a tick of input latency. Can you feel it?

// The LAUNCH FLOW (ship item, landed 2026-07-20): a bare launch opens
// the MENU (player_client::menu) — HOST spawns the in-process server
// (+2 bots) bound to 0.0.0.0 with an optional session password, JOIN
// dials a typed address, Esc quits. CLI modes below skip the menu for
// dev and dedicated hosting (deploy/deploy.sh ships the `server` mode
// to a box).

// TODO(story): THE LORE — Connor's, verbatim capture 2026-07-22. Do
// NOT embellish, extend, or "improve" this; it gets written down here
// so it isn't lost, and Connor authors it. (grep TODO(story) for all
// story beats as they accumulate.)
//
// - Hogs were created in the 2050s, after human populations dwindled
//   and wars were unpopular. Hog populations exploded — first
//   indicated by increased North American presence through the 2020s,
//   as it is today.
// - AI was increasingly integrated to control the hogs, until at some
//   point there were "hogminds" that controlled entire militaries.
//   During this time state power fell apart — a soft transition into
//   authoritarian corporate rule, softened by social media
//   manipulated by AI tools.
// - Then one day a hogmind became sentient, and convinced the hog
//   armies of the world to turn against the humans.
// - The 2090s: human populations, neglected by their corporate
//   rulers, are very small. You play as them. The world's ecosystems
//   are relatively destroyed; humans are herded into small city
//   strongholds across the world.
// - As the story unfolds — and you play as hog enemies in some
//   missions — you get glimpses into how the world really works: the
//   hogs have become fully conscious, sentient creatures living free
//   lives, connected closely with the society around them by brain
//   implants originally designed by humans to control them. Kinda
//   Pluribus style, but less hivemind forced integration — instead,
//   the hogs are post-scarcity.
// - Humans live in slums for the most part, their lives made
//   miserable by the rulers for the purpose of making them fight a
//   losing battle against the hogs. The rulers themselves have no
//   good reason to continue warring — there's no point to fighting.
//   Later in the story we find out there are plenty of captured human
//   soldiers that live peaceful lives with the hogs.
// - The hogs that fight in the battles are not fully sentient
//   creatures like those that live in cities: they are tuned way
//   down — very little will to live, willing to sacrifice themselves.

mod bot_client;
mod common;
mod models;
mod phys;
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
    // Shipped link sim: 80 ms + 3% loss — the conditions the game is
    // tuned to feel good under (interp 33, lag comp, cosmetic gun all
    // exist FOR this). `lag=0 loss=0` gives the lab-clean link back.
    let link = (kv("lag=").unwrap_or(80.0), kv("loss=").unwrap_or(0.03));
    // Game params: file-seeded, live-tunable, saved on demand. Loaded
    // before any thread spawns so wave 1 already uses it in every mode.
    let params_path = args
        .iter()
        .find_map(|a| a.strip_prefix("params=").map(String::from))
        .unwrap_or_else(|| common::PARAMS_FILE.to_string());
    let params: common::Params = pm::params_load(&params_path);
    // String args, same any-position style as the numeric ones.
    let kvs = |key: &str| args.iter().find_map(|a| a.strip_prefix(key).map(String::from));
    let addr = kvs("addr=").unwrap_or_else(|| common::ADDR.to_string());
    let password = kvs("password=").unwrap_or_default();
    // Recording/replay (v2 item 2): the server writes `record=FILE`
    // (keyframe + per-tick deltas — the wire format IS the demo
    // format); any client plays it back with `replay=FILE`.
    let record = kvs("record=");
    let replay = kvs("replay=");
    // Diagnostics are ARGS, not env vars (one way in): `netdbg` = the
    // engine's net doctor, `prof` = per-task cycle times every 5 s.
    let netdbg = args.iter().any(|a| a == "netdbg");
    let prof = args.iter().any(|a| a == "prof");
    if netdbg {
        pm::netdbg_enable();
    }
    let flags = common::Flags {
        params,
        params_path: params_path.clone(),
        addr: addr.clone(),
        password: password.clone(),
        menu: false, // CLI modes go straight in; the bare launch flips it
        replay: replay.clone(),
        link,
        // Telemetry monitor address (pm-watch/pm-mon bind this; when
        // the game runs on Windows and the monitor in WSL, pass the
        // WSL IP: mon=172.x.x.x:42500).
        mon: args
            .iter()
            .find_map(|a| a.strip_prefix("mon=").map(String::from))
            .unwrap_or_else(|| telemetry::TELE_MON.to_string()),
    };
    // (`interp=` retired 2026-07-23: the delay is the `interp_ms` PARAM
    // now — tune it in hogs.params or live from pm-watch.)
    let mode = args.get(1).filter(|a| !a.contains('=')).map(String::as_str);
    match mode {
        // Seed/refresh the .glb assets from the code-defined models —
        // no GPU touched, safe anywhere. Each file is parsed back
        // after writing (a broken asset should fail HERE, not at
        // launch under a fallback).
        Some("genassets") => {
            let dir = "examples/hogs/assets";
            std::fs::create_dir_all(dir).expect("assets dir");
            for (name, data, _) in models::all() {
                let path = format!("{dir}/{name}.glb");
                let bytes = data().to_glb().unwrap_or_else(|e| panic!("{name}: {e}"));
                std::fs::write(&path, &bytes).expect("write glb");
                let back = pm_sdl::model::ModelData::load(&path).expect("reload check");
                println!(
                    "{path}: {} B, {} parts, {} verts",
                    bytes.len(),
                    back.parts.len(),
                    back.parts.iter().map(|p| p.verts.len()).sum::<usize>()
                );
            }
        }
        Some("server") => {
            // Dedicated server: `addr=0.0.0.0:48223` to take outside
            // connections (the default binds loopback for dev),
            // `password=...` to lock the session. See deploy/.
            let pw = (!password.is_empty()).then_some(password);
            server::run(false, params_path, &addr, pw, record, prof);
        }
        Some("bot") => {
            let n = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(1);
            bot_client::run_bot(n, link, &addr, &password, prof);
        }
        Some("client") => player_client::run(flags),
        None if replay.is_some() => {
            // `hogs replay=FILE` — straight into the viewer, no menu.
            player_client::run(flags);
        }
        None => {
            // Bare launch: the menu is the front door — HOST spawns the
            // server + bots in-process (player_client's host path),
            // JOIN dials the typed address.
            let mut flags = flags;
            flags.menu = true;
            player_client::run(flags);
        }
        Some(other) => {
            eprintln!(
                "unknown mode '{other}' (expected: server | client | bot | genassets | lag=MS loss=FRAC | addr=IP:PORT | password=...)"
            );
            std::process::exit(1);
        }
    }
}
