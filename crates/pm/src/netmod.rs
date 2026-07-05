//! Installable net modules: the transport-pumping loop every game wrote
//! by hand, hoisted into core. `Pm::server` / `Pm::client` pick the role;
//! `run()` moves the QUIC endpoint into a single net task (priority
//! [`NET_PRIO`]) that trades exclusively in plain data — games hold typed
//! handles and never touch the transport.
//!
//! The doctrine: **the client only ever sends channels; the server only
//! ever replicates pools.** Every named thing on the wire — synced pools,
//! event channels, the input channel — lives in one registry (one hash
//! keyspace, one collision panic) and is schema-checked at the handshake,
//! so both ends must agree on the full channel set before a single tick
//! of state flows.
//!
//! Server (`Pm::server(addr)`), everything from the [`PmServer`] wrapper:
//! - [`net`](PmServer::net) → [`ServerNet`]: who joined/left this tick,
//!   and each peer's controlled entity (`own_set` ships the id in every
//!   snapshot header — the built-in replacement for a hand-rolled
//!   "here's your id" event).
//! - [`input`](PmServer::input) → [`InputRx`]: the continuous input
//!   channel. `pop` is the command-frame model (one per tick, bounded
//!   skip-ahead), `latest` is newest-wins; consuming records the seq
//!   that gets echoed back for prediction reconciliation.
//! - [`event`](PmServer::event) → [`EventRx`]: reliable client→server
//!   events received this tick, tagged with the sender peer.
//! - [`sync_single`](PmServer::sync_single): a replicated singleton the
//!   server owns.
//!
//! Client (`Pm::client(addr, hz)`), everything from the [`PmClient`] wrapper:
//! - [`net`](PmClient::net) → [`ClientNet`]: status reads
//!   ([`mine`](ClientNet::mine), [`rtt_ms`](ClientNet::rtt_ms),
//!   [`peer`](ClientNet::peer), [`snapshots`](ClientNet::snapshots),
//!   [`connected`](ClientNet::connected)) and the per-tick
//!   [`applied`](ClientNet::applied) snapshot log (each entry carries the
//!   server's input-seq echo, the reconciliation point).
//! - [`input`](PmClient::input) → [`InputTx`]: set the continuous input
//!   pod; the net task samples and sends it at the constructor's fixed
//!   `input_hz` cadence, decoupled from the loop rate (prediction must
//!   step the same fixed dt as the server, whatever the display refresh
//!   is). Exactly ONE continuous channel per connection.
//! - [`event`](PmClient::event) → [`EventTx`]: queue reliable one-way
//!   client→server events (held until the handshake completes). There is
//!   no server→client event channel: server→client facts are state.
//! - [`sync_single`](PmClient::sync_single) → [`SingleRx`]: typed read
//!   handle for a server-owned replicated singleton.
//! - [`predict_pool`](PmClient::predict_pool) /
//!   [`interp_pool`](PmClient::interp_pool): local-avatar prediction and
//!   remote-entity snapshot interpolation over a synced pool.
//!
//! Handles follow the kernel idiom: fetch at init, clone into the task
//! closures that need them. Per-tick data (peer joins, events, applied
//! snapshots) is cleared and refilled by the net task each tick: read it
//! from tasks at priority above [`NET_PRIO`] — it is valid for the rest
//! of that tick. A client whose connection errors or closes quits the
//! loop.

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::marker::PhantomData;
use std::ops::{Deref, DerefMut};
use std::rc::Rc;

use bytemuck::Pod;

use crate::id::Id;
use crate::kernel::{PoolHandle, Pm, SingleHandle};
use crate::net::{Applied, NetClient, NetServer, Outbox, SyncSet, WireKind, WireReg, event_tag};
use crate::predict::Predictor;
use crate::smooth::{InterpBuffer, pool_interp};
use crate::transport::{QuicClient, QuicServer};

/// Priority of the net task both roles register. It runs first in the
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
/// ([`net`](PmServer::net), [`input`](PmServer::input) receive,
/// [`event`](PmServer::event) receive, [`sync_single`](PmServer::sync_single)).
/// Derefs to [`Pm`] for everything shared — pools, singles, tasks. Build
/// with [`Pm::server`].
pub struct PmServer {
    pm: Pm,
    addr: String,
}

/// A networked [`Pm`] in the **client** role: mirrors server state and carries
/// the client-only surface ([`net`](PmClient::net), [`input`](PmClient::input)
/// send, [`event`](PmClient::event) send, [`sync_single`](PmClient::sync_single)
/// read, [`predict_pool`](PmClient::predict_pool),
/// [`interp_pool`](PmClient::interp_pool)). Derefs to [`Pm`] for everything
/// shared. Build with [`Pm::client`].
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
/// ([`Pm::server`] / [`Pm::client`]), register pools with [`Pm::sync_pool`]
/// and channels with the role's `input`/`event`, then `run`. The transport
/// pumps in a single task bound lazily by `run` (the schema must be
/// complete first); games only ever hold typed handles.
impl Pm {
    /// Construct an authoritative [`PmServer`] that binds `addr` at
    /// [`run`](PmServer::run). pm owns the transport end to end — games never
    /// touch the socket.
    pub fn server(addr: &str) -> PmServer {
        PmServer {
            pm: Pm::new(),
            addr: addr.to_string(),
        }
    }

    /// Construct a [`PmClient`] that connects to `addr` at
    /// [`run`](PmClient::run), sampling its input channel at `input_hz`
    /// (match the server's sim rate — 60.0 for a 60 Hz server).
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
    /// (shared) and the roles' `sync_single` are the public surface.
    fn sync<T: Pod + 'static>(&mut self, pool: &PoolHandle<T>) {
        let name = pool.name().to_string();
        self.single::<WireReg>("net.reg")
            .get_mut()
            .register(WireKind::Pool, &name, size_of::<T>());
        self.single::<SyncSet>("net.sync")
            .get_mut()
            .pool_sync(&name, pool);
    }

    /// The full handshake schema — every registered pool, event channel,
    /// and the input channel, as (kind, name, size). Order-independent:
    /// the transport sorts by name, and replication keys sections by name
    /// hash.
    fn net_schema(&mut self) -> Vec<(u8, String, usize)> {
        self.single::<WireReg>("net.reg").get().schema()
    }

    /// Bind the server transport and install the net task. Called by `run`.
    fn serve(&mut self, addr: &str) -> std::io::Result<()> {
        let mut quic = QuicServer::bind(addr, &self.net_schema())?;
        if let Some((delay, loss)) = link_lag_from_env() {
            quic.link_lag_set(delay, loss);
        }
        let sync = std::mem::take(&mut *self.single::<SyncSet>("net.sync").get_mut());
        self.removal_hold_set(true);
        NetServer::with_sync(sync).serve(self, quic);
        Ok(())
    }

    /// Connect the client transport and install the net task. Called by `run`.
    fn connect(&mut self, addr: &str, input_hz: f32) -> std::io::Result<()> {
        let mut quic = QuicClient::connect(addr, &self.net_schema())?;
        if let Some((delay, loss)) = link_lag_from_env() {
            quic.link_lag_set(delay, loss);
        }
        let sync = std::mem::take(&mut *self.single::<SyncSet>("net.sync").get_mut());
        NetClient::with_sync(sync).connect(self, quic, input_hz);
        Ok(())
    }
}

// --- the continuous input channel ---------------------------------------
//
// Exactly ONE per connection: prediction replay is inherently
// single-stream (one seq, one echo), and a pod with more fields beats a
// second pod. The type is a property of the channel, not of the app — so
// `run()` needs no turbofish, and the wire registry carries the channel's
// name and pod size into the handshake schema.

/// State shared between an [`InputTx`], the erased sender the net task
/// drives, and [`predict_pool`](PmClient::predict_pool): the current pod
/// (newest-wins) and the (seq, cmd) pairs sent this tick.
struct InputShared<C> {
    current: C,
    sent: Vec<(u32, C)>,
}

/// Sender half of the continuous input channel ([`PmClient::input`]).
/// `set` the pod from the input task; the net task samples and ships it
/// unreliably at the fixed input cadence. Clone freely into tasks.
pub struct InputTx<C: Pod> {
    shared: Rc<RefCell<InputShared<C>>>,
}

impl<C: Pod> Clone for InputTx<C> {
    fn clone(&self) -> Self {
        Self {
            shared: self.shared.clone(),
        }
    }
}

impl<C: Pod + 'static> InputTx<C> {
    /// Set the continuous input the net task sends each input-cadence
    /// tick. Newest-wins: call it every tick from the input task.
    //
    // TODO(input-map): a declarative device→action layer (Unreal Enhanced
    // Input style) should FILL this pod from named axes/actions, so games
    // stop hand-building it from raw scancodes. Lives in pm_sdl; this
    // setter is the seam it writes through.
    pub fn set(&self, cmd: C) {
        self.shared.borrow_mut().current = cmd;
    }
}

/// The erased face of a registered [`InputTx`] that the client net task
/// drives: clear the per-tick sent log, then sample/send/log at the
/// input cadence.
trait InputSend {
    fn frame_begin(&self);
    fn send(&self, quic: &mut QuicClient);
}

struct TxAdapter<C: Pod> {
    shared: Rc<RefCell<InputShared<C>>>,
}

impl<C: Pod> InputSend for TxAdapter<C> {
    fn frame_begin(&self) {
        self.shared.borrow_mut().sent.clear();
    }

    fn send(&self, quic: &mut QuicClient) {
        let cmd = self.shared.borrow().current;
        let seq = quic.input_send(bytemuck::bytes_of(&cmd));
        self.shared.borrow_mut().sent.push((seq, cmd));
    }
}

/// The registered client input channel (`"net.input"` single) — plumbing
/// between registration and the net task.
#[derive(Default)]
pub(crate) struct ClientInputChan(Option<Rc<dyn InputSend>>);

/// Per-peer queues of the continuous input channel, shared between an
/// [`InputRx`] and the erased sink the server net task pushes into.
struct InputQueues<C: Pod> {
    queues: HashMap<u8, VecDeque<(u32, C)>>,
    applied: HashMap<u8, (u32, C)>,
}

impl<C: Pod> Default for InputQueues<C> {
    fn default() -> Self {
        Self {
            queues: HashMap::new(),
            applied: HashMap::new(),
        }
    }
}

/// Receiver half of the continuous input channel ([`PmServer::input`]).
/// The net task pushes decoded pods in; the sim consumes per peer via
/// [`pop`](InputRx::pop) or [`latest`](InputRx::latest), which also
/// records the applied seq the net task echoes back to that peer. Clone
/// freely into tasks.
pub struct InputRx<C: Pod> {
    shared: Rc<RefCell<InputQueues<C>>>,
}

impl<C: Pod> Clone for InputRx<C> {
    fn clone(&self) -> Self {
        Self {
            shared: self.shared.clone(),
        }
    }
}

impl<C: Pod + 'static> InputRx<C> {
    /// Command-frame consumption: one input per call (= per sim tick),
    /// holding the last command when the queue runs dry and skipping
    /// ahead when it backs up (bounds queue-induced latency to ~2
    /// ticks). Matches one client prediction step per consumed input.
    pub fn pop(&self, peer: u8) -> C {
        let mut sh = self.shared.borrow_mut();
        let q = sh.queues.entry(peer).or_default();
        let mut consumed = None;
        while q.len() > 2 {
            consumed = q.pop_front();
        }
        if let Some(next) = q.pop_front() {
            consumed = Some(next);
        }
        if let Some(c) = consumed {
            sh.applied.insert(peer, c);
        }
        sh.applied
            .get(&peer)
            .map(|&(_, c)| c)
            .unwrap_or_else(C::zeroed)
    }

    /// Newest-wins consumption: drain to the latest command and hold it.
    /// For games where input is continuous state (held movement keys),
    /// not per-tick command frames.
    pub fn latest(&self, peer: u8) -> C {
        let mut sh = self.shared.borrow_mut();
        let q = sh.queues.entry(peer).or_default();
        if let Some(last) = q.drain(..).last() {
            sh.applied.insert(peer, last);
        }
        sh.applied
            .get(&peer)
            .map(|&(_, c)| c)
            .unwrap_or_else(C::zeroed)
    }
}

/// The erased face of a registered [`InputRx`] that the server net task
/// drives: peer lifecycle, decoded pushes (size-checked against the pod),
/// and the applied-seq echo.
trait InputSink {
    fn peer_add(&self, peer: u8);
    fn peer_remove(&self, peer: u8);
    fn push(&self, peer: u8, seq: u32, bytes: &[u8]);
    fn applied_seqs(&self) -> Vec<(u8, u32)>;
}

struct RxAdapter<C: Pod> {
    shared: Rc<RefCell<InputQueues<C>>>,
}

impl<C: Pod> InputSink for RxAdapter<C> {
    fn peer_add(&self, peer: u8) {
        self.shared.borrow_mut().queues.entry(peer).or_default();
    }

    fn peer_remove(&self, peer: u8) {
        let mut sh = self.shared.borrow_mut();
        sh.queues.remove(&peer);
        sh.applied.remove(&peer);
    }

    fn push(&self, peer: u8, seq: u32, bytes: &[u8]) {
        if bytes.len() != size_of::<C>() {
            return;
        }
        self.shared
            .borrow_mut()
            .queues
            .entry(peer)
            .or_default()
            .push_back((seq, bytemuck::pod_read_unaligned(bytes)));
    }

    /// Last consumed (peer, seq) pairs — echoed by the net task so clients
    /// can reconcile predictions against exactly what was applied.
    fn applied_seqs(&self) -> Vec<(u8, u32)> {
        self.shared
            .borrow()
            .applied
            .iter()
            .map(|(&p, &(seq, _))| (p, seq))
            .collect()
    }
}

/// The registered server input channel (`"net.input"` single) — plumbing
/// between registration and the net task.
#[derive(Default)]
pub(crate) struct ServerInputChan(Option<Rc<dyn InputSink>>);

/// Register the client half of the continuous input channel. Crate-shared
/// so in-crate tests can register on a bare kernel; [`PmClient::input`]
/// is the public door.
pub(crate) fn input_tx<C: Pod + 'static>(pm: &mut Pm, name: &str) -> InputTx<C> {
    pm.single::<WireReg>("net.reg")
        .get_mut()
        .register(WireKind::Input, name, size_of::<C>());
    let shared = Rc::new(RefCell::new(InputShared {
        current: C::zeroed(),
        sent: Vec::new(),
    }));
    pm.single::<ClientInputChan>("net.input").get_mut().0 = Some(Rc::new(TxAdapter {
        shared: shared.clone(),
    }));
    InputTx { shared }
}

/// Register the server half of the continuous input channel. Crate-shared
/// so in-crate tests can register on a bare kernel; [`PmServer::input`]
/// is the public door.
pub(crate) fn input_rx<C: Pod + 'static>(pm: &mut Pm, name: &str) -> InputRx<C> {
    pm.single::<WireReg>("net.reg")
        .get_mut()
        .register(WireKind::Input, name, size_of::<C>());
    let shared = Rc::new(RefCell::new(InputQueues::default()));
    pm.single::<ServerInputChan>("net.input").get_mut().0 = Some(Rc::new(RxAdapter {
        shared: shared.clone(),
    }));
    InputRx { shared }
}

/// The client role's in-task surface: status reads and the applied-
/// snapshot log, as an ordinary cloneable handle. Obtained from
/// [`PmClient::net`] — only a client can construct one, so a task that
/// captures it is a client task by construction. Like every pool/single
/// handle: fetch at init, clone into the closures that need it. Reads go
/// straight to the captured singles — no per-call store lookup.
#[derive(Clone)]
pub struct ClientNet {
    status: SingleHandle<NetStatus>,
    applied: SingleHandle<AppliedLog>,
}

impl ClientNet {
    /// This peer's controlled entity, as the server marked it — `None`
    /// until the first snapshot that carries one (see
    /// [`ServerNet::own_set`]).
    pub fn mine(&self) -> Option<Id> {
        self.status.get().avatar
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

    /// Snapshots applied this tick, each carrying the server's input-seq
    /// echo — the reconciliation points. [`predict_pool`](PmClient::predict_pool)
    /// consumes these for you; read them directly for HUD/diagnostics or a
    /// hand-rolled predictor. Valid for the rest of the tick (call from a
    /// task above [`NET_PRIO`]).
    pub fn applied(&self) -> Vec<Applied> {
        self.applied.get().0.clone()
    }
}

/// The server role's in-task surface: peer joins/leaves this tick and the
/// peer→controlled-entity table. Obtained from [`PmServer::net`] — only a
/// server can construct one. Clone into the closures that need it.
#[derive(Clone)]
pub struct ServerNet {
    peers: SingleHandle<PeerEvents>,
    own: SingleHandle<ServerOwn>,
}

impl ServerNet {
    /// Peers that joined this tick. Per-tick data from the net task: read
    /// from a task above [`NET_PRIO`].
    pub fn joined(&self) -> Vec<u8> {
        self.peers.get().joined.clone()
    }

    /// Peers that left this tick (disconnect or timeout).
    pub fn left(&self) -> Vec<u8> {
        self.peers.get().left.clone()
    }

    /// Mark `peer`'s controlled entity. The net task ships it in every
    /// snapshot header, so the client always knows which replicated entity
    /// is its own ([`ClientNet::mine`]) — the built-in replacement for a
    /// hand-rolled "here's your id" reliable event, robust to packet loss
    /// (it rides every snapshot, not one). Doubles as the server's
    /// peer→entity lookup ([`own`](ServerNet::own)/[`owned`](ServerNet::owned)).
    pub fn own_set(&self, peer: u8, id: Id) {
        self.own.get_mut().set(peer, id);
    }

    /// `peer`'s controlled entity, if one is marked. Cleared automatically
    /// by the net task when the peer leaves — peer ids recycle, so a stale
    /// entry would hand the next player on this id someone else's entity.
    pub fn own(&self, peer: u8) -> Option<Id> {
        self.own.get().get(peer)
    }

    /// Every (peer, controlled entity) pair — the iteration most sim
    /// tasks want.
    pub fn owned(&self) -> Vec<(u8, Id)> {
        self.own.get().0.iter().map(|(&p, &id)| (p, id)).collect()
    }
}

/// Typed read handle for a **synced single** on the client — the replica
/// side of [`PmServer::sync_single`]. The server owns the value; the
/// replica reads the latest snapshot of it (zeroed until the first
/// snapshot carrying it arrives). Obtained from [`PmClient::sync_single`];
/// clone into tasks.
pub struct SingleRx<T: Pod> {
    pool: PoolHandle<T>,
}

impl<T: Pod> Clone for SingleRx<T> {
    fn clone(&self) -> Self {
        Self {
            pool: self.pool.clone(),
        }
    }
}

impl<T: Pod + 'static> SingleRx<T> {
    /// The current replicated value — zeroed until the first snapshot
    /// carrying it arrives (check [`ClientNet::connected`] or a flag field
    /// if the distinction matters).
    pub fn get(&self) -> T {
        self.pool
            .get()
            .values()
            .first()
            .copied()
            .unwrap_or_else(T::zeroed)
    }
}

/// Client-only setup surface: the status handle, the send halves of the
/// channels, synced-single reads, local-avatar prediction, remote
/// interpolation, and the connect-and-run entry.
impl PmClient {
    /// The client's in-task status surface as a handle: fetch once at
    /// init, clone into the tasks that read status ([`mine`](ClientNet::mine),
    /// [`rtt_ms`](ClientNet::rtt_ms), …) or the applied-snapshot log.
    pub fn net(&mut self) -> ClientNet {
        ClientNet {
            status: self.pm.single::<NetStatus>("net.status"),
            applied: self.pm.single::<AppliedLog>("net.applied"),
        }
    }

    /// Register the **continuous input channel** and return its sender.
    /// `C` is the input pod: unreliable, newest-wins, sampled at the
    /// constructor's `input_hz` cadence. The name is the channel's wire
    /// identity — the server must register the same name and pod. Exactly
    /// one continuous channel per connection (a second registration
    /// panics): prediction replay is single-stream, and a pod with more
    /// fields beats a second pod. Discrete intents (respawn, ready) belong
    /// on [`event`](PmClient::event) channels instead — don't event-ize
    /// axes, don't pod-ize one-shots.
    pub fn input<C: Pod + 'static>(&mut self, name: &str) -> InputTx<C> {
        input_tx(&mut self.pm, name)
    }

    /// Register a synced single and return the typed **read** handle for
    /// its replicated value — the replica side of
    /// [`PmServer::sync_single`], without the pool ceremony.
    pub fn sync_single<T: Pod + 'static>(&mut self, name: &str) -> SingleRx<T> {
        let pool = self.pm.sync_pool::<T>(name);
        SingleRx { pool }
    }

    /// Client-side prediction for the local avatar ([`ClientNet::mine`]), wired
    /// straight to the net module: this installs the task every predicted
    /// game wrote by hand. Each tick it reconciles the [`Predictor`] against
    /// every applied snapshot's input-seq echo, replays this tick's sends
    /// from the input channel, then writes the smooth-predicted avatar into
    /// the pool's draw sibling (`"<name>.draw"`, the one
    /// [`interp_pool`](PmClient::interp_pool) fills) — extrapolated by the
    /// in-flight input fraction so the render clock doesn't beat against
    /// the fixed predict step.
    ///
    /// `input` is the channel the predictions replay — the same one the
    /// input task `set`s. `step` is THE shared integration (the same one
    /// the server runs — determinism is what makes reconciliation
    /// byte-exact); `fixed_dt` is the server's sim step. Pair with
    /// [`interp_pool`](PmClient::interp_pool) on the same pool for the
    /// remote entities (it returns the draw pool to render). Returns the
    /// predictor single so rendering can read `state()` / `corrections`.
    pub fn predict_pool<S: Pod + 'static, C: Pod + 'static>(
        &mut self,
        auth: &PoolHandle<S>,
        input: &InputTx<C>,
        step: impl Fn(&mut S, C, f32) + 'static,
        err: impl Fn(&S, &S) -> f32 + 'static,
        tolerance: f32,
        fixed_dt: f32,
    ) -> SingleHandle<Predictor<S, C>> {
        let draw = self.pool::<S>(&format!("{}.draw", auth.name()));
        let pred = self.single::<Predictor<S, C>>("net.pred");
        let status = self.single::<NetStatus>("net.status");
        let applied = self.single::<AppliedLog>("net.applied");
        let shared = input.shared.clone();
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
                for &(seq, cmd) in &shared.borrow().sent {
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
                    step(&mut s, shared.borrow().current, alpha * fixed_dt);
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
        // Per-pool task name: several interp'd pools coexist and each
        // shows up on its own in task_stats / task_stop.
        let task = format!("net.interp.{}", auth.name());
        let auth = auth.clone();
        let ret = draw.clone();
        let mut buf = InterpBuffer::<T>::new(delay);
        buf.extrap_max = extrap_max;
        let mut clock = 0.0f64;
        self.task_add(&task, NET_PRIO + 1.0, 0.0, move |pm| {
            clock += pm.loop_dt() as f64;
            pool_interp(&auth, &draw, &mut buf, clock, &lerp);
        });
        ret
    }

    /// Connect to the server (the schema is complete now) and run the loop.
    /// Honors `PM_LAG_MS`/`PM_LOSS`; returns when the loop quits, `Err` if
    /// the connection fails.
    pub fn run(self) -> std::io::Result<()> {
        let PmClient {
            mut pm,
            addr,
            input_hz,
        } = self;
        pm.connect(&addr, input_hz)?;
        pm.loop_run();
        Ok(())
    }

    /// Register a typed reliable event channel and return its **sender**.
    /// Events are one-way client→server: discrete, must-arrive intents the
    /// client originates ("respawn", "ready", "start") — the things that
    /// don't fit the continuous, lossy input channel. (Server→client facts
    /// stay state: a synced pool, TTL'd if they fade.) The returned
    /// [`EventTx`] queues an event the net task ships reliably once
    /// connected. Events are fixed-size pods; a short name is a
    /// `Name([u8; 24])`, not a `String`.
    pub fn event<E: Pod + 'static>(&mut self, name: &str) -> EventTx<E> {
        let tag = event_register(&mut self.pm, name, size_of::<E>());
        EventTx {
            out: self.pm.single::<Outbox>("net.out"),
            tag,
            _marker: PhantomData,
        }
    }
}

impl PmServer {
    /// The server's in-task surface as a handle: fetch once at init, clone
    /// into the tasks that react to joins/leaves or mark controlled
    /// entities ([`own_set`](ServerNet::own_set)).
    pub fn net(&mut self) -> ServerNet {
        ServerNet {
            peers: self.pm.single::<PeerEvents>("net.peers"),
            own: self.pm.single::<ServerOwn>("net.own"),
        }
    }

    /// Register the **continuous input channel** and return its receiver.
    /// `C` is the input pod clients send (same name and pod on both ends —
    /// the handshake schema enforces it). Exactly one continuous channel
    /// per connection (a second registration panics); see
    /// [`PmClient::input`].
    pub fn input<C: Pod + 'static>(&mut self, name: &str) -> InputRx<C> {
        input_rx(&mut self.pm, name)
    }

    /// Bind the endpoint (the schema is complete now) and run the loop.
    /// Honors `PM_LAG_MS`/`PM_LOSS`; returns when the loop quits, `Err` on
    /// bind failure.
    pub fn run(self) -> std::io::Result<()> {
        let PmServer { mut pm, addr } = self;
        pm.serve(&addr)?;
        pm.loop_run();
        Ok(())
    }

    /// Create a replicated singleton the server owns. The replica side
    /// reads it through [`PmClient::sync_single`]'s typed handle.
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
        let tag = event_register(&mut self.pm, name, size_of::<E>());
        EventRx {
            events: self.pm.single::<ServerEvents>("net.events"),
            tag,
            _marker: PhantomData,
        }
    }
}

/// Register an event channel in the wire registry (idempotent for the
/// same name/pod; panics on a hash collision or a kind/size mismatch)
/// and derive its wire tag.
fn event_register(pm: &mut Pm, name: &str, size: usize) -> u16 {
    pm.single::<WireReg>("net.reg")
        .get_mut()
        .register(WireKind::Event, name, size);
    event_tag(name)
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

/// Peers that joined or left this tick (server, `"net.peers"`). Internal
/// plumbing the net task fills; games read it through [`ServerNet`].
#[derive(Default)]
pub(crate) struct PeerEvents {
    pub joined: Vec<u8>,
    pub left: Vec<u8>,
}

/// Reliable client→server events received this tick (server,
/// `"net.events"`): (peer, tag, payload). Internal plumbing behind
/// [`EventRx`].
#[derive(Default)]
pub(crate) struct ServerEvents(pub Vec<(u8, u16, Vec<u8>)>);

/// Per-peer controlled entity (server, `"net.own"`). Internal plumbing
/// behind [`ServerNet::own_set`]; the net task ships it in every snapshot
/// header.
#[derive(Default)]
pub(crate) struct ServerOwn(pub HashMap<u8, Id>);

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
    /// [`ServerNet::own_set`]). `None` until the first snapshot that
    /// carries one. [`ClientNet::mine`] is the sugar most game code reads.
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

/// Snapshots applied this tick (client, `"net.applied"`). Internal
/// plumbing behind [`ClientNet::applied`] and
/// [`predict_pool`](PmClient::predict_pool).
#[derive(Default)]
pub(crate) struct AppliedLog(pub Vec<Applied>);

impl NetServer {
    /// Install the server net module: moves `self` and the endpoint
    /// into a net task that pumps the wire and trades in the `"net.*"`
    /// singles (see module docs). Call after registering synced pools
    /// and channels.
    pub fn serve(self, pm: &mut Pm, mut quic: QuicServer) {
        let mut net = self;
        let peers = pm.single::<PeerEvents>("net.peers");
        let events = pm.single::<ServerEvents>("net.events");
        let own = pm.single::<ServerOwn>("net.own");
        let sink = pm.single::<ServerInputChan>("net.input").get().0.clone();
        pm.task_add("net", NET_PRIO, 0.0, move |pm| {
            quic.pump();
            {
                let mut pe = peers.get_mut();
                // Leaves reported last tick have had a full tick above
                // NET_PRIO with ownership intact (games despawn via
                // `own(p)`); drop their entries now — peer ids recycle,
                // and a stale entry would mark the departed player's
                // entity as the NEXT player's own. Runs before this
                // tick's joins, so a same-id rejoin starts clean.
                for &p in &pe.left {
                    own.get_mut().clear(p);
                }
                pe.joined.clear();
                pe.left.clear();
                for p in quic.joined_drain() {
                    net.peer_add(p);
                    if let Some(s) = &sink {
                        s.peer_add(p);
                    }
                    pe.joined.push(p);
                }
                for p in quic.left_drain() {
                    net.peer_remove(p);
                    if let Some(s) = &sink {
                        s.peer_remove(p);
                    }
                    pe.left.push(p);
                }
                if let Some(s) = &sink {
                    for (p, seq, bytes) in quic.inputs_drain() {
                        s.push(p, seq, &bytes);
                    }
                    // Echo what the sim consumed (last tick — it runs
                    // after this task); clients reconcile against this.
                    for (p, seq) in s.applied_seqs() {
                        net.input_processed(p, seq);
                    }
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
    /// singles (see module docs). The input channel (if registered) is
    /// sampled at a fixed `input_hz` cadence (match the server's sim
    /// rate — 60.0 for a 60 Hz server). Connection errors quit the loop.
    pub fn connect(self, pm: &mut Pm, mut quic: QuicClient, input_hz: f32) {
        let net = self;
        let status = pm.single::<NetStatus>("net.status");
        let applied = pm.single::<AppliedLog>("net.applied");
        let out = pm.single::<Outbox>("net.out");
        let chan = pm.single::<ClientInputChan>("net.input").get().0.clone();
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
            if let Some(ch) = &chan {
                ch.frame_begin();
            }
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
                    if let Some(ch) = &chan {
                        ch.send(&mut quic);
                    }
                }
                status.get_mut().input_alpha = accum * input_hz;
            }
            status.get_mut().rtt_ms = quic.rtt().as_secs_f32() * 1e3;
        });
    }
}
