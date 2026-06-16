//! Installable net modules: the transport-pumping loop every game wrote
//! by hand, hoisted into core. `NetServer::serve` / `NetClient::connect`
//! move the QUIC endpoint into a single net task (priority [`NET_PRIO`])
//! that trades exclusively in plain data — games read and write singles
//! and never touch the transport.
//!
//! Server (`Pm::server(addr)`, `pm.sync_pool…`, `pm.run::<C>()`), `C` the input pod:
//! - `"net.peers"`  [`PeerEvents`] — who joined/left this tick
//! - `"net.cmds"`   [`Commands<C>`] — per-peer input queues. `pop` is
//!   the command-frame model (one per tick, bounded skip-ahead),
//!   `latest` is newest-wins; consuming records the seq that gets
//!   echoed back for prediction reconciliation
//! - `"net.events"` [`ServerEvents`] — reliable events received this tick
//! - `"net.out"`    [`ServerOutbox`] — queue reliable events to a peer
//!
//! Client (`Pm::client(addr, hz)`, `pm.sync_pool…`, `pm.run::<C>()`):
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

use crate::id::Id;
use crate::kernel::{PoolHandle, Pm, SingleHandle};
use crate::net::{Applied, NetClient, NetServer, Outbox, SyncSet};
use crate::predict::Predictor;
use crate::smooth::{InterpBuffer, pool_interp};
use crate::transport::{QuicClient, QuicServer};

/// Priority of the net task both modules register. It runs first in the
/// tick (per net.rs: sending before the sim avoids relabeling freshly
/// stamped entries); register game tasks above it.
pub const NET_PRIO: f32 = 5.0;

/// Artificial link delay/loss from `PM_LAG_MS` (milliseconds) and
/// `PM_LOSS` (0..1 drop fraction) — the simulation knob clients used to
/// wire by hand around the QUIC endpoint, now read by `serve`/`connect`.
/// `None` when both are unset or zero.
fn link_lag_from_env() -> Option<(std::time::Duration, f32)> {
    let env = |k: &str| {
        std::env::var(k)
            .ok()
            .and_then(|v| v.parse::<f32>().ok())
            .unwrap_or(0.0)
    };
    let lag_ms = env("PM_LAG_MS");
    let loss = env("PM_LOSS");
    (lag_ms > 0.0 || loss > 0.0)
        .then(|| (std::time::Duration::from_secs_f32(lag_ms / 1000.0), loss))
}

/// Networking role + endpoint, set once at construction
/// (`Pm::server`/`Pm::client`) and consumed by [`Pm::run`]. Stored rather
/// than acted on immediately because the QUIC handshake needs the full
/// pool schema, which isn't complete until every `sync_pool` has run.
pub(crate) enum NetRole {
    /// Single-player / no networking (the [`Pm::new`] default).
    Local,
    /// Authoritative server bound to `addr`.
    Server { addr: String },
    /// Client connecting to `addr`, sending input at `input_hz`.
    Client { addr: String, input_hz: f32 },
}

/// Networking is part of the kernel: pick a role at construction
/// ([`Pm::server`] / [`Pm::client`]), register pools with [`Pm::sync_pool`],
/// then [`Pm::run`]. The transport pumps in a single task bound lazily by
/// `run`; games only ever touch the `"net.*"` singles.
impl Pm {
    /// Construct an authoritative server that will bind `addr` when
    /// [`run`](Pm::run) is called. pm owns the transport end to end — games
    /// never touch `Quic*`.
    pub fn server(addr: &str) -> Self {
        let mut pm = Pm::new();
        pm.net_role = NetRole::Server {
            addr: addr.to_string(),
        };
        pm
    }

    /// Construct a client that will connect to `addr` when [`run`](Pm::run)
    /// is called, sending its input pod at `input_hz` (match the server's
    /// sim rate — 60.0 for a 60 Hz server).
    pub fn client(addr: &str, input_hz: f32) -> Self {
        let mut pm = Pm::new();
        pm.net_role = NetRole::Client {
            addr: addr.to_string(),
            input_hz,
        };
        pm
    }

    /// Bind/connect the chosen role (the schema is complete by now) and run
    /// the loop. `C` is the input pod — the one type that can't ride the
    /// constructor without making the whole kernel generic, so it lands
    /// here. Honors `PM_LAG_MS`/`PM_LOSS` for link simulation. Returns once
    /// the loop quits; `Err` if the bind/connect fails. Local games use
    /// [`Pm::loop_run`] instead.
    pub fn run<C: Pod + 'static>(&mut self) -> std::io::Result<()> {
        match std::mem::replace(&mut self.net_role, NetRole::Local) {
            NetRole::Server { addr } => self.serve::<C>(&addr)?,
            NetRole::Client { addr, input_hz } => self.connect::<C>(&addr, input_hz)?,
            NetRole::Local => panic!(
                "Pm::run::<C>() needs a net role — build with Pm::server/Pm::client, \
                 or call Pm::loop_run() for a local game"
            ),
        }
        self.loop_run();
        Ok(())
    }

    /// Create a pool and register it for replication, returning the handle —
    /// the one-call replacement for `pool()` + a separate sync step. The
    /// pool's name is its wire identity (hashed; see `pool_key`), so server
    /// and client may register in any order.
    pub fn sync_pool<T: Pod + 'static>(&mut self, name: &str) -> PoolHandle<T> {
        let pool = self.pool::<T>(name);
        self.sync(&pool);
        pool
    }

    /// Create a singleton and register it for replication (the owning side
    /// only — a replica reads it with `pool()`; see [`Pm::single`]).
    pub fn sync_single<T: Pod + Default + 'static>(&mut self, name: &str) -> SingleHandle<T> {
        let single = self.single::<T>(name);
        self.sync(single.pool());
        single
    }

    /// Register an existing `pool` for replication. Internal: `sync_pool` /
    /// `sync_single` are the public surface.
    fn sync<T: Pod + 'static>(&mut self, pool: &PoolHandle<T>) {
        let name = pool.name().to_string();
        self.single::<SyncSet>("net.sync")
            .get_mut()
            .pool_sync(&name, pool);
    }

    /// Schema (pool name + value size) of everything registered for sync,
    /// for the QUIC handshake. Order-independent: the transport sorts by
    /// name, and replication keys sections by name hash.
    fn net_schema(&mut self) -> Vec<(String, usize)> {
        self.single::<SyncSet>("net.sync").get().schema()
    }

    /// Bind the server transport and install the net task. Called by `run`.
    fn serve<C: Pod + 'static>(&mut self, addr: &str) -> std::io::Result<()> {
        let mut quic = QuicServer::bind(addr, &self.net_schema())?;
        if let Some((delay, loss)) = link_lag_from_env() {
            quic.link_lag_set(delay, loss);
        }
        let sync = std::mem::take(&mut *self.single::<SyncSet>("net.sync").get_mut());
        self.removal_hold_set(true);
        NetServer::with_sync(sync).serve::<C>(self, quic);
        Ok(())
    }

    /// Connect the client transport and install the net task. Called by `run`.
    fn connect<C: Pod + 'static>(&mut self, addr: &str, input_hz: f32) -> std::io::Result<()> {
        let mut quic = QuicClient::connect(addr, &self.net_schema())?;
        if let Some((delay, loss)) = link_lag_from_env() {
            quic.link_lag_set(delay, loss);
        }
        let sync = std::mem::take(&mut *self.single::<SyncSet>("net.sync").get_mut());
        NetClient::with_sync(sync).connect::<C>(self, quic, input_hz);
        Ok(())
    }

    /// This peer's controlled entity, as the server marked it — sugar over
    /// [`NetStatus::avatar`]. `None` until the first snapshot that carries
    /// one (client-side; always `None` on the server).
    pub fn mine(&mut self) -> Option<Id> {
        self.single::<NetStatus>("net.status").get().avatar
    }

    /// Client-side prediction for the local avatar ([`Pm::mine`]), wired
    /// straight to the net module: this installs the task every predicted
    /// game wrote by hand. Each tick it reconciles the [`Predictor`] against
    /// every `"net.applied"` snapshot's input-seq echo, replays this tick's
    /// `"net.sent"` inputs, then writes the smooth-predicted avatar into the
    /// pool's draw sibling (`"<name>.draw"`, the one [`Pm::interp`] fills) —
    /// extrapolated by the in-flight input fraction
    /// ([`NetStatus::input_alpha`]) so the render clock doesn't beat against
    /// the fixed predict step.
    ///
    /// `step` is THE shared integration (the same one the server runs —
    /// determinism is what makes reconciliation byte-exact); `fixed_dt` is
    /// the server's sim step. Pair with [`Pm::interp`] on the same pool for
    /// the remote entities (it returns the draw pool to render). Returns the
    /// predictor single so rendering can read `state()` / `corrections`.
    pub fn predict_pool<S: Pod + 'static, C: Pod + 'static>(
        &mut self,
        auth: &PoolHandle<S>,
        step: impl Fn(&mut S, C, f32) + 'static,
        err: impl Fn(&S, &S) -> f32 + 'static,
        tolerance: f32,
        fixed_dt: f32,
    ) -> SingleHandle<Predictor<S, C>> {
        let draw = self.pool::<S>(&format!("{}.draw", auth.name()));
        let pred = self.single::<Predictor<S, C>>("net.pred");
        let status = self.single::<NetStatus>("net.status");
        let input = self.single::<NetInput<C>>("net.input");
        let applied = self.single::<AppliedLog>("net.applied");
        let sent = self.single::<SentLog<C>>("net.sent");
        let auth = auth.clone();
        self.task_add("net.predict", NET_PRIO + 2.0, 0.0, {
            let pred = pred.clone();
            move |_pm| {
                let Some(mine) = status.get().avatar else {
                    return;
                };
                for a in &applied.get().0 {
                    let Some(auth_s) = auth.get_id(mine).map(|r| *r) else {
                        continue;
                    };
                    pred.get_mut().reconcile(
                        auth_s,
                        a.input_seq,
                        |s, c| step(s, c, fixed_dt),
                        |a, b| err(a, b),
                        tolerance,
                    );
                }
                for &(seq, cmd) in &sent.get().0 {
                    pred.get_mut().predict(seq, cmd, |s, c| step(s, c, fixed_dt));
                }
                // Smooth-predicted local avatar into draw, extrapolated by
                // the in-flight input fraction. Guard on the entity still
                // existing in `auth` so a despawn can't leave a predicted
                // ghost behind.
                if auth.get_id(mine).is_some()
                    && let Some(mut s) = pred.get().state()
                {
                    let alpha = status.get().input_alpha.min(1.0);
                    step(&mut s, input.get().0, alpha * fixed_dt);
                    draw.get_mut().add(mine, s);
                }
            }
        });
        pred
    }

    /// Snapshot-interpolation presentation for a replicated pool — the
    /// per-pool sync modifier the [`pool_interp`] note promised. Installs a
    /// task that eases every entity in `auth` into a draw sibling pool
    /// (`"<name>.draw"`) ~`delay` seconds behind the newest authoritative
    /// sample (see [`InterpBuffer`]), via the game's field-aware `lerp`, and
    /// returns that draw pool — the one rendering should read. Runs before
    /// [`Pm::predict_pool`] on the same pool, so the local avatar's
    /// interpolated value is harmlessly overwritten by the predicted one;
    /// everyone else stays smooth.
    ///
    /// `delay`/`extrap_max` are seconds; a snapshot interval or two of delay
    /// is the usual start, with a small `extrap_max` to ride loss bursts.
    pub fn interp_pool<T: Pod + PartialEq + 'static>(
        &mut self,
        auth: &PoolHandle<T>,
        lerp: impl Fn(&T, &T, f32) -> T + 'static,
        delay: f64,
        extrap_max: f64,
    ) -> PoolHandle<T> {
        let draw = self.pool::<T>(&format!("{}.draw", auth.name()));
        let auth = auth.clone();
        let ret = draw.clone();
        let mut buf = InterpBuffer::<T>::new(delay);
        buf.extrap_max = extrap_max;
        let mut clock = 0.0f64;
        self.task_add("net.interp", NET_PRIO + 1.0, 0.0, move |pm| {
            clock += pm.loop_dt() as f64;
            pool_interp(&auth, &draw, &mut buf, clock, &lerp);
        });
        ret
    }
}

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
        Self {
            queues: HashMap::new(),
            applied: HashMap::new(),
        }
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
        self.applied
            .get(&peer)
            .map(|&(_, c)| c)
            .unwrap_or_else(C::zeroed)
    }

    /// Newest-wins consumption: drain to the latest command and hold it.
    /// For games where input is continuous state (held movement keys),
    /// not per-tick command frames.
    pub fn latest(&mut self, peer: u8) -> C {
        let q = self.queues.entry(peer).or_default();
        if let Some(last) = q.drain(..).last() {
            self.applied.insert(peer, last);
        }
        self.applied
            .get(&peer)
            .map(|&(_, c)| c)
            .unwrap_or_else(C::zeroed)
    }

    /// Last consumed (peer, seq) pairs — echoed by the module so clients
    /// can reconcile predictions against exactly what was applied.
    fn applied_seqs(&self) -> impl Iterator<Item = (u8, u32)> + '_ {
        self.applied.iter().map(|(&p, &(seq, _))| (p, seq))
    }
}

/// Reliable events received this tick (server, `"net.events"`):
/// (peer, type, payload).
// TODO(roadmap): typed event queues — sugar over these raw
// (type, payload) event singles so games push/pop concrete event enums
// instead of hand-encoding bytes. Reliable events stay for true
// must-see instants; everything a late joiner must know stays pool
// state (TTL'd if it's a fact that fades).
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

/// Per-peer controlled entity (server, `"net.own"`). The game records each
/// peer's avatar here on spawn (and clears it on despawn/leave); the net
/// task ships it in every snapshot header, so the client always knows which
/// replicated entity is its own — the built-in replacement for a hand-rolled
/// "here's your id" reliable event, and robust to packet loss (it rides
/// every snapshot, not one). Doubles as the server's peer→entity lookup.
#[derive(Default)]
pub struct ServerOwn(pub HashMap<u8, Id>);

impl ServerOwn {
    pub fn set(&mut self, peer: u8, id: Id) {
        self.0.insert(peer, id);
    }

    pub fn clear(&mut self, peer: u8) {
        self.0.remove(&peer);
    }

    pub fn get(&self, peer: u8) -> Option<Id> {
        self.0.get(&peer).copied()
    }
}

/// Connection status (client, `"net.status"`).
#[derive(Default)]
pub struct NetStatus {
    pub peer: u8,
    pub rtt_ms: f32,
    pub snapshots: u32,
    pub connected: bool,
    /// This peer's controlled entity, as the server marked it (see
    /// [`ServerOwn`]). `None` until the first snapshot that carries one.
    /// [`Pm::mine`] is the sugar most game code reads.
    pub avatar: Option<Id>,
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
        let peers = pm.single::<PeerEvents>("net.peers");
        let cmds = pm.single::<Commands<C>>("net.cmds");
        let events = pm.single::<ServerEvents>("net.events");
        let out = pm.single::<ServerOutbox>("net.out");
        let own = pm.single::<ServerOwn>("net.own");
        pm.task_add("net", NET_PRIO, 0.0, move |pm| {
            quic.pump();
            {
                let mut pe = peers.get_mut();
                pe.joined.clear();
                pe.left.clear();
                let mut cs = cmds.get_mut();
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
                let ev = &mut events.get_mut().0;
                ev.clear();
                ev.extend(quic.events_drain());
            }
            for (p, ty, payload) in out.get_mut().drain() {
                quic.event_send(p, ty, &payload);
            }
            {
                let own = own.get();
                for (&p, &id) in &own.0 {
                    net.set_avatar(p, id.0);
                }
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
            sent.get_mut().0.clear();
            applied.get_mut().0.clear();
            {
                let ev = &mut events.get_mut().0;
                ev.clear();
                ev.extend(quic.events_drain());
            }
            for snap in quic.snapshots_drain() {
                let Ok(a) = net.apply(pm, &snap) else {
                    continue;
                };
                quic.ack_send(a.tick);
                if a.avatar != 0 {
                    status.get_mut().avatar = Some(Id(a.avatar));
                }
                applied.get_mut().0.push(a);
                status.get_mut().snapshots += 1;
            }
            if let Some(peer) = quic.handshake_done() {
                pm.local_peer = peer;
                {
                    let mut st = status.get_mut();
                    st.peer = peer;
                    st.connected = true;
                }
                for (ty, payload) in out.get_mut().drain() {
                    quic.event_send(ty, &payload);
                }
                // Fixed-cadence input: the server consumes one per
                // sim tick, and prediction must step the same fixed
                // dt — regardless of what rate this loop runs at.
                accum += pm.loop_dt();
                let step = 1.0 / input_hz;
                while accum >= step {
                    accum -= step;
                    let cmd = input.get().0;
                    let seq = quic.input_send(bytemuck::bytes_of(&cmd));
                    sent.get_mut().0.push((seq, cmd));
                }
                status.get_mut().input_alpha = accum * input_hz;
            }
            status.get_mut().rtt_ms = quic.rtt().as_secs_f32() * 1e3;
        });
    }
}
