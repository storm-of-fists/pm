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
//! of state flows. The strictness is deliberate: local pools and singles
//! never enter the schema (server-only metrics and client-only draw
//! pools are free), so a mismatch in what remains is always a bug, and
//! equality turns it into a loud connect error.
//!
//! Server (`Pm::server(addr)`), everything from the [`PmServer`] wrapper:
//! - [`net`](PmServer::net) → [`ServerNet`]: who joined/left this tick,
//!   each peer's controlled entity (`own_set` ships the whole
//!   peer→entity table in every snapshot header — the built-in
//!   replacement for a hand-rolled "here's your id" event AND for
//!   hand-carried peer fields in avatar pods), and per-peer link stats
//!   ([`acked_tick`](ServerNet::acked_tick)/[`rtt_ms`](ServerNet::rtt_ms)).
//!   Ownership auto-clears the tick after a leave: the leave handler
//!   still sees `own(p)` to despawn by.
//! - [`ttl_pool`](PmServer::ttl_pool) / [`history_pool`](PmServer::history_pool):
//!   the duration modifiers — transient entries expire after a lifetime;
//!   a pool keeps a window of past ticks the server can rewind into
//!   (lag compensation).
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
//!   [`connected`](ClientNet::connected)), the replicated ownership
//!   table ([`owner_of`](ClientNet::owner_of)/[`own`](ClientNet::own)/
//!   [`owned`](ClientNet::owned) — who controls what, not just
//!   [`mine`](ClientNet::mine)), and the per-tick
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
//!
//!   Why isn't the input channel reliable too? A dropped input frame is
//!   *stale* — the next frame supersedes it. Reliable delivery would
//!   retransmit it anyway, arriving late in a burst, blocking fresher
//!   data behind it (head-of-line) exactly when feel matters most.
//!   Reliability comes from **redundancy** instead: every input
//!   datagram carries the last several frames, so loss costs nothing
//!   and nothing is retransmitted late. Reliable events are for facts
//!   that stay true (a respawn request); the stick position is a fact
//!   that expires every tick. Two channel kinds because there are two
//!   kinds of truth. This was re-litigated against Unreal's current
//!   stack (2026-07-16) and every UE movement system agrees: Enhanced
//!   Input is client-local (intent pods, never keycodes — forces never
//!   cross the wire; a replayed input through a deterministic step IS
//!   the force); CharacterMovementComponent sends moves as an
//!   explicitly *unreliable* RPC covered by redundant `SavedMoves`
//!   resubmission; Chaos Networked Physics and Mover define an intent
//!   input struct networked with history and re-applied during
//!   resimulation.
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

use std::cell::{Ref, RefCell};
use std::collections::{HashMap, VecDeque};
use std::marker::PhantomData;
use std::ops::{Deref, DerefMut};
use std::rc::Rc;

use bytemuck::Pod;

use crate::duration::{HistoryRing, pool_expire};
use crate::id::Id;
use crate::kernel::{Pm, PoolHandle, SingleHandle};
use crate::net::{
    Applied, NetClient, NetServer, Outbox, SyncSet, Wire, WireKind, WireReg, event_tag,
};
use crate::predict::Predictor;
use crate::smooth::{InterpBuffer, pool_interp};
use crate::transport::{QuicClient, QuicServer};

/// Priority of the net task both roles register. It runs first in the
/// tick (per net.rs: sending before the sim avoids relabeling freshly
/// stamped entries); register game tasks above it.
// TODO(roadmap): the "read per-tick net data only from tasks above
// NET_PRIO" rule lives in folklore + module docs — promote it into a
// type or runtime guard when touching this area anyway.
pub const NET_PRIO: f32 = 5.0;

/// Artificial link delay/loss from `PM_LAG_MS` (milliseconds) and
/// `PM_LOSS` (0..1 drop fraction) — the simulation knob clients used to
/// wire by hand around the QUIC endpoint, now read by `connect` ONLY
/// (one lagged socket per link; see `serve` for why the server must not
/// stack a second one). `None` when both are unset or zero.
/// `PM_NETDBG=1` — the net doctor. Both roles print link vitals every
/// 5 s: the server reports, per peer, how far its ack cursor trails the
/// tick (the lag-comp rewind anchor — if this GROWS the peer is being
/// served stale state), RTT, this tick's snapshot flight size (datagrams
/// sent — pinned at 1 when the backlog fits, shrinking back toward 1
/// under congestion), and quinn's remaining datagram buffer; the
/// client reports its snapshot apply rate and how fast the applied tick
/// LABELS advance (labels slower than ticks = stale content, the
/// invisible failure that broke lag comp for weeks; see the
/// FIXME(lag-sim) in transport.rs). The history ring also warns when a
/// rewind clamps past its window instead of clamping silently.
pub(crate) fn netdbg() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("PM_NETDBG").is_ok_and(|v| v != "0"))
}

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
    password: Option<String>,
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
    link_lag: Option<(std::time::Duration, f32)>,
    password: Option<String>,
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
            password: None,
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
            link_lag: None,
            password: None,
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

    /// [`sync_pool`](Self::sync_pool) with a compact wire representation:
    /// the pool keeps the game's full-precision struct; snapshots carry
    /// its derived [`Wire::Repr`] (see `#[derive(pm::Wire)]` for the
    /// quantization attributes). The handshake schema carries the REPR
    /// size, so an end still registering the pool via `sync_pool` is
    /// rejected at connect — switch both sides together.
    pub fn wire_pool<T: Wire>(&mut self, name: &str) -> PoolHandle<T> {
        let pool = self.pool::<T>(name);
        self.single::<WireReg>("net.reg").get_mut().register(
            WireKind::Pool,
            name,
            size_of::<T::Repr>(),
        );
        self.single::<SyncSet>("net.sync")
            .get_mut()
            .pool_wire(name, &pool);
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
    ///
    /// Deliberately does NOT honor `PM_LAG_MS`/`PM_LOSS` — the CLIENT
    /// applies the simulated link (see `connect`). When server and client
    /// share a process (hogs no-arg, tests) they share the env too, and
    /// lagging both sockets stacks: every packet crossed two delay queues
    /// (RTT = 4x the knob, not 2x) and rolled loss twice per direction —
    /// "80 ms / 3%" actually simulated a 320 ms / ~6% link.
    fn serve(&mut self, addr: &str, password: Option<String>) -> std::io::Result<()> {
        let mut quic = QuicServer::bind(addr, &self.net_schema())?;
        if let Some(pw) = &password {
            quic.password_set(pw);
        }
        let sync = std::mem::take(&mut *self.single::<SyncSet>("net.sync").get_mut());
        self.removal_hold_set(true);
        NetServer::with_sync(sync).serve(self, quic);
        Ok(())
    }

    /// Connect the client transport and install the net task. Called by `run`.
    /// `link_lag`: an explicit simulated link (one-way delay, loss) wins;
    /// `None` falls back to the `PM_LAG_MS`/`PM_LOSS` env knob.
    fn connect(
        &mut self,
        addr: &str,
        input_hz: f32,
        link_lag: Option<(std::time::Duration, f32)>,
        password: Option<String>,
    ) -> std::io::Result<()> {
        let mut quic = QuicClient::connect(addr, &self.net_schema())?;
        if let Some(pw) = &password {
            quic.password_set(pw);
        }
        if let Some((delay, loss)) = link_lag.or_else(link_lag_from_env) {
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
    /// (seq, cmd, peer's acked snapshot tick when this input ARRIVED).
    /// The view stamp is captured at arrival because the ack that rode
    /// alongside the input is what the shooter actually saw — reading
    /// `acked_tick` later, at consumption, anchors lag-comp a queue's
    /// worth of ticks too fresh (zero kills under 40 ms lag until this).
    queues: HashMap<u8, VecDeque<(u32, C, u32)>>,
    applied: HashMap<u8, (u32, C, u32)>,
}

impl<C: Pod> Default for InputQueues<C> {
    fn default() -> Self {
        Self {
            queues: HashMap::new(),
            applied: HashMap::new(),
        }
    }
}

impl<C: Pod> InputQueues<C> {
    /// The command held for `peer` — the last one consumed, or zeroed
    /// before any input arrived.
    fn held(&self, peer: u8) -> C {
        self.applied
            .get(&peer)
            .map(|&(_, c, _)| c)
            .unwrap_or_else(C::zeroed)
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
        sh.held(peer)
    }

    /// The peer's acked snapshot tick as of when the most recently
    /// consumed input ARRIVED — THE lag-compensation rewind anchor:
    /// subtract the game's interp ticks and judge that input's shots
    /// against `history_pool` frames at the result. (`ServerNet::
    /// acked_tick` read at consumption time is subtly too FRESH: the
    /// input waited in this queue while newer acks landed.) 0 before
    /// any input was consumed.
    pub fn view(&self, peer: u8) -> u32 {
        self.shared
            .borrow()
            .applied
            .get(&peer)
            .map_or(0, |&(_, _, v)| v)
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
        sh.held(peer)
    }
}

/// The erased face of a registered [`InputRx`] that the server net task
/// drives: peer lifecycle, decoded pushes (size-checked against the pod),
/// and the applied-seq echo.
trait InputSink {
    fn peer_add(&self, peer: u8);
    fn peer_remove(&self, peer: u8);
    fn push(&self, peer: u8, seq: u32, bytes: &[u8], view: u32);
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

    fn push(&self, peer: u8, seq: u32, bytes: &[u8], view: u32) {
        if bytes.len() != size_of::<C>() {
            return;
        }
        self.shared
            .borrow_mut()
            .queues
            .entry(peer)
            .or_default()
            .push_back((seq, bytemuck::pod_read_unaligned(bytes), view));
    }

    /// Last consumed (peer, seq) pairs — echoed by the net task so clients
    /// can reconcile predictions against exactly what was applied.
    fn applied_seqs(&self) -> Vec<(u8, u32)> {
        self.shared
            .borrow()
            .applied
            .iter()
            .map(|(&p, &(seq, _, _))| (p, seq))
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
    /// [`ServerNet::own_set`]). Sugar for [`own`](ClientNet::own) on our
    /// own peer id.
    pub fn mine(&self) -> Option<Id> {
        self.status.get().avatar
    }

    /// `peer`'s controlled entity, if the server marked one — the same
    /// table the server reads via [`ServerNet::own`], replicated in every
    /// snapshot header.
    pub fn own(&self, peer: u8) -> Option<Id> {
        let st = self.status.get();
        st.owners
            .iter()
            .find(|&&(p, _)| p == peer)
            .map(|&(_, id)| id)
    }

    /// Every (peer, controlled entity) pair — the client-side mirror of
    /// [`ServerNet::owned`].
    pub fn owned(&self) -> Vec<(u8, Id)> {
        self.status.get().owners.clone()
    }

    /// Which peer controls entity `id`, if any — the reverse lookup that
    /// replaces a hand-carried peer field in an avatar pod (tint by
    /// player, "is this row me", nameplates).
    pub fn owner_of(&self, id: Id) -> Option<u8> {
        let st = self.status.get();
        st.owners
            .iter()
            .find(|&&(_, e)| e == id)
            .map(|&(p, _)| p)
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

/// The server role's in-task surface: peer joins/leaves this tick, the
/// peer→controlled-entity table, and per-peer link stats. Obtained from
/// [`PmServer::net`] — only a server can construct one. Clone into the
/// closures that need it.
#[derive(Clone)]
pub struct ServerNet {
    peers: SingleHandle<PeerEvents>,
    own: SingleHandle<ServerOwn>,
    stats: SingleHandle<PeerStats>,
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

    /// Mark `peer`'s controlled entity. The net task ships the whole
    /// table in every snapshot header, so every client knows which
    /// replicated entity is its own ([`ClientNet::mine`]) AND who
    /// controls everyone else's ([`ClientNet::owner_of`]) — the built-in
    /// replacement for both a hand-rolled "here's your id" event and
    /// hand-carried peer fields in avatar pods, robust to packet loss
    /// (it rides every snapshot, not one). Doubles as the server's
    /// peer→entity lookup ([`own`](ServerNet::own)/[`owned`](ServerNet::owned)).
    //
    // TODO(roadmap): ONE entity per peer by design — a second per-peer
    // entity (a roster row, say) is a pool with a peer field, scanned
    // (fine at ≤8 peers).
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

    /// Which peer controls entity `id`, if any — the reverse lookup
    /// (mirrored client-side as [`ClientNet::owner_of`]).
    pub fn owner_of(&self, id: Id) -> Option<u8> {
        self.own
            .get()
            .0
            .iter()
            .find(|&(_, &e)| e == id)
            .map(|(&p, _)| p)
    }

    /// The newest snapshot tick `peer` has acknowledged (0 until the
    /// first ack). Acks and inputs share the client→server path, so this
    /// is ≈ the newest snapshot the client *had* when it sent the inputs
    /// arriving now — which makes
    /// `acked_tick(peer) - interp_delay_in_ticks` the tick that peer was
    /// *looking at*: the rewind point for lag compensation (see
    /// [`history_pool`](PmServer::history_pool)).
    pub fn acked_tick(&self, peer: u8) -> u32 {
        self.stats.get().0.get(&peer).map_or(0, |s| s.acked_tick)
    }

    /// Round-trip time to `peer` in milliseconds (0 for an unknown peer).
    /// The server-side sibling of [`ClientNet::rtt_ms`] — a link-health
    /// readout for metrics, and the raw ingredient if a game wants an
    /// RTT-based rewind instead of the acked-tick one.
    pub fn rtt_ms(&self, peer: u8) -> f32 {
        self.stats.get().0.get(&peer).map_or(0.0, |s| s.rtt_ms)
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

/// Read handle over a server-side window of a pool's past ticks — what
/// [`PmServer::history_pool`] returns. Clone into the tasks that rewind
/// (the handles share one ring; the installed task writes it each tick).
pub struct PoolHistory<T> {
    ring: Rc<RefCell<HistoryRing<T>>>,
}

// Hand-implemented (not derived) so cloning the *handle* never demands
// `T: Clone` — a derive bounds the impl on T. Same idiom as `InputTx`.
impl<T> Clone for PoolHistory<T> {
    fn clone(&self) -> Self {
        Self {
            ring: self.ring.clone(),
        }
    }
}

impl<T: Copy + 'static> PoolHistory<T> {
    /// The recorded frame nearest `tick` (clamped to the window edges;
    /// see [`HistoryRing::frame`]) as a zero-copy borrow. `None` only
    /// before the first frame is recorded. Don't hold it across a call
    /// that ticks the ring — it borrows the shared `RefCell`.
    pub fn frame(&self, tick: u32) -> Option<Ref<'_, [(Id, T)]>> {
        Ref::filter_map(self.ring.borrow(), |r| r.frame(tick)).ok()
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
    ///
    /// **Every field of `S` must be something `step` evolves from the
    /// command.** A field the server writes OUTSIDE the step (damage,
    /// pickups, scores) compiles fine and then misbehaves subtly: it
    /// freezes between corrections when read from `state()`, and every
    /// server write registers as a prediction miss, forcing spurious
    /// correction snaps. Server-owned quantities belong in their own
    /// synced pool, read raw. Enforce the rule mechanically with an
    /// exhaustive destructure (no `..`) at the top of `step` — a new
    /// field then refuses to compile until someone visits the one place
    /// where this contract is written down (see hogs' `truck_step`).
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
        // Per-pool names (same rule as interp_pool's task): a game can
        // predict SEVERAL pools — e.g. truck AND heli, whichever one the
        // avatar currently lives in — and a shared "net.pred" name would
        // panic on the second registration (pools key by name alone).
        // Each predictor idles while the avatar isn't in its pool.
        let pred = self.single::<Predictor<S, C>>(&format!("net.pred.{}", auth.name()));
        let status = self.single::<NetStatus>("net.status");
        let applied = self.single::<AppliedLog>("net.applied");
        let shared = input.shared.clone();
        let auth = auth.clone();
        self.task_add(&format!("net.predict.{}", auth.name()), NET_PRIO + 2.0, 0.0, {
            let pred = pred.clone();
            move |_pm| {
                let Some(mine) = status.get().avatar else {
                    return;
                };
                // One fetch for all of this tick's snapshots — the pool
                // doesn't change between them (the net task already
                // applied every snapshot before this task runs).
                let Some(auth_s) = auth.get_id(mine).map(|r| *r) else {
                    // Avatar isn't in this pool right now (not spawned,
                    // or currently a different predicted vehicle): idle
                    // the predictor — otherwise it keeps stepping stale
                    // state with the other pool's inputs. Emptied, it
                    // reseeds from authority the moment the avatar is
                    // back (reconcile's None path), and its state() stays
                    // None so game code can tell which pool is live.
                    let mut p = pred.get_mut();
                    if p.state().is_some() {
                        *p = Predictor::default();
                    }
                    return;
                };
                for a in &applied.get().0 {
                    pred.get_mut().reconcile(
                        auth_s,
                        a.input_seq,
                        |s, c| step(s, c, fixed_dt),
                        |a, b| err(a, b),
                        tolerance,
                    );
                }
                for &(seq, cmd) in &shared.borrow().sent {
                    pred.get_mut()
                        .predict(seq, cmd, |s, c| step(s, c, fixed_dt));
                }
                // Smooth-predicted local avatar into draw, extrapolated
                // by the in-flight input fraction. The avatar is present
                // in `auth` (checked above), so no ghost outlives a
                // despawn.
                if let Some(mut s) = pred.get().state() {
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

    /// Present this session password at connect. Must match the
    /// server's [`PmServer::password`] or the server closes the
    /// connection ("bad password") — a locked server also drops clients
    /// that present nothing. A bouncer, not cryptography: QUIC/TLS
    /// already encrypts the wire; this only decides who is ADMITTED
    /// (before admission the server sends no snapshots and reads no
    /// inputs or events).
    pub fn password(&mut self, pw: &str) {
        self.password = Some(pw.to_string());
    }

    /// Simulate link conditions on this client: one-way `lag_ms` and
    /// `loss` (0..1 drop fraction), applied in both directions at connect
    /// (RTT rises by ~2x the lag). The programmatic sibling of the
    /// `PM_LAG_MS`/`PM_LOSS` env knob — an explicit call wins over the
    /// env, so games can surface these as CLI arguments (env vars are a
    /// pain to pass on Windows). Zero/zero means a clean link.
    pub fn link_lag(&mut self, lag_ms: f32, loss: f32) {
        self.link_lag = Some((
            std::time::Duration::from_secs_f32(lag_ms.max(0.0) / 1000.0),
            loss.clamp(0.0, 1.0),
        ));
        // Seed the live-tune single so dashboards see the initial values
        // (seq stays 0 — connect applies the initial itself).
        let tune = self.pm.single::<LinkTune>("net.linktune");
        let mut t = tune.get_mut();
        t.lag_ms = lag_ms.max(0.0);
        t.loss = loss.clamp(0.0, 1.0);
    }

    /// Handle to the LIVE link-sim tune: bump `seq` after writing
    /// `lag_ms`/`loss` and the net task re-applies them to the transport
    /// on its next pump — the runtime sibling of
    /// [`link_lag`](PmClient::link_lag), for telemetry knobs and debug
    /// consoles. Untouched (seq 0) it costs one comparison per tick.
    pub fn link_tune(&mut self) -> SingleHandle<LinkTune> {
        self.pm.single::<LinkTune>("net.linktune")
    }

    /// Connect to the server (the schema is complete now) and run the loop.
    /// Honors `PM_LAG_MS`/`PM_LOSS` unless [`link_lag`](PmClient::link_lag)
    /// was called; returns when the loop quits, `Err` if the connection
    /// fails.
    pub fn run(self) -> std::io::Result<()> {
        let PmClient {
            mut pm,
            addr,
            input_hz,
            link_lag,
            password,
        } = self;
        pm.connect(&addr, input_hz, link_lag, password)?;
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

/// Seconds → whole sim ticks at the current loop rate. Read every tick
/// (not captured at task install) — `loop_rate` is usually set after the
/// duration modifiers, just before run().
fn secs_ticks(pm: &Pm, secs: f32) -> u32 {
    (secs * pm.loop_rate.max(1) as f32).ceil() as u32
}

impl PmServer {
    /// The server's in-task surface as a handle: fetch once at init, clone
    /// into the tasks that react to joins/leaves, mark controlled
    /// entities ([`own_set`](ServerNet::own_set)), or read per-peer link
    /// stats ([`acked_tick`](ServerNet::acked_tick)/[`rtt_ms`](ServerNet::rtt_ms)).
    pub fn net(&mut self) -> ServerNet {
        ServerNet {
            peers: self.pm.single::<PeerEvents>("net.peers"),
            own: self.pm.single::<ServerOwn>("net.own"),
            stats: self.pm.single::<PeerStats>("net.peerstat"),
        }
    }

    /// Handle to the LIVE per-peer send-budget tune ([`SendTune`]): how
    /// many kilobits/sec of snapshot flight each peer may receive
    /// (default [`SEND_KBPS_DEFAULT`]). Write it any time — a game param
    /// task, a debug console — and the net task reads it on its next
    /// tick. One datagram per tick per peer always goes regardless, so a
    /// low value degrades to the classic single-datagram cadence, never
    /// below it. The server-side sibling of [`PmClient::link_tune`].
    pub fn send_tune(&mut self) -> SingleHandle<SendTune> {
        self.pm.single::<SendTune>("net.sendtune")
    }

    /// Give `pool`'s entries a lifetime: any entry not written for `secs`
    /// is removed (entity and all — see [`pool_expire`]), and the removal
    /// replicates like any other. This is what makes **transient facts
    /// safe as pool entries** (a contact point, a hit marker, a grenade
    /// ping): spawn each occurrence on a fresh id and let the TTL clean
    /// up — never hand-roll a removal timer per game.
    ///
    /// Keep `secs` comfortably above one resend window (~1 RTT + a couple
    /// of snapshot intervals): an entry that dies faster than that can
    /// expire before a lossy client ever saw it — its add and its removal
    /// coalesce into nothing.
    pub fn ttl_pool<T: 'static>(&mut self, pool: &PoolHandle<T>, secs: f32) {
        let task = format!("net.ttl.{}", pool.name());
        let pool = pool.clone();
        self.task_add(&task, NET_PRIO + 0.5, 0.0, move |pm| {
            pool_expire(pm, &pool, secs_ticks(pm, secs));
        });
    }

    /// Keep a `secs`-deep window of `pool`'s **past ticks** on the server
    /// and return the [`PoolHistory`] handle that rewinds into it — the
    /// memory lag compensation reads.
    ///
    /// Frames are labeled like snapshots (`tick - 1`, the last completed
    /// tick, recorded right after the net task packs), so a frame label
    /// IS a snapshot tick a client may have seen. To judge an acting
    /// peer's view — "was I really on that car when I hit it?" — rewind
    /// to what they were looking at when they issued the input:
    ///
    /// ```text
    /// let view = net.acked_tick(peer)              // newest tick they HAD
    ///     .saturating_sub(interp_ticks);           // minus their interp delay
    /// let frame = hist.frame(view);                // other entities, as seen
    /// ```
    ///
    /// The interp delay is the client's presentation constant
    /// ([`interp_pool`](PmClient::interp_pool)'s `delay`, in ticks) —
    /// share it between both builds like the fixed dt. Rewinds deeper
    /// than the window clamp to the oldest frame (bounded rewind).
    pub fn history_pool<T: Copy + 'static>(
        &mut self,
        pool: &PoolHandle<T>,
        secs: f32,
    ) -> PoolHistory<T> {
        let ring = Rc::new(RefCell::new(HistoryRing::new(1)));
        let task = format!("net.hist.{}", pool.name());
        let pool = pool.clone();
        let handle = PoolHistory { ring: ring.clone() };
        self.task_add(&task, NET_PRIO + 0.5, 0.0, move |pm| {
            let mut ring = ring.borrow_mut();
            ring.cap_set(secs_ticks(pm, secs).max(1) as usize);
            let frame: Vec<(Id, T)> = pool.get().iter().map(|(id, v)| (id, *v)).collect();
            ring.push(pm.tick().saturating_sub(1), frame);
        });
        handle
    }

    /// Register the **continuous input channel** and return its receiver.
    /// `C` is the input pod clients send (same name and pod on both ends —
    /// the handshake schema enforces it). Exactly one continuous channel
    /// per connection (a second registration panics); see
    /// [`PmClient::input`].
    pub fn input<C: Pod + 'static>(&mut self, name: &str) -> InputRx<C> {
        input_rx(&mut self.pm, name)
    }

    /// Require a session password from every client (the door for a
    /// hosted game — see [`PmClient::password`] for the contract). Call
    /// before [`run`](PmServer::run); an unauthenticated connection gets
    /// the schema hello but is never admitted: no snapshots out, no
    /// inputs/events in, closed after a short timeout or on a wrong
    /// guess.
    pub fn password(&mut self, pw: &str) {
        self.password = Some(pw.to_string());
    }

    /// Bind the endpoint (the schema is complete now) and run the loop.
    /// Returns when the loop quits, `Err` on bind failure. (The simulated
    /// link is the CLIENT's — see `serve`; the server never lags itself.)
    pub fn run(self) -> std::io::Result<()> {
        let PmServer { mut pm, addr, password } = self;
        pm.serve(&addr, password)?;
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

/// Per-peer link/ack stats (server, `"net.peerstat"`), refreshed by the
/// net task every tick. Internal plumbing behind
/// [`ServerNet::acked_tick`]/[`ServerNet::rtt_ms`].
#[derive(Default)]
pub(crate) struct PeerStats(pub HashMap<u8, PeerStat>);

#[derive(Clone, Copy, Default)]
pub(crate) struct PeerStat {
    pub acked_tick: u32,
    pub rtt_ms: f32,
}

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

/// Live client link-sim tuning (see [`PmClient::link_tune`]): the net
/// task applies `lag_ms`/`loss` to the transport whenever `seq` changes.
#[derive(Clone, Copy, Default)]
pub struct LinkTune {
    pub lag_ms: f32,
    pub loss: f32,
    pub seq: u32,
}

/// Default per-peer flight budget, kilobits/sec: ~250 KB/s ≈ 3–4 full
/// datagrams per tick at 60 Hz — drains a 200-hog fully-dirty set with
/// headroom while staying small next to any real link.
pub const SEND_KBPS_DEFAULT: f32 = 2000.0;

/// Live server send-budget tune (the `"net.sendtune"` single, see
/// [`PmServer::send_tune`]): per-peer snapshot bandwidth in
/// kilobits/sec, read by the net task every tick. The first datagram of
/// each tick is unconditional (the classic cadence — no value can
/// starve the link below the pre-flight baseline); the budget governs
/// how far the multi-datagram flight may extend past it.
#[derive(Clone, Copy)]
pub struct SendTune {
    pub kbps: f32,
}

impl Default for SendTune {
    fn default() -> Self {
        SendTune {
            kbps: SEND_KBPS_DEFAULT,
        }
    }
}

/// Hard per-tick flight rail regardless of budget: 8 datagrams ≈
/// 5.6 Mbit/s at 60 Hz — a runaway-knob backstop, not a tuning surface.
const FLIGHT_MAX: u32 = 8;

/// Stop extending a flight when quinn's outgoing datagram buffer can't
/// take one more full datagram plus slack: the buffer filling up is BBR
/// saying the link isn't draining what's already queued, and the 16 KB
/// drop-oldest policy would evict flight members we just paid to pack.
const FLIGHT_SPACE_SLACK: usize = 256;

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
    /// The full peer→controlled-entity table from the newest snapshot
    /// header, sorted by peer — every player's ownership, not just ours.
    /// Read through [`ClientNet::owner_of`]/[`own`](ClientNet::own)/
    /// [`owned`](ClientNet::owned).
    pub owners: Vec<(u8, Id)>,
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
        let stats = pm.single::<PeerStats>("net.peerstat");
        let tune = pm.single::<SendTune>("net.sendtune");
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
                // Acks BEFORE inputs: they rode the same datagrams, and
                // each input gets stamped with the acked tick as of its
                // arrival — the shooter's contemporaneous view, i.e. the
                // lag-comp anchor (`InputRx::view`). Stamping later, at
                // consumption, reads acks that landed while the input
                // queued and anchors the rewind too fresh.
                for (p, tick, seq) in quic.acks_drain() {
                    net.ack(p, tick, seq);
                }
                if let Some(s) = &sink {
                    for (p, seq, bytes) in quic.inputs_drain() {
                        s.push(p, seq, &bytes, net.acked_tick(p));
                    }
                    // Echo what the sim consumed (last tick — it runs
                    // after this task); clients reconcile against this.
                    for (p, seq) in s.applied_seqs() {
                        net.input_processed(p, seq);
                    }
                }
            }
            let plist: Vec<u8> = net.peers().collect();
            {
                // Per-peer stats, refreshed now that this tick's acks
                // landed; departed peers' rows drop with them (peer ids
                // recycle — a stale row would hand the next player on
                // this id the old link's numbers).
                let mut st = stats.get_mut();
                st.0.retain(|p, _| plist.contains(p));
                for &p in &plist {
                    let e = st.0.entry(p).or_default();
                    e.acked_tick = net.acked_tick(p);
                    e.rtt_ms = quic.rtt(p).as_secs_f32() * 1e3;
                }
            }
            {
                let ev = &mut events.get_mut().0;
                ev.clear();
                ev.extend(quic.events_drain());
            }
            // Ship the whole peer→entity table in every header (same
            // bytes for all peers); sorted inside owners_set so the
            // HashMap's iteration order never reaches the wire.
            net.owners_set(own.get().0.iter().map(|(&p, &id)| (p, id.0)).collect());
            // Multi-datagram flights (roadmap 2026-07-17): the datagram
            // is atomic in one UDP packet, so per-tick freshness scales
            // by COUNT, not size. The first send is unconditional (the
            // classic cadence: keepalive, input echo, owner table,
            // removals); while entries didn't fit, more datagrams extend
            // the flight until the backlog drains, the kbps budget or
            // hard rail is spent, or the send buffer says the link isn't
            // draining (under congestion BBR shrinks the flight back
            // toward one datagram — and smallest-dirty-first fairness is
            // what keeps sparse pools fresh while the horde degrades).
            // Convergence never depends on the flight: what doesn't fit
            // stays unconfirmed and rotates, exactly as before.
            let per_tick =
                (tune.get().kbps.max(0.0) * 125.0 / pm.loop_rate.max(1) as f32) as usize;
            let mut flights: Vec<(u8, u32)> = Vec::new();
            for p in plist {
                let mut allowance = per_tick;
                let mut sends = 0u32;
                loop {
                    let dgram = quic.snapshot_budget(p);
                    let budget = if sends == 0 { dgram } else { dgram.min(allowance) };
                    let Some(snap) = net.snapshot_budgeted(pm, p, budget) else {
                        break;
                    };
                    quic.snapshot_send(p, &snap.bytes);
                    allowance = allowance.saturating_sub(snap.bytes.len());
                    sends += 1;
                    if !snap.more
                        || snap.entries == 0
                        || sends >= FLIGHT_MAX
                        || allowance == 0
                        || quic.dgram_space(p) < dgram + FLIGHT_SPACE_SLACK
                    {
                        break;
                    }
                }
                flights.push((p, sends));
            }
            net.prune(pm);
            if netdbg() && pm.tick() % 300 == 0 {
                for (p, sends) in flights {
                    eprintln!(
                        "[netdbg srv] peer {p}: ack-lag={} ticks  rtt={:.0}ms  flight={sends}  dgram-buf-free={}B",
                        pm.tick().saturating_sub(net.acked_tick(p)),
                        quic.rtt(p).as_secs_f32() * 1e3,
                        quic.dgram_space(p),
                    );
                }
            }
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
        let tune = pm.single::<LinkTune>("net.linktune");
        let mut tune_seq = 0u32;
        let mut accum = 0.0f32;
        // Net-doctor state: (applied count, newest label) at last report.
        let mut dbg_prev = (0u32, 0u32);
        let mut dbg_newest = 0u32;
        pm.task_add("net", NET_PRIO, 0.0, move |pm| {
            // Live link-sim retune (telemetry knobs): seq bump = apply.
            {
                let t = tune.get();
                if t.seq != tune_seq {
                    tune_seq = t.seq;
                    quic.link_lag_set(
                        std::time::Duration::from_secs_f32(t.lag_ms.max(0.0) / 1000.0),
                        t.loss.clamp(0.0, 1.0),
                    );
                }
            }
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
            let mut snaps = quic.snapshots_drain();
            // A flight arrives as back-to-back datagrams UDP may reorder;
            // apply in send order (the seq at header bytes 4..8) so a
            // straggler can't overwrite newer state with older.
            snaps.sort_by_key(|s| {
                s.get(4..8)
                    .map_or(0, |b| u32::from_le_bytes(b.try_into().unwrap()))
            });
            for snap in snaps {
                let Ok(a) = net.apply(pm, &snap) else {
                    continue;
                };
                dbg_newest = dbg_newest.max(a.tick);
                quic.ack_send(a.tick, a.seq);
                {
                    // The header table is authoritative as of this
                    // snapshot: mine() is our own row (peer ids start at
                    // 1, so a pre-handshake peer of 0 matches nothing and
                    // the next snapshot fills it in).
                    let mut st = status.get_mut();
                    let me = st.peer;
                    st.avatar = a
                        .owners
                        .iter()
                        .find(|&&(p, _)| p == me)
                        .map(|&(_, id)| id);
                    st.owners = a.owners.clone();
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
            if netdbg() && pm.tick() % 300 == 0 {
                let snaps = status.get().snapshots;
                // labels+N vs ~300 loop ticks is the STALENESS gauge: at
                // 60 Hz both should be ~300. applied+ fine while labels+
                // trails = the server is feeding this client old state.
                eprintln!(
                    "[netdbg cli] applied+{}  labels+{}  (per ~300 ticks)  rtt={:.0}ms",
                    snaps.saturating_sub(dbg_prev.0),
                    dbg_newest.saturating_sub(dbg_prev.1),
                    quic.rtt().as_secs_f32() * 1e3,
                );
                dbg_prev = (snaps, dbg_newest);
            }
        });
    }
}
