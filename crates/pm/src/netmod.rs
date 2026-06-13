//! Installable net modules: the transport-pumping loop every game wrote
//! by hand, hoisted into core. `NetServer::serve` / `NetClient::connect`
//! move the QUIC endpoint into a single net task (priority
//! [`NET_PRIO`], registered under `module_add` so
//! `module_remove("net_server" | "net_client")` is a clean shutdown)
//! that trades exclusively in plain data — games read and write singles
//! and never touch the transport.
//!
//! Server (`net.serve::<C>(pm, quic)`), where `C` is the input pod:
//! - `"net.peers"`  [`PeerEvents`] — who joined/left this tick
//! - `"net.cmds"`   [`Commands<C>`] — per-peer input queues. `pop` is
//!   the command-frame model (one per tick, bounded skip-ahead),
//!   `latest` is newest-wins; consuming records the seq that gets
//!   echoed back for prediction reconciliation
//! - `"net.events"` [`ServerEvents`] — reliable events received this tick
//! - `"net.out"`    [`ServerOutbox`] — queue reliable events to a peer
//!
//! Client (`net.connect::<C>(pm, quic, input_hz)`):
//! - `"net.status"`  [`NetStatus`] — peer / rtt / snapshot count / connected
//! - `"net.input"`   [`NetInput<C>`] — the game writes its current
//!   command; the module sends it at a fixed `input_hz` cadence,
//!   decoupled from the loop rate (prediction must step the same fixed
//!   dt as the server, whatever the display refresh is)
//! - `"net.sent"`    [`SentLog<C>`] — (seq, cmd) pairs sent this tick;
//!   feed them to a [`Predictor`](crate::Predictor)
//! - `"net.applied"` [`AppliedLog`] — snapshots applied this tick; each
//!   carries the server's input-seq echo, the reconciliation point
//! - `"net.events"`  [`ClientEvents`] — reliable events received this tick
//! - `"net.out"`     [`Outbox`] — queue reliable events to the server
//!   (held until the handshake completes, so it's safe to queue at init)
//!
//! Per-tick singles are cleared and refilled by the net task each tick:
//! read them from tasks at priority above [`NET_PRIO`] — they are valid
//! for the rest of that tick. A client whose connection errors or
//! closes quits the loop.

use std::collections::{HashMap, VecDeque};

use bytemuck::Pod;

use crate::kernel::Pm;
use crate::net::{Applied, NetClient, NetServer, Outbox};
use crate::transport::{QuicClient, QuicServer};

/// Priority of the net task both modules register. It runs first in the
/// tick (per net.rs: sending before the sim avoids relabeling freshly
/// stamped entries); register game tasks above it.
pub const NET_PRIO: f32 = 5.0;

/// Peers that joined or left this tick (server, `"net.peers"`).
#[derive(Default)]
pub struct PeerEvents {
    pub joined: Vec<u8>,
    pub left: Vec<u8>,
}

/// Per-peer input queues (server, `"net.cmds"`). The module pushes
/// decoded commands in; the sim consumes via `pop` or `latest`, which
/// also records the applied seq the module echoes back to that peer.
pub struct Commands<C: Pod> {
    queues: HashMap<u8, VecDeque<(u32, C)>>,
    applied: HashMap<u8, (u32, C)>,
}

impl<C: Pod> Default for Commands<C> {
    fn default() -> Self {
        Self { queues: HashMap::new(), applied: HashMap::new() }
    }
}

impl<C: Pod> Commands<C> {
    fn peer_add(&mut self, peer: u8) {
        self.queues.entry(peer).or_default();
    }

    fn peer_remove(&mut self, peer: u8) {
        self.queues.remove(&peer);
        self.applied.remove(&peer);
    }

    fn push(&mut self, peer: u8, seq: u32, cmd: C) {
        self.queues.entry(peer).or_default().push_back((seq, cmd));
    }

    /// Command-frame consumption: one input per call (= per sim tick),
    /// holding the last command when the queue runs dry and skipping
    /// ahead when it backs up (bounds queue-induced latency to ~2
    /// ticks). Matches one client prediction step per consumed input.
    pub fn pop(&mut self, peer: u8) -> C {
        let q = self.queues.entry(peer).or_default();
        while q.len() > 2 {
            let skipped = q.pop_front().unwrap();
            self.applied.insert(peer, skipped);
        }
        if let Some(next) = q.pop_front() {
            self.applied.insert(peer, next);
        }
        self.applied.get(&peer).map(|&(_, c)| c).unwrap_or_else(C::zeroed)
    }

    /// Newest-wins consumption: drain to the latest command and hold it.
    /// For games where input is continuous state (held movement keys),
    /// not per-tick command frames.
    pub fn latest(&mut self, peer: u8) -> C {
        let q = self.queues.entry(peer).or_default();
        if let Some(last) = q.drain(..).last() {
            self.applied.insert(peer, last);
        }
        self.applied.get(&peer).map(|&(_, c)| c).unwrap_or_else(C::zeroed)
    }

    /// Last consumed (peer, seq) pairs — echoed by the module so clients
    /// can reconcile predictions against exactly what was applied.
    fn applied_seqs(&self) -> impl Iterator<Item = (u8, u32)> + '_ {
        self.applied.iter().map(|(&p, &(seq, _))| (p, seq))
    }
}

/// Reliable events received this tick (server, `"net.events"`):
/// (peer, type, payload).
#[derive(Default)]
pub struct ServerEvents(pub Vec<(u8, u16, Vec<u8>)>);

/// Outgoing reliable events (server, `"net.out"`): any task queues,
/// the net task — sole owner of the socket — drains and sends.
#[derive(Default)]
pub struct ServerOutbox {
    events: Vec<(u8, u16, Vec<u8>)>,
}

impl ServerOutbox {
    pub fn send(&mut self, peer: u8, ty: u16, payload: &[u8]) {
        self.events.push((peer, ty, payload.to_vec()));
    }

    fn drain(&mut self) -> Vec<(u8, u16, Vec<u8>)> {
        std::mem::take(&mut self.events)
    }
}

/// Connection status (client, `"net.status"`).
#[derive(Default)]
pub struct NetStatus {
    pub peer: u8,
    pub rtt_ms: f32,
    pub snapshots: u32,
    pub connected: bool,
    /// Fraction (0..1) of the next fixed input step already accumulated
    /// this tick. The predictor advances in whole `1/input_hz` steps; at
    /// render rates not phase-locked to that, draw the local avatar at
    /// `step(pred.state(), current_cmd, alpha / input_hz)` — the render-
    /// clock interpolation that removes fixed-step beat ("smooth
    /// predict"). Extrapolating with the current command is exact: it is
    /// precisely where the next predict will land.
    pub input_alpha: f32,
}

/// The command the input layer wants sent (client, `"net.input"`) —
/// written by the game's input task, read by the net task at its fixed
/// send cadence.
pub struct NetInput<C: Pod>(pub C);

impl<C: Pod> Default for NetInput<C> {
    fn default() -> Self {
        Self(C::zeroed())
    }
}

/// (seq, cmd) pairs sent this tick (client, `"net.sent"`) — predict
/// each one: `pred.predict(seq, cmd, step)`.
pub struct SentLog<C: Pod>(pub Vec<(u32, C)>);

impl<C: Pod> Default for SentLog<C> {
    fn default() -> Self {
        Self(Vec::new())
    }
}

/// Snapshots applied this tick (client, `"net.applied"`) — reconcile
/// against each one's `input_seq` echo.
#[derive(Default)]
pub struct AppliedLog(pub Vec<Applied>);

/// Reliable events received this tick (client, `"net.events"`):
/// (type, payload).
#[derive(Default)]
pub struct ClientEvents(pub Vec<(u16, Vec<u8>)>);

impl NetServer {
    /// Install the server net module: moves `self` and the endpoint
    /// into a net task that pumps the wire and trades in the `"net.*"`
    /// singles (see module docs). Call after registering synced pools;
    /// `C` is the input pod clients send.
    pub fn serve<C: Pod + 'static>(self, pm: &mut Pm, mut quic: QuicServer) {
        let mut net = self;
        let _ = pm.module_add("net_server", |pm| {
            let peers = pm.single::<PeerEvents>("net.peers");
            let cmds = pm.single::<Commands<C>>("net.cmds");
            let events = pm.single::<ServerEvents>("net.events");
            let out = pm.single::<ServerOutbox>("net.out");
            pm.task_add("net", NET_PRIO, 0.0, move |pm| {
                quic.pump();
                {
                    let mut pe = peers.borrow_mut();
                    pe.joined.clear();
                    pe.left.clear();
                    let mut cs = cmds.borrow_mut();
                    for p in quic.joined_drain() {
                        net.peer_add(p);
                        cs.peer_add(p);
                        pe.joined.push(p);
                    }
                    for p in quic.left_drain() {
                        net.peer_remove(p);
                        cs.peer_remove(p);
                        pe.left.push(p);
                    }
                    for (p, seq, bytes) in quic.inputs_drain() {
                        if bytes.len() == size_of::<C>() {
                            cs.push(p, seq, bytemuck::pod_read_unaligned(&bytes));
                        }
                    }
                    // Echo what the sim consumed (last tick — it runs
                    // after this task); clients reconcile against this.
                    for (p, seq) in cs.applied_seqs() {
                        net.input_processed(p, seq);
                    }
                }
                for (p, tick) in quic.acks_drain() {
                    net.ack(p, tick);
                }
                {
                    let ev = &mut events.borrow_mut().0;
                    ev.clear();
                    ev.extend(quic.events_drain());
                }
                for (p, ty, payload) in out.borrow_mut().drain() {
                    quic.event_send(p, ty, &payload);
                }
                let plist: Vec<u8> = net.peers().collect();
                for p in plist {
                    let budget = quic.snapshot_budget(p);
                    if let Some(snap) = net.snapshot_budgeted(pm, p, budget) {
                        quic.snapshot_send(p, &snap);
                    }
                }
                net.prune(pm);
            });
        });
    }
}

impl NetClient {
    /// Install the client net module: moves `self` and the endpoint
    /// into a net task that pumps the wire and trades in the `"net.*"`
    /// singles (see module docs). `C` is the input pod; it is sent at a
    /// fixed `input_hz` cadence (match the server's sim rate — 60.0 for
    /// a 60 Hz server). Connection errors quit the loop.
    pub fn connect<C: Pod + 'static>(self, pm: &mut Pm, mut quic: QuicClient, input_hz: f32) {
        let net = self;
        let _ = pm.module_add("net_client", |pm| {
            let status = pm.single::<NetStatus>("net.status");
            let input = pm.single::<NetInput<C>>("net.input");
            let sent = pm.single::<SentLog<C>>("net.sent");
            let applied = pm.single::<AppliedLog>("net.applied");
            let events = pm.single::<ClientEvents>("net.events");
            let out = pm.single::<Outbox>("net.out");
            let mut accum = 0.0f32;
            pm.task_add("net", NET_PRIO, 0.0, move |pm| {
                quic.pump();
                if let Some(err) = quic.error() {
                    eprintln!("[net] disconnected: {err}");
                    pm.loop_quit();
                    return;
                }
                if quic.is_gone() {
                    eprintln!("[net] server closed the connection");
                    pm.loop_quit();
                    return;
                }
                sent.borrow_mut().0.clear();
                applied.borrow_mut().0.clear();
                {
                    let ev = &mut events.borrow_mut().0;
                    ev.clear();
                    ev.extend(quic.events_drain());
                }
                for snap in quic.snapshots_drain() {
                    let Ok(a) = net.apply(pm, &snap) else { continue };
                    quic.ack_send(a.tick);
                    applied.borrow_mut().0.push(a);
                    status.borrow_mut().snapshots += 1;
                }
                if let Some(peer) = quic.handshake_done() {
                    pm.local_peer = peer;
                    {
                        let mut st = status.borrow_mut();
                        st.peer = peer;
                        st.connected = true;
                    }
                    for (ty, payload) in out.borrow_mut().drain() {
                        quic.event_send(ty, &payload);
                    }
                    // Fixed-cadence input: the server consumes one per
                    // sim tick, and prediction must step the same fixed
                    // dt — regardless of what rate this loop runs at.
                    accum += pm.loop_dt();
                    let step = 1.0 / input_hz;
                    while accum >= step {
                        accum -= step;
                        let cmd = input.borrow().0;
                        let seq = quic.input_send(bytemuck::bytes_of(&cmd));
                        sent.borrow_mut().0.push((seq, cmd));
                    }
                    status.borrow_mut().input_alpha = accum * input_hz;
                }
                status.borrow_mut().rtt_ms = quic.rtt().as_secs_f32() * 1e3;
            });
        });
    }
}
