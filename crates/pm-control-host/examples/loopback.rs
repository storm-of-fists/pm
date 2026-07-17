//! Two nodes plus a monitor talking over real UDP loopback sockets.
//!
//! An HMI node commands a drive node; the drive ramps its actual speed
//! toward the command and raises a debounced overspeed fault, which the HMI
//! answers with an e-stop. Same `pm_group!` on both sides — `node()` decides
//! who publishes what.
//!
//! The third socket is a [`Monitor`]: it shares no code with the app, yet
//! discovers every published signal from the broadcast schema, subscribes,
//! and prints live values — the CLI-tool story.
//!
//! Each socket "broadcasts" by sending to the others (stand-in for a real
//! broadcast/multicast socket on a machine segment). Port 42514 is left
//! free for an external CLI.
//!
//! Run: `cargo run -p pm-control-host --example loopback`
//!
//! Or keep it running and attach the CLI from another terminal:
//! ```text
//! cargo run -p pm-control-host --example loopback -- --forever
//! cargo run -p pm-control-host --bin pm-mon -- --bind 127.0.0.1:42514 127.0.0.1:42511 127.0.0.1:42512
//! ```

use pm_control_core::*;
use pm_control_host::UdpSegmentPort;

pm_group! {
    struct App {
        cmd_speed: PmF32 = PmF32::new().range(0.0, 1.0).node("hmi"),
        estop: PmBool = PmBool::new().node("hmi"),
        act_speed: PmF32 = PmF32::new().node("drive"),
        overspeed_flt: PmFault = PmFault::new().on_delay(250).node("drive"),
        /// Whole-loop timing, published like any signal — pin
        /// `hmi.scan.avg_us` in pm-mon and watch the loop run.
        scan: PmProf = PmProf::new().node("hmi"),
    }
}

struct Node {
    app: App,
    net: NetworkManager,
    port: UdpSegmentPort,
}

fn node(local: &str, remote: &str, port: UdpSegmentPort) -> Node {
    let app = App::new();
    let mut net = NetworkManager::new();
    net.publish_health(); // link state + cycle timing, published as net.*
    net.add(&app);
    net.bind(local, &[remote]);
    Node { app, net, port }
}

const HMI: &str = "127.0.0.1:42511";
const DRV: &str = "127.0.0.1:42512";
const MON: &str = "127.0.0.1:42513";
const EXT: &str = "127.0.0.1:42514"; // an external pm-mon may listen here

fn main() {
    pm_control_host::install_us_clock(); // profs measure real time
    let forever = std::env::args().any(|a| a == "--forever");
    let mut hmi = node("hmi", "drive", UdpSegmentPort::bind(HMI, &[DRV, MON, EXT]));
    let mut drv = node("drive", "hmi", UdpSegmentPort::bind(DRV, &[HMI, MON, EXT]));
    let mut mon = Monitor::new();
    let mut mon_port = UdpSegmentPort::bind(MON, &[HMI, DRV]);

    const DT_MS: u64 = 50;
    for tick in 0u64.. {
        clock::advance(DT_MS);
        let t = tick * DT_MS;
        let phase = t % 4000; // in --forever mode the story replays

        // The HMI times its whole loop body (in this single-process demo
        // that includes the drive's share too — it's the mechanism on show).
        let scan_span = hmi.app.scan.measure();

        hmi.net.begin_cycle(&mut hmi.port);
        drv.net.begin_cycle(&mut drv.port);
        mon.poll(&mut mon_port);

        // --- HMI app: command a speed, e-stop on the drive's fault.
        if forever && phase == 0 {
            hmi.app.estop.set(false); // operator resets, story replays
        }
        hmi.app.cmd_speed.set(if phase < 1500 { 0.5 } else { 1.0 });
        if hmi.app.overspeed_flt.val() {
            hmi.app.estop.set(true);
        }

        // --- Drive app: ramp toward the command, flag overspeed.
        let target = if drv.app.estop.val() { 0.0 } else { drv.app.cmd_speed.val() };
        let act = drv.app.act_speed.val();
        drv.app.act_speed.set(act + (target - act) * 0.25);
        drv.app.overspeed_flt.set(drv.app.act_speed.val() > 0.9);

        hmi.net.end_cycle(&mut hmi.port);
        drv.net.end_cycle(&mut drv.port);
        drop(scan_span);

        if tick % 10 == 0 {
            // The monitor's view: app signals straight off the wire, plus a
            // taste of the health/profiling ones (net.*, scan.*).
            let mut seen = String::new();
            for p in &mon.nodes {
                for e in &p.signals {
                    let name = e.meta().name();
                    if !name.starts_with("net.") && !name.starts_with("scan.") {
                        seen += &format!("  {}.{}={}", p.node, name, e.value_text());
                    }
                }
            }
            let health = |node: &str, name: &str| {
                mon.signal(node, name).map(|s| s.value_text()).unwrap_or_else(|| "-".into())
            };
            println!(
                "t={t:>4}ms monitor sees:{seen}  [drive net.connected={} hmi scan {}µs]",
                health("drive", "net.connected"),
                health("hmi", "scan.last_us"),
            );
        }
        // Fast in demo mode; real-time pacing when a CLI is watching.
        std::thread::sleep(std::time::Duration::from_millis(if forever { DT_MS } else { 5 }));
        if !forever && tick >= 80 {
            break;
        }
    }

    // The whole story must have played out.
    assert!(hmi.net.status().connected && drv.net.status().connected);
    assert!(hmi.app.estop.val(), "HMI should have e-stopped on the fault");
    assert!(!hmi.app.overspeed_flt.val(), "fault clears once speed decays");
    assert!(hmi.app.act_speed.val() < 0.05, "drive should have wound down");

    // And the monitor must have watched it happen with zero app code.
    let signals: usize = mon.nodes.iter().map(|p| p.signals.len()).sum();
    assert_eq!(mon.nodes.len(), 2, "monitor discovered both nodes");
    // 4 app signals + hmi's scan prof (3) + 14 net.* health per node.
    assert_eq!(signals, 4 + 3 + 2 * 14, "monitor discovered every published signal");

    // The dogfood story held: both nodes report their own link as healthy,
    // and the profs measured real time.
    let val = |node: &str, name: &str| mon.signal(node, name).expect(name).value_text();
    assert_eq!(val("hmi", "net.connected"), "1");
    assert_eq!(val("drive", "net.connected"), "1");
    assert!(hmi.app.scan.avg_us.val() > 0.0, "scan prof measured the loop");
    let act = mon.signal("drive", "act_speed").expect("monitor tracked drive.act_speed");
    assert_eq!(act.value_text(), "0.000", "monitor saw the wind-down");

    // Faults self-identify in the schema: the monitor stamped the overspeed
    // fault when it rose and still lists it (cleared) after it dropped.
    let flt = mon
        .node("drive")
        .and_then(|p| p.faults.iter().find(|f| f.sig.meta().name() == "overspeed_flt"))
        .expect("monitor tracked drive.overspeed_flt");
    assert_eq!(flt.sig.wire_type(), WireType::Fault);
    assert!(flt.stamp_ms > 0, "monitor stamped the fault's rise");
    assert!(!flt.active(), "fault dropped after the e-stop");

    println!("\nloopback demo ok: connected, faulted, e-stopped, recovered — and monitored.");
}
