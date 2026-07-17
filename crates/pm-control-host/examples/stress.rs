//! stress — 4 nodes in a full mesh, 1000 published signals each.
//!
//! Every node publishes 1000 f32 signals and subscribes to all 1000 from
//! each of the other three (3000 subscriptions per node, 4000 signals on
//! the segment). Boot is measured from the first scan until every node
//! reports fully connected, then the mesh runs forever with values
//! sweeping so a monitor shows live churn.
//!
//! ```text
//! cargo run -p pm-control-host --example stress
//! cargo run -p pm-control-host --bin pm-mon -- --bind 127.0.0.1:42514 \
//!     127.0.0.1:42531 127.0.0.1:42532 127.0.0.1:42533 127.0.0.1:42534
//! ```
//!
//! (An already-running pm-mon on :42514 will pick the mesh up without
//! restarting — the nodes broadcast to that port and their mutual
//! subscriptions keep all data flowing.)

use pm_control_core::*;
use pm_control_host::UdpSegmentPort;
use std::time::Instant;

const NODES: usize = 4;
const SIGNALS_PER_NODE: usize = 1000;
const DT_MS: u64 = 50;
const EXT_MONITOR: &str = "127.0.0.1:42514";

/// A dynamically-sized block of signals — the hand-written `Register` route
/// for signal sets whose size isn't known at compile time.
struct Bank {
    sigs: Vec<PmF32>,
}

impl Bank {
    fn new() -> Bank {
        Bank { sigs: (0..SIGNALS_PER_NODE).map(|_| PmF32::new()).collect() }
    }
}

impl Register for Bank {
    fn register(&self, r: &mut Registrar) {
        for (i, s) in self.sigs.iter().enumerate() {
            r.child(&format!("sig_{i:04}"), s);
        }
    }
}

struct Node {
    /// banks[j] mirrors node j's signals; banks[me] is what we publish.
    banks: Vec<Bank>,
    net: NetworkManager,
    port: UdpSegmentPort,
}

fn main() {
    pm_control_host::install_us_clock();
    let addrs: Vec<String> = (0..NODES).map(|i| format!("127.0.0.1:{}", 42531 + i)).collect();

    let build_start = Instant::now();
    let mut nodes: Vec<Node> = (0..NODES)
        .map(|me| {
            let banks: Vec<Bank> =
                (0..NODES).map(|owner| Bank::new().node(&format!("n{owner}"))).collect();
            let mut net = NetworkManager::new();
            net.publish_health(); // watch n0..n3 link state + cycle timing in pm-mon
            for b in &banks {
                net.add(b);
            }
            let remotes: Vec<String> =
                (0..NODES).filter(|o| *o != me).map(|o| format!("n{o}")).collect();
            let remote_refs: Vec<&str> = remotes.iter().map(String::as_str).collect();
            net.bind(&format!("n{me}"), &remote_refs);

            let segment: Vec<&str> = addrs
                .iter()
                .enumerate()
                .filter(|(j, _)| *j != me)
                .map(|(_, a)| a.as_str())
                .chain([EXT_MONITOR])
                .collect();
            let port = UdpSegmentPort::bind(&addrs[me], &segment);
            Node { banks, net, port }
        })
        .collect();

    let st = nodes[0].net.status();
    println!(
        "stress: {NODES} nodes × {SIGNALS_PER_NODE} signals — per node: {} published, {} subscribed",
        st.publishers, st.subscribers
    );
    println!("build + bind: {:.1} ms", build_start.elapsed().as_secs_f64() * 1000.0);
    println!(
        "attach: cargo run -p pm-control-host --bin pm-mon -- --bind {EXT_MONITOR} {}\n",
        addrs.join(" ")
    );

    let t0 = Instant::now();
    let mut connected_at: Vec<Option<u64>> = vec![None; NODES];
    let mut boot_reported = false;
    let mut tick: u64 = 0;
    loop {
        clock::advance(DT_MS);
        tick += 1;
        let now = clock::now_ms();

        for n in nodes.iter_mut() {
            n.net.begin_cycle(&mut n.port);
        }
        // Own values sweep 0.0..99.9 so every signal visibly changes.
        for (me, n) in nodes.iter_mut().enumerate() {
            for (k, s) in n.banks[me].sigs.iter().enumerate() {
                s.set(((tick as usize + k) % 1000) as f32 / 10.0);
            }
        }
        for n in nodes.iter_mut() {
            n.net.end_cycle(&mut n.port);
        }

        if !boot_reported {
            let mut all = true;
            for (i, n) in nodes.iter().enumerate() {
                if n.net.status().connected {
                    if connected_at[i].is_none() {
                        connected_at[i] = Some(now);
                        println!(
                            "  n{i} fully resolved: {now} ms sim / {:.0} ms wall",
                            t0.elapsed().as_secs_f64() * 1000.0
                        );
                    }
                } else {
                    all = false;
                }
            }
            if all {
                boot_reported = true;
                println!(
                    "\nboot complete: all {NODES} nodes connected in {now} ms sim / {:.0} ms wall",
                    t0.elapsed().as_secs_f64() * 1000.0
                );
                println!("running forever — Ctrl-C to stop");
            } else if now.is_multiple_of(500) {
                let prog: Vec<String> = nodes
                    .iter()
                    .enumerate()
                    .map(|(i, n)| format!("n{i}={}", n.net.status().unresolved))
                    .collect();
                println!("t={now:>5} ms  unresolved: {}", prog.join("  "));
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(DT_MS));
    }
}
