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
//! - `"net.events"` [`ServerEvents`] — reliable client→server events received
//!   this tick (read via [`EventRx`])
//!
//! Client (`Pm::client(addr, hz)`, `pm.sync_pool…`, `pm.run::<C>()`):
//! - status & input — through the [`ClientNet`] handle ([`PmClient::net`]),
//!   cloned into tasks: status reads ([`mine`](ClientNet::mine),
//!   [`rtt_ms`](ClientNet::rtt_ms), [`peer`](ClientNet::peer),
//!   [`snapshots`](ClientNet::snapshots), [`connected`](ClientNet::connected))
//!   and the input setter ([`input`](ClientNet::input)) — the module sends
//!   the input pod at a fixed `input_hz` cadence, decoupled from the loop
//!   rate (prediction must step the same fixed dt as the server, whatever
//!   the display refresh is)
//! - `"net.sent"`    [`SentLog<C>`] — (seq, cmd) pairs sent this tick;
//!   feed them to a [`Predictor`](crate::Predictor)
//! - `"net.applied"` [`AppliedLog`] — snapshots applied this tick; each
//!   carries the server's input-seq echo, the reconciliation point
//! - events — reliable client→server events via [`EventTx`] (queue with
//!   [`PmClient::event`]); held until the handshake completes. There is no
//!   server→client event channel: server→client facts are state.
//!
//! Per-tick singles are cleared and refilled by the net task each tick:
//! read them from tasks at priority above [`NET_PRIO`] — they are valid
//! for the rest of that tick. A client whose connection errors or
//! closes quits the loop.

use std::collections::{HashMap, VecDeque};
use std::marker::PhantomData;
use std::ops::{Deref, DerefMut};

use bytemuck::Pod;

use crate::id::Id;
use crate::kernel::{PoolHandle, Pm, SingleHandle};
use crate::net::{Applied, NetClient, NetServer, Outbox, SyncSet, pool_key};
use crate::predict::Predictor;
use crate::smooth::{InterpBuffer, pool_interp};
use crate::transport::{EVENT_USER_BASE, QuicClient, QuicServer};

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

/// A networked [`Pm`] in the **server** role: owns the simulation, binds the
/// endpoint at [`run`](PmServer::run), and carries the server-only surface
/// ([`sync_single`](PmServer::sync_single), [`event`](PmServer::event)
/// receive). Derefs to [`Pm`] for everything shared — pools, singles, tasks.
/// Build with [`Pm::server`].
pub struct PmServer {
    pm: Pm,
    addr: String,
}

/// A networked [`Pm`] in the **client** role: mirrors server state and carries
/// the client-only surface ([`predict_pool`](PmClient::predict_pool),
/// [`interp_pool`](PmClient::interp_pool), [`event`](PmClient::event) send).
/// Derefs to [`Pm`] for everything shared. Build with [`Pm::client`].
pub struct PmClient {
    pm: Pm,
    addr: String,
    input_hz: f32,
}

impl Deref for PmServer {
    type Target = Pm;
    fn deref(&self) -> &Pm {
        &self.pm
    }
}
impl DerefMut for PmServer {
    fn deref_mut(&mut self) -> &mut Pm {
        &mut self.pm
    }
}
impl Deref for PmClient {
    type Target = Pm;
    fn deref(&self) -> &Pm {
        &self.pm
    }
}
impl DerefMut for PmClient {
    fn deref_mut(&mut self) -> &mut Pm {
        &mut self.pm
    }
}

/// Networking is part of the kernel: pick a role at construction
/// ([`Pm::server`] / [`Pm::client`]), register pools with [`Pm::sync_pool`],
/// then `run`. The transport pumps in a single task bound lazily by `run`
/// (the schema must be complete first); games only ever touch the `"net.*"`
/// singles.
impl Pm {
    /// Construct an authoritative [`PmServer`] that binds `addr` at
    /// [`run`](PmServer::run). pm owns the transport end to end — games never
    /// touch `Quic*`.
    pub fn server(addr: &str) -> PmServer {
        PmServer {
            pm: Pm::new(),
            addr: addr.to_string(),
        }
    }

    /// Construct a [`PmClient`] that connects to `addr` at
    /// [`run`](PmClient::run), sending its input pod at `input_hz` (match the
    /// server's sim rate — 60.0 for a 60 Hz server).
    pub fn client(addr: &str, input_hz: f32) -> PmClient {
        PmClient {
            pm: Pm::new(),
            addr: addr.to_string(),
            input_hz,
        }
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

    /// Register an existing `pool` for replication. Internal: `sync_pool`
    /// (shared) and [`PmServer::sync_single`] are the public surface.
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

}

/// The client role's in-task surface: status reads and the input setter,
/// as an ordinary cloneable handle. Obtained from [`PmClient::net`] — only
/// a client can construct one, so a task that captures it is a client task
/// by construction; there is nothing to call (and nothing to answer with a
/// harmless default) on a server. Like every pool/single handle: fetch at
/// init, clone into the closures that need it. Reads go straight to the
/// captured singles — no per-call store lookup.
pub struct ClientNet<C: Pod> {
    status: SingleHandle<NetStatus>,
    input: SingleHandle<NetInput<C>>,
}

impl<C: Pod> Clone for ClientNet<C> {
    fn clone(&self) -> Self {
        Self {
            status: self.status.clone(),
            input: self.input.clone(),
        }
    }
}

impl<C: Pod + 'static> ClientNet<C> {
    /// This peer's controlled entity, as the server marked it — `None`
    /// until the first snapshot that carries one (see [`ServerOwn`]).
    pub fn mine(&self) -> Option<Id> {
        self.status.get().avatar
    }

    /// Set the continuous input the net module sends each input-cadence
    /// tick. `C` is the game's input pod; the net task ships it unreliably,
    /// newest-wins. Call from the input task.
    //
    // TODO(input-map): a declarative device→action layer (Unreal Enhanced
    // Input style) should FILL this pod from named axes/actions, so games
    // stop hand-building it from raw scancodes. Lives in pm_sdl; this setter
    // is the seam it writes through. See the player-client input task.
    pub fn input(&self, cmd: C) {
        self.input.get_mut().0 = cmd;
    }

    /// Round-trip time to the server in milliseconds (0 until connected).
    pub fn rtt_ms(&self) -> f32 {
        self.status.get().rtt_ms
    }

    /// Count of snapshots applied so far — a liveness/throughput readout
    /// for HUDs.
    pub fn snapshots(&self) -> u32 {
        self.status.get().snapshots
    }

    /// This peer's id, as assigned at handshake (0 before connected).
    pub fn peer(&self) -> u8 {
        self.status.get().peer
    }

    /// Whether the handshake has completed.
    pub fn connected(&self) -> bool {
        self.status.get().connected
    }
}

/// Client-only setup surface: the in-task status/input handle, local-avatar
/// prediction, remote interpolation, the connect-and-run entry, and the
/// typed event sender.
impl PmClient {
    /// The client's in-task surface as a handle: fetch once at init, clone
    /// into the tasks that read status ([`mine`](ClientNet::mine),
    /// [`rtt_ms`](ClientNet::rtt_ms), …) or set the input pod
    /// ([`input`](ClientNet::input)). `C` must be the same input pod later
    /// passed to [`run`](PmClient::run) (a mismatch panics at the `"net.input"`
    /// single, naming both types).
    pub fn net<C: Pod + 'static>(&mut self) -> ClientNet<C> {
        ClientNet {
            status: self.pm.single::<NetStatus>("net.status"),
            input: self.pm.single::<NetInput<C>>("net.input"),
        }
    }

    /// Client-side prediction for the local avatar ([`ClientNet::mine`]), wired
    /// straight to the net module: this installs the task every predicted
    /// game wrote by hand. Each tick it reconciles the [`Predictor`] against
    /// every `"net.applied"` snapshot's input-seq echo, replays this tick's
    /// `"net.sent"` inputs, then writes the smooth-predicted avatar into the
    /// pool's draw sibling (`"<name>.draw"`, the one [`interp_pool`](PmClient::interp_pool) fills) —
    /// extrapolated by the in-flight input fraction (the net status'
    /// `input_alpha`) so the render clock doesn't beat against the fixed
    /// predict step.
    ///
    /// `step` is THE shared integration (the same one the server runs —
    /// determinism is what makes reconciliation byte-exact); `fixed_dt` is
    /// the server's sim step. Pair with [`interp_pool`](PmClient::interp_pool) on the same pool for
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
    /// [`predict_pool`](PmClient::predict_pool) on the same pool, so the local avatar's
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

    /// Connect to the server (the schema is complete now) and run the loop.
    /// `C` is the input pod, sent at the constructor's `input_hz`. Honors
    /// `PM_LAG_MS`/`PM_LOSS`; returns when the loop quits, `Err` if the
    /// connection fails.
    pub fn run<C: Pod + 'static>(self) -> std::io::Result<()> {
        let PmClient {
            mut pm,
            addr,
            input_hz,
        } = self;
        pm.connect::<C>(&addr, input_hz)?;
        pm.loop_run();
        Ok(())
    }

    /// Register a typed reliable event channel and return its **sender**.
    /// Events are one-way client→server: discrete, must-arrive intents the
    /// client originates ("respawn", "ready", "chat") — the things that don't
    /// fit the continuous, lossy input pod. (Server→client facts stay state:
    /// a synced pool, TTL'd if they fade.) The returned [`EventTx`] queues an
    /// event the net task ships reliably once connected.
    pub fn event<E: Pod + 'static>(&mut self, name: &str) -> EventTx<E> {
        let tag = event_register(&mut self.pm, name);
        EventTx {
            out: self.pm.single::<Outbox>("net.out"),
            tag,
            _marker: PhantomData,
        }
    }
}

impl PmServer {
    /// Bind the endpoint (the schema is complete now) and run the loop. `C`
    /// is the input pod clients send. Honors `PM_LAG_MS`/`PM_LOSS`; returns
    /// when the loop quits, `Err` on bind failure.
    pub fn run<C: Pod + 'static>(self) -> std::io::Result<()> {
        let PmServer { mut pm, addr } = self;
        pm.serve::<C>(&addr)?;
        pm.loop_run();
        Ok(())
    }

    /// Create a replicated singleton. Server-only: the server owns it, and a
    /// replica mirrors the entity by reading the pool with `pool()` (see
    /// [`Pm::single`]).
    pub fn sync_single<T: Pod + Default + 'static>(&mut self, name: &str) -> SingleHandle<T> {
        let single = self.pm.single::<T>(name);
        self.pm.sync(single.pool());
        single
    }

    /// Register a typed reliable event channel and return its **receiver**.
    /// Events are one-way client→server (see [`PmClient::event`]); drain the
    /// returned [`EventRx`] from a server task to read this tick's events,
    /// each tagged with the sender peer.
    pub fn event<E: Pod + 'static>(&mut self, name: &str) -> EventRx<E> {
        let tag = event_register(&mut self.pm, name);
        EventRx {
            events: self.pm.single::<ServerEvents>("net.events"),
            tag,
            _marker: PhantomData,
        }
    }
}

/// Stable wire tag for a named event channel, in the user-tag space
/// (`>= EVENT_USER_BASE`) so it can't collide with internal frame types.
/// Same name → same tag on both ends; see [`pool_key`] for the hash.
fn event_tag(name: &str) -> u16 {
    let span = u16::MAX - EVENT_USER_BASE;
    EVENT_USER_BASE + pool_key(name) % span
}

/// Per-process guard: two event names hashing to the same tag would
/// cross-deliver, so registration panics loudly (mirrors the `pool_key`
/// collision guard for synced pools).
#[derive(Default)]
struct EventReg(HashMap<u16, String>);

fn event_register(pm: &mut Pm, name: &str) -> u16 {
    let tag = event_tag(name);
    let reg = pm.single::<EventReg>("net.event.reg");
    let mut reg = reg.get_mut();
    match reg.0.get(&tag) {
        Some(prev) if prev != name => panic!(
            "event name-hash collision: '{name}' and '{prev}' both tag to {tag:#06x} — rename one"
        ),
        Some(_) => {}
        None => {
            reg.0.insert(tag, name.to_string());
        }
    }
    tag
}

/// Sender for a typed, reliable **client→server** event channel
/// ([`PmClient::event`]). Clone freely into tasks.
#[derive(Clone)]
pub struct EventTx<E> {
    out: SingleHandle<Outbox>,
    tag: u16,
    _marker: PhantomData<E>,
}

impl<E: Pod> EventTx<E> {
    /// Queue `e` for reliable delivery to the server (held until the
    /// handshake completes, so it's safe to call at init).
    pub fn send(&self, e: E) {
        self.out.get_mut().send(self.tag, bytemuck::bytes_of(&e));
    }
}

/// Receiver for a typed, reliable **client→server** event channel
/// ([`PmServer::event`]).
#[derive(Clone)]
pub struct EventRx<E> {
    events: SingleHandle<ServerEvents>,
    tag: u16,
    _marker: PhantomData<E>,
}

impl<E: Pod> EventRx<E> {
    /// This tick's received events of type `E`, as `(peer, event)`. Reading
    /// is non-destructive (each receiver filters its own tag), valid for the
    /// rest of the tick — call from a server task above [`NET_PRIO`].
    pub fn drain(&self) -> Vec<(u8, E)> {
        self.events
            .get()
            .0
            .iter()
            .filter(|(_, ty, bytes)| *ty == self.tag && bytes.len() == std::mem::size_of::<E>())
            .map(|(peer, _, bytes)| (*peer, bytemuck::pod_read_unaligned(bytes)))
            .collect()
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

/// Reliable client→server events received this tick (server, `"net.events"`):
/// (peer, type, payload). Backs [`EventRx`]; games read it through that.
//
// TODO(typed-events): this is the last raw event surface. Once a
// variable-length/bytes event channel exists (for things like hellfire's
// `EV_NAME` String), migrate the remaining raw users and demote this to
// `pub(crate)`. Events are one-way client→server; there is deliberately no
// server→client event channel — server→client facts are state (a synced
// pool, TTL'd if they fade).
#[derive(Default)]
pub struct ServerEvents(pub Vec<(u8, u16, Vec<u8>)>);

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

/// Connection status (client, `"net.status"`). Internal plumbing the net
/// task fills; games read it through the [`ClientNet`] handle
/// ([`PmClient::net`]).
#[derive(Default)]
pub(crate) struct NetStatus {
    pub peer: u8,
    pub rtt_ms: f32,
    pub snapshots: u32,
    pub connected: bool,
    /// This peer's controlled entity, as the server marked it (see
    /// [`ServerOwn`]). `None` until the first snapshot that carries one.
    /// [`ClientNet::mine`] is the sugar most game code reads.
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

/// The command the input layer wants sent (client, `"net.input"`) — set by
/// the game via [`ClientNet::input`], read by the net task at its fixed
/// send cadence. Internal plumbing behind that setter.
pub(crate) struct NetInput<C: Pod>(pub C);

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
