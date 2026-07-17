//! pm-watch — the headless sibling of pm-mon: plain-stdout monitoring
//! for logging, scripts, and agents watching a live session (the TUI is
//! for humans with a spare terminal; this is for everyone else).
//!
//! One summary line per node per period, fault stamps as they land, and
//! tune commands on stdin (the same unlock-override writes pm-mon sends
//! with Ctrl-U — leases renew automatically while we run):
//!
//! ```text
//! cargo run -p pm-control-host --bin pm-watch -- \
//!     --bind 127.0.0.1:42500 127.0.0.1:42501
//!
//! set hogs day_secs 60      # unlock-write: hold the knob at 60
//! lock hogs day_secs        # release it back to the app
//! clear hogs overrun_flt    # clear a latched fault at its owner
//! quit                      # lock everything back and exit
//! ```
//!
//! Flags: `--bind ADDR` (default 0.0.0.0:42500), `--period MS` (summary
//! cadence, default 1000), `--filter REGEX` (which signals print;
//! default: everything except `net.*`). Positional args are peer
//! addresses (default 255.255.255.255:42500 broadcast).

use std::io::BufRead;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use pm_control_core::{Monitor, clock};
use pm_control_host::UdpSegmentPort;
use regex_lite::Regex;

fn main() {
    let mut bind = "0.0.0.0:42500".to_string();
    let mut period_ms: u64 = 1000;
    let mut filter: Option<Regex> = None;
    let mut peers: Vec<String> = Vec::new();

    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--bind" => bind = args.next().expect("--bind ADDR"),
            "--period" => {
                period_ms = args
                    .next()
                    .and_then(|v| v.parse().ok())
                    .expect("--period MS")
            }
            "--filter" => {
                let pat = args.next().expect("--filter REGEX");
                filter = Some(Regex::new(&pat).expect("bad --filter regex"));
            }
            "--help" | "-h" => {
                eprintln!(
                    "pm-watch [--bind ADDR] [--period MS] [--filter REGEX] [PEER ...]\n\
                     stdin: set NODE SIG VALUE | lock NODE SIG | clear NODE SIG | quit"
                );
                return;
            }
            p => peers.push(p.to_string()),
        }
    }
    if peers.is_empty() {
        peers.push("255.255.255.255:42500".into());
    }
    let peer_refs: Vec<&str> = peers.iter().map(String::as_str).collect();
    let mut port = UdpSegmentPort::bind(&bind, &peer_refs);
    pm_control_host::install_us_clock();

    // stdin commands ride a channel; the reader thread parks on lines.
    let (tx, rx) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines() {
            let Ok(line) = line else { break };
            if tx.send(line).is_err() {
                break;
            }
        }
    });

    let t0 = Instant::now();
    let mut mon = Monitor::new();
    let mut next_print = Duration::from_millis(period_ms);
    // Fault stamps already reported: (node, signal name, stamp ms).
    let mut reported: Vec<(String, String, u64)> = Vec::new();

    println!("pm-watch: bind {bind}, peers {peers:?} — waiting for nodes");
    loop {
        let elapsed = t0.elapsed();
        clock::set(elapsed.as_millis() as u64);
        mon.poll(&mut port);

        // Fault edges print the moment we see a fresh stamp.
        for n in &mon.nodes {
            for f in &n.faults {
                let fname = f.sig.meta().name.borrow().clone();
                if f.stamp_ms != 0
                    && !reported
                        .iter()
                        .any(|(nn, fs, ms)| *nn == n.node && *fs == fname && *ms == f.stamp_ms)
                {
                    println!(
                        "[{:8.1}s] FAULT {}.{} (stamped at {} ms{})",
                        elapsed.as_secs_f32(),
                        n.node,
                        fname,
                        f.stamp_ms,
                        if f.active() { ", ACTIVE" } else { ", cleared" },
                    );
                    reported.push((n.node.clone(), fname, f.stamp_ms));
                }
            }
        }

        if elapsed >= next_print {
            next_print += Duration::from_millis(period_ms);
            for n in &mon.nodes {
                let mut line = String::new();
                for s in &n.signals {
                    let name = s.meta().name.borrow();
                    let show = match &filter {
                        Some(re) => re.is_match(&name),
                        None => !name.starts_with("net."),
                    };
                    if show {
                        line.push_str(&format!(" {}={}", *name, s.value_text()));
                    }
                }
                if !line.is_empty() {
                    println!("[{:8.1}s] {}{}", elapsed.as_secs_f32(), n.node, line);
                }
            }
        }

        while let Ok(cmd) = rx.try_recv() {
            let parts: Vec<&str> = cmd.split_whitespace().collect();
            match parts.as_slice() {
                ["quit"] | ["q"] => {
                    mon.lock_all(&mut port);
                    mon.unsubscribe_all(&mut port);
                    return;
                }
                ["set", node, sig, value] => {
                    let ok = mon.unlock(node, sig, value, &mut port);
                    println!("set {node}.{sig} = {value}: {}", if ok { "ok (held until `lock`)" } else { "FAILED (unknown signal or bad value)" });
                }
                ["lock", node, sig] => {
                    mon.lock(node, sig, &mut port);
                    println!("locked {node}.{sig} back to its app");
                }
                ["clear", node, sig] => {
                    mon.clear_fault(node, sig, &mut port);
                    println!("cleared {node}.{sig}");
                }
                [] => {}
                _ => println!("commands: set NODE SIG VALUE | lock NODE SIG | clear NODE SIG | quit"),
            }
        }

        std::thread::sleep(Duration::from_millis(20));
    }
}
