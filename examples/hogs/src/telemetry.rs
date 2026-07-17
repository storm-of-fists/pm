//! The hogs telemetry node: pm-control signals broadcast from the PLAYER
//! CLIENT so a `pm-mon` (TUI) or `pm-watch` (headless) on the segment can
//! watch the session live — and TUNE it: the link sim (`lag_ms`/`loss`),
//! and the day length are writable knobs (Ctrl-U in pm-mon, `set` in
//! pm-watch); their initial values come from the CLI flags.
//!
//! ONE node per process, and it lives on the client thread: pm-control
//! signals are `Rc` (not `Send`) and the scan clock is a process-global
//! frozen-per-scan cell — two nodes on two threads is UB by contract.
//! The server facts worth watching (wave, horde count, points) already
//! replicate into the client through the `Hunt` single, so the client
//! node can publish them without touching the server thread.
//!
//! Wire layout: the node binds `TELE_BIND` and unicasts to the monitor
//! address (`mon=IP:PORT` arg; default localhost). pm-mon/pm-watch bind
//! that address and list us as a peer — see docs/journey.md.

use std::net::{SocketAddr, UdpSocket};
use std::sync::OnceLock;
use std::time::Instant;

use pm_control_core::signal::Stamp;
use pm_control_core::{
    NetworkManager, PmF32, PmFault, PmProf, SegmentPort, clock, pm_group,
};

use crate::bot_client::ClientWorld;
use crate::common::*;

/// Where the node's own socket binds (the monitor sends unlocks here).
/// All interfaces, not loopback: the game on Windows must reach a
/// monitor in WSL, and a loopback-bound socket can neither send off-box
/// nor hear the monitor's subscribe/unlock leases coming back.
pub const TELE_BIND: &str = "0.0.0.0:42501";
/// Default monitor address (pm-watch / pm-mon bind this).
pub const TELE_MON: &str = "127.0.0.1:42500";

pm_group! {
    struct Tele {
        // --- knobs (unlock-writable from the monitor) ---------------
        /// One-way simulated link delay, live-applied via LinkTune.
        link_lag_ms: PmF32 = PmF32::new().range(0.0, 400.0),
        /// Simulated drop fraction, live-applied via LinkTune.
        link_loss: PmF32 = PmF32::new().range(0.0, 0.5),
        /// Day-night cycle length (render-side, cosmetic).
        day_secs: PmF32 = PmF32::new().range(10.0, 3600.0),
        // --- metrics -------------------------------------------------
        /// Interp delay in force (creation-frozen; report-only).
        interp_ms: PmF32,
        rtt_ms: PmF32,
        corrections: PmF32,
        speed: PmF32,
        wave: PmF32,
        hogs_alive: PmF32,
        points: PmF32,
        /// Frame time (last/avg/max µs) off the render-thread loop dt.
        frame: PmProf,
        // --- faults --------------------------------------------------
        /// Frame ran past ~2.5x the 60 Hz budget for 250 ms straight.
        overrun_flt: PmFault = PmFault::new()
            .describe("client frame overrun (>40 ms sustained)")
            .on_delay(250),
    }
}

/// `SegmentPort` over a std UDP socket — inlined here so the game only
/// depends on the `no_std` core, not the host crate (whose manifest
/// carries the pm-mon TUI's deps).
struct UdpPort {
    sock: UdpSocket,
    mon: SocketAddr,
}

impl SegmentPort for UdpPort {
    fn recv(&mut self, buf: &mut [u8]) -> Option<usize> {
        self.sock.recv(buf).ok()
    }
    fn send(&mut self, data: &[u8]) {
        let _ = self.sock.send_to(data, self.mon);
    }
}

/// Install the fine µs clock once (PmProf reads it; without it every
/// profile reports 0).
fn install_us_clock() {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    EPOCH.get_or_init(Instant::now);
    clock::install_us(|| {
        EPOCH
            .get()
            .map_or(0, |t0| t0.elapsed().as_micros() as u64)
    });
}

/// Register the telemetry task on the player client. `mon` is the
/// monitor's address; knob signals are seeded from the CLI flags and
/// their changes flow back into the game (LinkTune single for the
/// transport, the Tune single for day length).
pub fn install(pm: &mut pm::PmClient, w: &ClientWorld, flags: &Flags) {
    let Ok(sock) = UdpSocket::bind(TELE_BIND) else {
        // Second client on this box (bots don't run telemetry, but a
        // second player window would collide): run silent, not dead.
        eprintln!("[tele] {TELE_BIND} taken — telemetry off");
        return;
    };
    sock.set_nonblocking(true).expect("tele nonblock");
    let Ok(mon) = flags.mon.parse::<SocketAddr>() else {
        eprintln!("[tele] bad mon= address '{}' — telemetry off", flags.mon);
        return;
    };
    let mut port = UdpPort { sock, mon };

    install_us_clock();
    let tele = Tele::new().node("hogs");
    // Seed knobs from the flags (signals are born locked = app-owned,
    // so plain set() works and clamps to the declared ranges).
    tele.link_lag_ms.set(flags.link.0);
    tele.link_loss.set(flags.link.1);
    tele.day_secs.set(flags.day);
    tele.interp_ms.set(flags.interp_ms);

    let mut net_mgr = NetworkManager::new();
    net_mgr.publish_health();
    net_mgr.add(&tele);
    net_mgr.bind("hogs", &[]);

    let tune = pm.link_tune();
    let day = pm.single::<Tune>("hogs.tune");
    day.get_mut().day_secs = flags.day;
    let net = pm.net();
    let pred = w.pred.clone();
    let pred_heli = w.pred_heli.clone();
    let hunt = w.hunt.clone();
    // Last knob values we applied to the game (change-detect).
    let mut applied = (flags.link.0, flags.link.1, flags.day);
    let mut clock_ms = 0.0f64;

    pm.task_add("telemetry", 95.0, 0.0, move |pm| {
        // The scan clock: one advance per tick, this node is the only
        // clock user in the process.
        clock_ms += pm.loop_dt() as f64 * 1000.0;
        clock::set(clock_ms as u64);
        net_mgr.begin_cycle(&mut port);

        // Knobs → game. A monitor write lands in the same value cell
        // (the signal unlocks while commanded); we just diff values.
        let knobs = (
            tele.link_lag_ms.val(),
            tele.link_loss.val(),
            tele.day_secs.val(),
        );
        if (knobs.0, knobs.1) != (applied.0, applied.1) {
            let mut t = tune.get_mut();
            t.lag_ms = knobs.0;
            t.loss = knobs.1;
            t.seq = t.seq.wrapping_add(1);
        }
        if knobs.2 != applied.2 {
            day.get_mut().day_secs = knobs.2.max(10.0);
        }
        applied = knobs;

        // Game → metrics.
        let dt = pm.loop_dt();
        tele.frame.record_us((dt * 1e6) as u64);
        tele.overrun_flt.set(dt > 0.040);
        tele.rtt_ms.set(net.rtt_ms());
        tele.corrections
            .set((pred.get().corrections + pred_heli.get().corrections) as f32);
        let speed = pred
            .get()
            .state()
            .map(|t| t.speed().abs())
            .or_else(|| pred_heli.get().state().map(|h| h.body.vel.len()))
            .unwrap_or(0.0);
        tele.speed.set(speed * 3.6 / 1.6); // mph, same as the title bar
        let sb = hunt.get();
        tele.wave.set(sb.wave as f32);
        tele.hogs_alive.set(sb.alive as f32);
        tele.points.set(sb.points);

        net_mgr.end_cycle(&mut port);
    });
    eprintln!("[tele] node 'hogs' up: {TELE_BIND} -> {mon}");
}
