//! Installable net modules: the transport-pumping loop every game wrote
//! by hand, hoisted into core. `Pm::server` / `Pm::client` pick the role;
//! `run()` moves the QUIC endpoint into the engine's PRESENT/COMMIT
//! phases (see `Pm::loop_once`) — receive/apply before the task cycle,
//! send after it — trading exclusively in plain data; games hold typed
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
//! - [`ttl_pool`](PmServer::ttl_pool) / [`journal_pool`](PmServer::journal_pool):
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
//! snapshots) is cleared and refilled by the PRESENT phase each tick,
//! before any task runs — every game task may read it, no ordering rule
//! to remember (the phase turn, 2026-07-23, retired the old "register
//! above NET_PRIO" folklore). A client whose connection errors or
//! closes quits the loop.

use std::any::Any;
use std::cell::{Cell, Ref, RefCell};
use std::collections::{HashMap, VecDeque};
use std::marker::PhantomData;
use std::ops::{Deref, DerefMut};
use std::rc::Rc;

use bytemuck::Pod;

use crate::blend::{PodErr, PodLerp, PodSchema};
use crate::math::{Vec3, vec3};
use crate::journal::Journal;
use crate::params::{PARAM_SAVE, ParamSet, params_load, params_save};
use crate::id::Id;
use crate::kernel::{Pm, PoolHandle, SingleHandle};
use crate::net::{
    Applied, NetClient, NetServer, Outbox, RECORDER_PEER, SyncSet, Wire, WireKind, WireReg,
    event_tag,
};
use crate::predict::Predictor;
use crate::smooth::{InterpBuffer, pool_interp};
use crate::transport::{QuicClient, QuicServer};


// TODO(refactor): THE API CRITIQUE (2026-07-23, Connor asked for a
// harder look: "primitives as dumb as possible, networking as hidden
// as possible" — hogs is now the ONLY consumer, demo/drive/hellfire
// deleted, so the API serves exactly one game and can be judged by
// it). What hogs writes that smells engine-shaped, worst first:
// 1. THE REDIAL LOOP: token persistence, first-loss clock, attempt
//    counter, grace+5 give-up — ~60 lines of player_client that every
//    game will need verbatim. Wants `run` to own it: game hands a
//    session-builder closure, engine owns dial/redial/token/backoff.
// 2. RECONNECT PARKING: the engine returns the same peer id, but the
//    game hand-rolls park-on-left / adopt-on-join / expire-at-grace
//    (roster task HashMap). The ownership table already knows the
//    peer's entity — `own_park`/`own_adopt` semantics (ownership
//    survives a leave, marked parked, auto-expiring on the SAME grace
//    clock) would delete the whole dance.
// 3. INTEREST SCORERS: hogs' avatar_xz closure + eye/cone math is the
//    standard scorer every game will write. Wants a built-in: game
//    supplies position-of-entry (`|v| (x, z)`), engine supplies
//    eye-else-avatar distance × forward cone × staleness.
// 4. DRAW-POOL DELAY/EXTRAP REPETITION: interp_pool(&p, delay, extrap)
//    × 5 pools with the same two constants — wants client-level
//    defaults (`interp_defaults(delay, extrap)`) so the call is
//    `interp_pool(&p)`. (Auto-install stays rejected: the line per
//    pool is the record of which pools get draw siblings.)
// 5. REGISTRATION DUPLICATION: server.rs and client_setup list the
//    same 11 channels; the schema check catches drift at CONNECT, a
//    shared `register_world(pm)` in the game would catch it at
//    COMPILE. Game-side fix, engine could bless the pattern.
// 6. DONE 2026-07-23 (the phase turn): engine work moved into the
//    kernel's PRESENT/COMMIT phases — NET_PRIO deleted, the ordering
//    folklore is now structure, and send-after-sim reclaimed a tick
//    of latency each way.
// 7. THE VEHICLE-DUALITY TAX: truck+heli as separate pools multiplies
//    every seam (two predictors, two interps, avatar-in-which-pool
//    checks, per-vehicle clamp bugs — the 07-23 turret bug lived
//    exactly here). M3 adds infantry for humans AND hogs; decide
//    before then whether "avatar" becomes one pod with a kind enum or
//    the engine grows a first-class pool-set-avatar concept.
// (hellfire's deletion also left `interp_pool_with` caller-less — it
// stays as the custom-blend escape hatch — and modload without a demo;
// modload gets a real consumer again when hogs wants mods or not at
// all. pool_mirror/coast_blend, the demos' dead-reckoning seams, were
// deleted 2026-07-23 the moment the review found them caller-less.)
//
// TODO(refactor): THE NAMING PASS (2026-07-23, Connor: "design as if a
// hundred people use it — simple, clear, bulletproof"; renames are
// cheapest RIGHT NOW with one consumer). Recommendations, his pick:
// 8. `InputRx::view()` now COLLIDES with the view-pose surface
//    (`view_set`/`view_pose`, different concept entirely) → rename to
//    `seen_tick(peer)` ("the tick the peer had SEEN when this input
//    arrived" — which is what the doc already says it is).
// 9. THREE WORDS FOR ONE CONCEPT: `own_set`/`own`/`owned`/`owner_of`
//    (server) vs `mine` (client) vs `NetStatus.avatar` (internals) all
//    mean "the peer's controlled entity" → unify on AVATAR:
//    `avatar_set`, `avatar(peer)`, `avatars()`, `avatar_owner(id)`;
//    keep `mine()` as client sugar.
// 10. `Journal` vs `JournalHandle` vs `InterpBuffer` — three names
//    for "tick-addressed past of a pool" (v2 item 2 says it's ONE
//    concept) → converge on JOURNAL: `Journal`→`JournalRing`,
//    duration.rs→journal.rs; InterpBuffer waits for the client-store
//    merge.
// 11. SEAM/METHOD INVERSION: methods are `interp_pool`/`ttl_pool`,
//    their manual seams are `pool_interp`/`pool_expire` — same words,
//    flipped order, no rule says which form you're holding → seams
//    take the method's name + `_into`/demote to pub(crate).
// 12. `SendTune`/`send_tune` is the per-peer bandwidth BUDGET, and
//    "tune" already means the link-sim knobs → `send_budget`.
// 13. KEEP `predict_pool`/`interp_pool` despite the mirror instinct:
//    they're the industry's own words (a hundred newcomers arrive
//    knowing them); the pairing belongs in docs ("mine vs everyone
//    else"), not in renaming away shared vocabulary.
// 14. netmod.rs (the file) says nothing → roles.rs.

/// THE NET DOCTOR toggle ([`netdbg_enable`](crate::netdbg_enable) —
/// games surface it as an arg; runtime env knobs are dead by doctrine,
/// 2026-07-23). Both roles print link vitals every
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
    NETDBG.load(std::sync::atomic::Ordering::Relaxed)
}

static NETDBG: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Turn on the net doctor (periodic link vitals on stderr, plus loud
/// journal-rewind clamps). Call it from arg parsing — e.g. hogs'
/// `netdbg` flag; there is no env knob (one way in, by doctrine).
pub fn netdbg_enable() {
    NETDBG.store(true, std::sync::atomic::Ordering::Relaxed);
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
    /// Recording path (see [`record_to`](PmServer::record_to)); opened at `run`.
    record: Option<String>,
    /// One tick journal per pool name, shared across consumers — the
    /// registry behind [`journal_pool`](PmServer::journal_pool). Boxed
    /// `(Rc<RefCell<Journal<T>>>, Rc<Cell<f32>>)` per entry.
    journals: HashMap<String, Box<dyn Any>>,
    /// Reconnect grace override (seconds); `None` = transport default.
    grace: Option<f32>,
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
    session_token: Option<[u8; 16]>,
    /// Replay file instead of a live connection (see
    /// [`replay_from`](PmClient::replay_from)); opened at `run`.
    replay: Option<String>,
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
            record: None,
            journals: HashMap::new(),
            grace: None,
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
            session_token: None,
            replay: None,
        }
    }

    /// Create a pool and register it for replication, returning the handle —
    /// the one-call replacement for `pool()` + a separate sync step. The
    /// pool's name is its wire identity (hashed; see `pool_key`), so server
    /// and client may register in any order.
    pub fn sync_pool<T: Pod + PodSchema + 'static>(&mut self, name: &str) -> PoolHandle<T> {
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
    pub fn wire_pool<T: Wire + PodSchema>(&mut self, name: &str) -> PoolHandle<T> {
        let pool = self.pool::<T>(name);
        self.single::<WireReg>("net.reg").get_mut().register(
            WireKind::Pool,
            name,
            size_of::<T::Repr>(),
            T::SCHEMA_HASH,
        );
        self.single::<SyncSet>("net.sync")
            .get_mut()
            .pool_wire(name, &pool);
        pool
    }

    /// Register an existing `pool` for replication. Internal: `sync_pool`
    /// (shared) and the roles' `sync_single` are the public surface.
    fn sync<T: Pod + PodSchema + 'static>(&mut self, pool: &PoolHandle<T>) {
        let name = pool.name().to_string();
        self.single::<WireReg>("net.reg")
            .get_mut()
            .register(WireKind::Pool, &name, size_of::<T>(), T::SCHEMA_HASH);
        self.single::<SyncSet>("net.sync")
            .get_mut()
            .pool_sync(&name, pool);
    }

    /// The full handshake schema — every registered pool, event channel,
    /// and the input channel, as (kind, name, size). Order-independent:
    /// the transport sorts by name, and replication keys sections by name
    /// hash.
    fn net_schema(&mut self) -> Vec<(u8, String, usize, u64)> {
        self.single::<WireReg>("net.reg").get().schema()
    }

    /// Bind the server transport and install the net task. Called by `run`.
    ///
    /// The simulated link is the CLIENT's (`PmClient::link_lag`; see
    /// `connect`): lagging both ends of an in-process pair would stack
    /// (RTT = 4x the knob, loss rolled twice per direction).
    fn serve(
        &mut self,
        addr: &str,
        password: Option<String>,
        grace: Option<f32>,
        record: Option<String>,
    ) -> std::io::Result<()> {
        let schema = self.net_schema();
        let mut quic = QuicServer::bind(addr, &schema)?;
        if let Some(pw) = &password {
            quic.password_set(pw);
        }
        if let Some(secs) = grace {
            quic.reconnect_grace_set(std::time::Duration::from_secs_f32(secs.max(0.0)));
        }
        let sync = std::mem::take(&mut *self.single::<SyncSet>("net.sync").get_mut());
        self.removal_hold_set(true);
        let mut net = NetServer::with_sync(sync);
        if let Some(path) = record {
            let w = std::io::BufWriter::new(std::fs::File::create(&path)?);
            net.record_to(w, crate::transport::schema_encode(&schema));
            eprintln!("[pm net] recording to {path}");
        }
        net.serve(self, quic);
        Ok(())
    }

    /// Connect the client transport and install the net phases. Called by
    /// `run`. `link_lag`: the simulated link (one-way delay, loss), if any.
    fn connect(
        &mut self,
        addr: &str,
        input_hz: f32,
        link_lag: Option<(std::time::Duration, f32)>,
        password: Option<String>,
        session_token: Option<[u8; 16]>,
    ) -> std::io::Result<()> {
        let mut quic = QuicClient::connect(addr, &self.net_schema())?;
        if let Some(pw) = &password {
            quic.password_set(pw);
        }
        if let Some(token) = session_token {
            quic.session_token_set(token);
        }
        if let Some((delay, loss)) = link_lag {
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
    /// against journal frames at the result. (`ServerNet::
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
pub(crate) fn input_tx<C: Pod + PodSchema + 'static>(pm: &mut Pm, name: &str) -> InputTx<C> {
    pm.single::<WireReg>("net.reg")
        .get_mut()
        .register(WireKind::Input, name, size_of::<C>(), C::SCHEMA_HASH);
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
pub(crate) fn input_rx<C: Pod + PodSchema + 'static>(pm: &mut Pm, name: &str) -> InputRx<C> {
    pm.single::<WireReg>("net.reg")
        .get_mut()
        .register(WireKind::Input, name, size_of::<C>(), C::SCHEMA_HASH);
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

    /// Whether the handshake has completed (and the connection hasn't
    /// died since — the net task flips this false when the link drops).
    pub fn connected(&self) -> bool {
        self.status.get().connected
    }

    /// Report this client's VIEW POSE (camera eye + forward) to the
    /// server — the on-screen-ness ingredient for
    /// [`interest_pool`](crate::PmServer::interest_pool) scorers, read
    /// there via [`ServerNet::view_pose`]. Call every tick from the
    /// camera task; the net task ships the latest at the input cadence
    /// (unreliable, newest-wins — presentation metadata, never sim
    /// input). Clients that never call it simply score without a pose.
    pub fn view_set(&self, eye: Vec3, forward: Vec3) {
        self.status.get_mut().view =
            Some([eye.x, eye.y, eye.z, forward.x, forward.y, forward.z]);
    }

    /// Why the connection ended, once it has. The handle keeps the
    /// status single alive past the loop, so this is readable AFTER
    /// [`PmClient::run`] returns — `Some` means the loop quit on a
    /// dead link (redial with the same session token to reconnect),
    /// `None` means a local quit (menu, Escape).
    pub fn lost(&self) -> Option<String> {
        self.status.get().lost.clone()
    }

    /// Snapshots applied this tick, each carrying the server's input-seq
    /// echo — the reconciliation points. [`predict_pool`](PmClient::predict_pool)
    /// consumes these for you; read them directly for HUD/diagnostics or a
    /// hand-rolled predictor. Valid for the whole task cycle (the
    /// PRESENT phase fills it before any task runs).
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
    /// Peers that joined this tick. Per-tick data from the PRESENT
    /// phase: valid for the whole task cycle.
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
    /// [`journal_pool`](PmServer::journal_pool)).
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

    /// `peer`'s latest reported view pose as (eye, forward) — what
    /// [`ClientNet::view_set`] sent, refreshed every tick. `None` for
    /// peers that never report (bots). Built for
    /// [`interest_pool`](crate::PmServer::interest_pool) scorers:
    /// distance from EYE (not avatar) plus a forward-cone boost is the
    /// classic on-screen-ness score — keep any boost multiplicative and
    /// positive so staleness still guarantees off-screen entities a
    /// cadence (never cull to zero).
    pub fn view_pose(&self, peer: u8) -> Option<(Vec3, Vec3)> {
        self.stats.get().0.get(&peer).and_then(|s| s.view).map(|v| {
            (vec3(v[0], v[1], v[2]), vec3(v[3], v[4], v[5]))
        })
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
        self.try_get().unwrap_or_else(T::zeroed)
    }

    /// The replicated value if a snapshot has delivered one yet — the
    /// "has the server actually spoken" read (ParamsClient uses it to
    /// serve shipped defaults instead of zeros before the handshake).
    pub fn try_get(&self) -> Option<T> {
        self.pool.get().values().first().copied()
    }
}

/// A game's live-tuning surface on the client — the pair
/// [`PmClient::params`] returns: read the replicated set, ask the
/// server to change it. Clone freely into tasks.
pub struct ParamsClient<P: Pod> {
    pub(crate) replica: SingleRx<P>,
    pub(crate) tx: EventTx<ParamSet>,
}

impl<P: Pod> Clone for ParamsClient<P> {
    fn clone(&self) -> Self {
        Self {
            replica: self.replica.clone(),
            tx: self.tx.clone(),
        }
    }
}

impl<P: Pod + pm_control_core::Tunable + 'static> ParamsClient<P> {
    /// The current set: the server's replicated truth, or the shipped
    /// defaults before the first snapshot delivers it.
    pub fn get(&self) -> P {
        self.replica.try_get().unwrap_or_default()
    }

    /// Ask the server to set param `idx` (its clamp is the record; the
    /// applied value replicates back to everyone).
    pub fn set(&self, idx: u32, value: f32) {
        self.tx.send(ParamSet { idx, value });
    }

    /// Ask the server to persist the current set to its params file.
    pub fn save(&self) {
        self.tx.send(ParamSet {
            idx: PARAM_SAVE,
            value: 0.0,
        });
    }
}

/// Read handle over a pool's TICK JOURNAL — the server-side ring of
/// tick-stamped whole-pool frames behind [`PmServer::journal_pool`].
/// Clone into the tasks that rewind (all handles for one pool share one
/// ring; the installed task writes it once per tick).
///
/// THE JOURNAL is engine-v2 item 2's spine landing in place: one
/// tick-addressed store of "state at tick T" per synced pool, that the
/// mechanisms which each kept a bespoke copy derive from instead.
/// Derived TODAY: the lag-comp rewind (rewound frames read straight
/// off it); the snapshot packer's dirty scan rides the same tick axis
/// (`NetServer::refresh`, a per-tick change capture → per-peer
/// candidate lists), and the client's interp samples are stamped with
/// snapshot LABEL times (see `interp_pool`) — stage 2, 2026-07-23.
/// QUEUED (the TODO(v2) item-2 status in lib.rs): recordings/replays
/// (serialize the ring) and kill-cams (play it back).
pub struct JournalHandle<T> {
    ring: Rc<RefCell<Journal<T>>>,
}

// Hand-implemented (not derived) so cloning the *handle* never demands
// `T: Clone` — a derive bounds the impl on T. Same idiom as `InputTx`.
impl<T> Clone for JournalHandle<T> {
    fn clone(&self) -> Self {
        Self {
            ring: self.ring.clone(),
        }
    }
}

impl<T: Copy + 'static> JournalHandle<T> {
    /// The recorded frame nearest `tick` (clamped to the window edges;
    /// see [`Journal::frame`]) as a zero-copy borrow. `None` only
    /// before the first frame is recorded. Don't hold it across a call
    /// that ticks the ring — it borrows the shared `RefCell`.
    pub fn frame(&self, tick: u32) -> Option<Ref<'_, [(Id, T)]>> {
        Ref::filter_map(self.ring.borrow(), |r| r.frame(tick)).ok()
    }

    /// The newest recorded tick label (`None` before the first frame) —
    /// how deep "now" is; pair with [`oldest`](JournalHandle::oldest) to
    /// see the whole recorded window.
    pub fn newest(&self) -> Option<u32> {
        self.ring.borrow().newest()
    }

    /// The oldest recorded tick label still in the window.
    pub fn oldest(&self) -> Option<u32> {
        self.ring.borrow().oldest()
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
    pub fn input<C: Pod + PodSchema + 'static>(&mut self, name: &str) -> InputTx<C> {
        input_tx(&mut self.pm, name)
    }

    /// Register a synced single and return the typed **read** handle for
    /// its replicated value — the replica side of
    /// [`PmServer::sync_single`], without the pool ceremony.
    pub fn sync_single<T: Pod + PodSchema + 'static>(&mut self, name: &str) -> SingleRx<T> {
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
    /// byte-exact); `fixed_dt` is the server's sim step. The error
    /// metric comes from the pod itself (`S::pod_err`, generated by
    /// `#[pm::pod]` — no hand closure to drift); `tolerance` is the
    /// game's snap threshold over it. Pair with
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
    pub fn predict_pool<S: Pod + PodErr + 'static, C: Pod + 'static>(
        &mut self,
        auth: &PoolHandle<S>,
        input: &InputTx<C>,
        step: impl Fn(&mut S, C, f32) + 'static,
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
        // The step is shared by both phase closures (reconcile/draw in
        // PRESENT, replay in COMMIT) — one Rc, zero per-tick cost.
        let step = Rc::new(step);
        // PRESENT: reconcile against everything the receive phase just
        // applied, then write the smooth-predicted avatar into draw —
        // extrapolated by the in-flight input fraction so the render
        // clock doesn't beat against the fixed predict step (exact:
        // it's precisely where this tick's replay will land).
        {
            let (pred, status, applied) = (pred.clone(), status.clone(), applied.clone());
            let (shared, auth, draw, step) =
                (shared.clone(), auth.clone(), draw.clone(), step.clone());
            self.pm
                .phase_present(&format!("predict.{}", auth.name()), move |_pm| {
                    let Some(mine) = status.get().avatar else {
                        return;
                    };
                    // One fetch for all of this tick's snapshots — the
                    // receive phase already applied every one of them.
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
                            S::pod_err,
                            tolerance,
                        );
                    }
                    // The avatar is present in `auth` (checked above),
                    // so no ghost outlives a despawn.
                    if let Some(mut s) = pred.get().state() {
                        let alpha = status.get().input_alpha.min(1.0);
                        step(&mut s, shared.borrow().current, alpha * fixed_dt);
                        draw.get_mut().add(mine, s);
                    }
                });
        }
        // COMMIT: replay the input sends the net send phase just made
        // (it runs first in commit and assigns their seqs).
        // TODO(simplify): predict-on-input — the render task still sees
        // the avatar one input behind (masked well by the alpha
        // extrapolation above); true zero-frame response means stepping
        // the predictor the moment InputTx::set runs, which needs the
        // input seam to own a peek at the predictor. Revisit after the
        // phase turn has been played.
        {
            let (pred, status, auth) = (pred.clone(), status, auth.clone());
            self.pm
                .phase_commit(&format!("predict.{}.replay", auth.name()), move |_pm| {
                    let Some(mine) = status.get().avatar else {
                        return;
                    };
                    if auth.get_id(mine).is_none() {
                        return;
                    }
                    for &(seq, cmd) in &shared.borrow().sent {
                        pred.get_mut().predict(seq, cmd, |s, c| step(s, c, fixed_dt));
                    }
                });
        }
        pred
    }

    /// Snapshot-interpolation presentation for a replicated pool — the
    /// per-pool sync modifier the [`pool_interp`] note promised. Installs a
    /// task that eases every entity in `auth` into a draw sibling pool
    /// (`"<name>.draw"`) ~`delay` seconds behind the newest authoritative
    /// sample (see [`InterpBuffer`]), via the pod's own generated blend
    /// (`T::pod_lerp` — angle/snap tags and all; no hand closure), and
    /// returns that draw pool — the one rendering should read. Runs before
    /// [`predict_pool`](PmClient::predict_pool) on the same pool, so the local avatar's
    /// interpolated value is harmlessly overwritten by the predicted one;
    /// everyone else stays smooth.
    ///
    /// `delay`/`extrap_max` are seconds; a snapshot interval or two of delay
    /// is the usual start, with a small `extrap_max` to ride loss bursts.
    ///
    /// The samples live on the TICK AXIS (v2 item 2 stage 2): each is
    /// stamped with its applied snapshot's LABEL time (label ×
    /// 1/input_hz — exact server pacing), not the wall clock it happened
    /// to arrive on, so flight bursts and arrival jitter never wobble
    /// the spacing; a budget-rotated entity's samples sit exactly as far
    /// apart as the server sent them. Rendering samples at a
    /// smoothly-advancing estimate of the newest label (advance by
    /// loop_dt, softly slewed onto the label stream; a >0.25 s gap
    /// snaps), `delay` behind.
    pub fn interp_pool<T: Pod + PodLerp + PartialEq + 'static>(
        &mut self,
        auth: &PoolHandle<T>,
        delay: f64,
        extrap_max: f64,
    ) -> PoolHandle<T> {
        self.interp_pool_with(auth, T::pod_lerp, delay, extrap_max)
    }

    /// [`interp_pool`](PmClient::interp_pool) with a custom blend for
    /// the cases per-field tags can't express (hellfire's "snap on a
    /// respawn-sized jump" is the canonical one — the decision needs
    /// BOTH values, not one field). Everything else about the task —
    /// the draw sibling, the tick-axis sample clock — is identical.
    pub fn interp_pool_with<T: Pod + PartialEq + 'static>(
        &mut self,
        auth: &PoolHandle<T>,
        lerp: impl Fn(&T, &T, f32) -> T + 'static,
        delay: f64,
        extrap_max: f64,
    ) -> PoolHandle<T> {
        let draw = self.pool::<T>(&format!("{}.draw", auth.name()));
        // Per-pool phase name: several interp'd pools coexist and each
        // shows up on its own in task_stats.
        let name = format!("interp.{}", auth.name());
        let applied = self.pm.single::<AppliedLog>("net.applied");
        let tick_secs = 1.0 / f64::from(self.input_hz.max(1.0));
        let auth = auth.clone();
        let ret = draw.clone();
        let mut buf = InterpBuffer::<T>::new(delay);
        buf.extrap_max = extrap_max;
        // Newest applied label in server seconds, and the render-now
        // estimate on that same axis (None until the first snapshot).
        let mut label_time: Option<f64> = None;
        let mut clock: Option<f64> = None;
        self.pm.phase_present(&name, move |pm| {
            if let Some(t) = applied.get().0.iter().map(|a| a.tick).max() {
                let lt = t as f64 * tick_secs;
                label_time = Some(label_time.map_or(lt, |p| p.max(lt)));
            }
            let Some(target) = label_time else {
                return; // nothing applied yet — no time axis to be on
            };
            let now = {
                let c = clock.get_or_insert(target);
                *c += f64::from(pm.loop_dt());
                let err = target - *c;
                if err.abs() > 0.25 {
                    *c = target; // rejoin after a gap (reconnect, long stall)
                } else {
                    *c += err * 0.1; // soft slew onto the label stream
                }
                *c
            };
            pool_interp(&auth, &draw, &mut buf, target, now, &lerp);
        });
        ret
    }

    /// The client half of the engine-hosted params
    /// ([`PmServer::params`]): the replica single plus the typed write
    /// path. `get()` serves the pod's SHIPPED DEFAULTS until the first
    /// snapshot arrives (never zeros), so pre-connect reads at setup
    /// (interp delay, feel constants) are sane.
    pub fn params<P>(&mut self) -> ParamsClient<P>
    where
        P: Pod + PodSchema + pm_control_core::Tunable + 'static,
    {
        ParamsClient {
            replica: self.sync_single::<P>("pm.params"),
            tx: self.event::<ParamSet>("pm.param.set"),
        }
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
    /// (RTT rises by ~2x the lag). Surface these as CLI arguments (hogs:
    /// `lag=80 loss=0.03`); zero/zero means a clean link.
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
            session_token,
            replay,
        } = self;
        if let Some(path) = replay {
            let schema = pm.net_schema();
            let sync = std::mem::take(&mut *pm.single::<SyncSet>("net.sync").get_mut());
            NetClient::with_sync(sync).replay(&mut pm, &path, input_hz, &schema)?;
            pm.loop_run();
            return Ok(());
        }
        pm.connect(&addr, input_hz, link_lag, password, session_token)?;
        pm.loop_run();
        Ok(())
    }

    /// PLAY BACK a recording instead of connecting: `run` reads `path`
    /// (written by [`PmServer::record_to`](PmServer::record_to)) on the
    /// tick clock and applies its snapshot frames through the normal
    /// apply path — [`interp_pool`](PmClient::interp_pool), draw pools,
    /// HUD reads all work unchanged, which is the point: the demo
    /// format IS the wire format. The viewer must register the SAME
    /// wire schema as the recording server (checked like a live
    /// connect, mismatch names the channel). There is no input, no
    /// prediction (no avatar), and no redial; when the file ends the
    /// world freezes and [`ClientNet::lost`] reads "replay ended".
    pub fn replay_from(&mut self, path: &str) {
        self.replay = Some(path.to_string());
    }

    /// Present this session token at the handshake — the RECONNECT
    /// identity. Generate one random token per game session, keep it
    /// across redials, and pass it to every [`Pm::client`] you build for
    /// that session: a redial inside the server's grace window gets the
    /// same peer id back (and whatever the game parked under it). Left
    /// unset, the transport rolls a fresh random token — first connects
    /// don't need this call.
    pub fn session_token(&mut self, token: [u8; 16]) {
        self.session_token = Some(token);
    }

    /// Register a typed reliable event channel and return its **sender**.
    /// Events are one-way client→server: discrete, must-arrive intents the
    /// client originates ("respawn", "ready", "start") — the things that
    /// don't fit the continuous, lossy input channel. (Server→client facts
    /// stay state: a synced pool, TTL'd if they fade.) The returned
    /// [`EventTx`] queues an event the net task ships reliably once
    /// connected. Events are fixed-size pods; a short name is a
    /// `Name([u8; 24])`, not a `String`.
    pub fn event<E: Pod + PodSchema + 'static>(&mut self, name: &str) -> EventTx<E> {
        let tag = event_register(&mut self.pm, name, size_of::<E>(), E::SCHEMA_HASH);
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

/// Remove every entity whose entry in `pool` was last written more than
/// `ttl_ticks` ago. Removal goes through the normal deferred path
/// ([`Pm::id_remove`]), so on a server it replicates like any other
/// removal.
///
/// The clock is **ticks since last write** (the pool's change stamps): a
/// mutated entry refreshes its lifetime; the immutable transient facts
/// this exists for age from birth. Expiry removes the *entity*, not just
/// the pool entry — a TTL'd pool owns its entities (each occurrence gets a
/// fresh [`Id`]; see the contact-points rule in the crate netcode docs).
pub(crate) fn pool_expire<T: 'static>(pm: &mut Pm, pool: &PoolHandle<T>, ttl_ticks: u32) {
    let now = pm.tick();
    let expired: Vec<Id> = {
        let pool = pool.get();
        pool.ids()
            .iter()
            .zip(pool.changed_ticks())
            .filter(|&(_, &t)| now.saturating_sub(t) > ttl_ticks)
            .map(|(&id, _)| id)
            .collect()
    };
    for id in expired {
        pm.id_remove(id);
    }
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

    /// Host a [`pm_params!`](pm_control_core::pm_params) pod as THE
    /// server's live tuning set — the whole stack in one call (engine-
    /// owned since 2026-07-23; every game had hand-rolled it):
    /// - loads `path` (clamped `name=value` lines; missing file =
    ///   shipped defaults),
    /// - replicates the pod as the `"pm.params"` synced single (the
    ///   pod's `PodSchema` hash guards the handshake like any channel),
    /// - runs the CLAMP OF RECORD over the `"pm.param.set"` event:
    ///   clamped indexed writes, write-gated so the single re-ships
    ///   only on real change; the [`PARAM_SAVE`](crate::PARAM_SAVE)
    ///   sentinel rewrites `path`.
    ///
    /// Clients pair with [`PmClient::params`]. Sim tasks read the
    /// returned single; shared steps take `&P` from it (server single /
    /// client replica — the determinism story params were built on).
    pub fn params<P>(&mut self, path: &str) -> SingleHandle<P>
    where
        P: Pod + PodSchema + pm_control_core::Tunable + 'static,
    {
        let single = self.sync_single::<P>("pm.params");
        *single.get_mut() = params_load::<P>(path);
        let rx = self.event::<ParamSet>("pm.param.set");
        let path = path.to_string();
        let handle = single.clone();
        self.pm.phase_present("params", move |_pm| {
            for (_peer, ev) in rx.drain() {
                if ev.idx == PARAM_SAVE {
                    match params_save(&path, &*handle.get()) {
                        Ok(()) => eprintln!("[pm params] saved to {path}"),
                        Err(e) => eprintln!("[pm params] save FAILED ({path}): {e}"),
                    }
                    continue;
                }
                // Copy out, clamped indexed write, write back only on a
                // real change — the single stamps (and ships) only then.
                // Unknown idx (stale sender): set_clamped says false.
                let mut p = *handle.get();
                if p.set_clamped(ev.idx as usize, ev.value) {
                    *handle.get_mut() = p;
                    let spec = P::specs()[ev.idx as usize];
                    eprintln!(
                        "[pm params] {} = {}",
                        spec.name,
                        p.get(ev.idx as usize).unwrap_or(0.0)
                    );
                }
            }
        });
        single
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
        let name = format!("ttl.{}", pool.name());
        let pool = pool.clone();
        self.pm.phase_present(&name, move |pm| {
            pool_expire(pm, &pool, secs_ticks(pm, secs));
        });
    }

    /// Record `pool`'s TICK JOURNAL — a `secs`-deep ring of tick-stamped
    /// whole-pool frames — and return the [`JournalHandle`] handle that
    /// reads it. ONE ring per pool no matter how many consumers: a
    /// second call for the same pool shares the ring and widens the
    /// window to the larger `secs`. This is the "state at tick T" store
    /// every tick-addressed feature derives from (see [`JournalHandle`]).
    ///
    /// Frames are labeled like snapshots (`tick - 1`, the last completed
    /// tick, recorded right after the net task packs), so a frame label
    /// IS a snapshot tick a client may have seen. That makes it the
    /// lag-compensation memory: to judge an acting peer's view — "was I
    /// really on that car when I hit it?" — rewind to what they were
    /// looking at when they issued the input:
    ///
    /// ```text
    /// let view = net.acked_tick(peer)              // newest tick they HAD
    ///     .saturating_sub(interp_ticks);           // minus their interp delay
    /// let frame = journal.frame(view);             // other entities, as seen
    /// ```
    ///
    /// The interp delay is the client's presentation constant
    /// ([`interp_pool`](PmClient::interp_pool)'s `delay`, in ticks) —
    /// share it between both builds like the fixed dt. Rewinds deeper
    /// than the window clamp to the oldest frame (bounded rewind), and
    /// keep `secs` comfortably above the deepest honest rewind
    /// (~interp delay + a worst-case RTT).
    // TODO(simplify): the interp-delay CONTRACT is still folklore — the
    // client's interp delay and the server's rewind depth are the same
    // number, shared today by game convention (hogs' interp_delay()).
    // Engine-own it: the client reports its delay at the handshake, and
    // the journal grows `frame_seen_by(peer)` doing the
    // `acked_tick - delay_ticks` arithmetic itself, so games stop
    // hand-writing the lag-comp rewind recipe.
    pub fn journal_pool<T: Copy + 'static>(
        &mut self,
        pool: &PoolHandle<T>,
        secs: f32,
    ) -> JournalHandle<T> {
        type Entry<T> = (Rc<RefCell<Journal<T>>>, Rc<Cell<f32>>);
        let key = pool.name().to_string();
        if let Some(entry) = self.journals.get(&key) {
            let (ring, want) = entry
                .downcast_ref::<Entry<T>>()
                .expect("journal_pool: a pool journals one element type");
            want.set(want.get().max(secs));
            return JournalHandle { ring: ring.clone() };
        }
        let ring = Rc::new(RefCell::new(Journal::new(1)));
        let want = Rc::new(Cell::new(secs));
        self.journals
            .insert(key, Box::new((ring.clone(), want.clone()) as Entry<T>));
        let name = format!("journal.{}", pool.name());
        let pool = pool.clone();
        let handle = JournalHandle { ring: ring.clone() };
        // COMMIT phase: the frame captures the tick's FINAL state,
        // labeled `tick` — the same label this tick's snapshots carry.
        self.pm.phase_commit(&name, move |pm| {
            let mut ring = ring.borrow_mut();
            ring.cap_set(secs_ticks(pm, want.get()).max(1) as usize);
            let frame: Vec<(Id, T)> = pool.get().iter().map(|(id, v)| (id, *v)).collect();
            ring.push(pm.tick(), frame);
        });
        handle
    }

    /// Give `pool` a per-peer INTEREST scorer (v2 item 4): `score(peer,
    /// id, value)` returns a POSITIVE importance, and the snapshot
    /// packer visits that pool's dirty entries in `importance ×
    /// staleness` order instead of plain rotation — what fills the byte
    /// budget first is what matters most to THAT peer (distance,
    /// recency, on-screen-ness; the classic priority-accumulator).
    /// Staleness (ticks since the peer confirmed the entry) multiplies
    /// in so a low-importance entry sends at a lower cadence, never
    /// never — the starvation lesson is load-bearing here. The budget
    /// still does ALL the throttling; cross-pool fairness
    /// (smallest-dirty-first) is unchanged.
    ///
    /// Call after the pool's `sync_pool`/`wire_pool` registration
    /// (panics on an unknown pool or a mismatched element type). The
    /// scorer runs inside the net task's pack, per peer per datagram —
    /// keep it cheap, and read other pools through cloned handles only
    /// (never the pool being scored).
    pub fn interest_pool<T: 'static>(
        &mut self,
        pool: &PoolHandle<T>,
        score: impl Fn(u8, Id, &T) -> f32 + 'static,
    ) {
        let f: Rc<dyn Fn(u8, Id, &T) -> f32> = Rc::new(score);
        self.pm
            .single::<SyncSet>("net.sync")
            .get_mut()
            .interest(pool.name(), f);
    }

    /// Register the **continuous input channel** and return its receiver.
    /// `C` is the input pod clients send (same name and pod on both ends —
    /// the handshake schema enforces it). Exactly one continuous channel
    /// per connection (a second registration panics); see
    /// [`PmClient::input`].
    pub fn input<C: Pod + PodSchema + 'static>(&mut self, name: &str) -> InputRx<C> {
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

    /// How long after a disconnect a session token may reclaim its peer
    /// id (default 20 s). Keep the game's avatar-parking window in step
    /// — the engine remembers WHO a returning connection is; parking
    /// what they owned is the game's half of reconnect.
    pub fn reconnect_grace(&mut self, secs: f32) {
        self.grace = Some(secs);
    }

    /// RECORD this session to `path` (v2 item 2: recordings derive from
    /// the snapshot stream — see [`RECORDER_PEER`](crate::RECORDER_PEER)
    /// for why the file starts with a free keyframe and stays pure
    /// deltas). Play it back with
    /// [`PmClient::replay_from`](PmClient::replay_from) — the viewer
    /// needs the same registered schema, checked like a live connect.
    /// Call before [`run`](PmServer::run); the file is created there.
    pub fn record_to(&mut self, path: &str) {
        self.record = Some(path.to_string());
    }

    /// Bind the endpoint (the schema is complete now) and run the loop.
    /// Returns when the loop quits, `Err` on bind failure. (The simulated
    /// link is the CLIENT's — see `serve`; the server never lags itself.)
    pub fn run(self) -> std::io::Result<()> {
        let PmServer { mut pm, addr, password, grace, record, .. } = self;
        pm.serve(&addr, password, grace, record)?;
        pm.loop_run();
        Ok(())
    }

    /// Create a replicated singleton the server owns. The replica side
    /// reads it through [`PmClient::sync_single`]'s typed handle.
    pub fn sync_single<T: Pod + PodSchema + Default + 'static>(&mut self, name: &str) -> SingleHandle<T> {
        let single = self.pm.single::<T>(name);
        self.pm.sync(single.pool());
        single
    }

    /// Register a typed reliable event channel and return its **receiver**.
    /// Events are one-way client→server (see [`PmClient::event`]); drain the
    /// returned [`EventRx`] from a server task to read this tick's events,
    /// each tagged with the sender peer.
    pub fn event<E: Pod + PodSchema + 'static>(&mut self, name: &str) -> EventRx<E> {
        let tag = event_register(&mut self.pm, name, size_of::<E>(), E::SCHEMA_HASH);
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
fn event_register(pm: &mut Pm, name: &str, size: usize, hash: u64) -> u16 {
    pm.single::<WireReg>("net.reg")
        .get_mut()
        .register(WireKind::Event, name, size, hash);
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
    /// is non-destructive (each receiver filters its own tag), valid for
    /// the whole task cycle (the PRESENT phase fills it).
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
    /// Latest view pose the peer reported (see [`ClientNet::view_set`]).
    pub view: Option<[f32; 6]>,
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
    /// Why the connection ended, once it has ("server closed…", a QUIC
    /// error). The net task fills this and quits the loop; the single
    /// outlives the `Pm` through cloned handles, so the game reads it
    /// AFTER `run()` returns to decide whether to redial (reconnect)
    /// or surface the reason.
    pub lost: Option<String>,
    /// This peer's controlled entity, as the server marked it (see
    /// [`ServerNet::own_set`]). `None` until the first snapshot that
    /// carries one. [`ClientNet::mine`] is the sugar most game code reads.
    pub avatar: Option<Id>,
    /// The full peer→controlled-entity table from the newest snapshot
    /// header, sorted by peer — every player's ownership, not just ours.
    /// Read through [`ClientNet::owner_of`]/[`own`](ClientNet::own)/
    /// [`owned`](ClientNet::owned).
    pub owners: Vec<(u8, Id)>,
    /// View pose to report to the server (`[eye xyz, forward xyz]`) —
    /// set via [`ClientNet::view_set`]; the net task ships it at the
    /// input cadence as a newest-wins datagram. `None` = never report
    /// (bots, tools).
    pub view: Option<[f32; 6]>,
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
    pub fn serve(self, pm: &mut Pm, quic: QuicServer) {
        let mut net = self;
        let mut rec = net.record.take();
        let mut header_done = false;
        if rec.is_some() {
            net.peer_add(RECORDER_PEER);
        }
        let peers = pm.single::<PeerEvents>("net.peers");
        let events = pm.single::<ServerEvents>("net.events");
        let own = pm.single::<ServerOwn>("net.own");
        let commit_own = own.clone();
        let stats = pm.single::<PeerStats>("net.peerstat");
        let tune = pm.single::<SendTune>("net.sendtune");
        let sink = pm.single::<ServerInputChan>("net.input").get().0.clone();
        // One NetServer + endpoint, shared by the two phase closures —
        // receive in PRESENT (the sim reads a fresh world), send in
        // COMMIT (snapshots carry THIS tick's state: the phase turn
        // reclaimed the tick of staleness the old net-task-first
        // ordering paid).
        let shared = Rc::new(RefCell::new((net, quic)));
        let commit_shared = shared.clone();
        let commit_sink = sink.clone();
        pm.phase_present_front("net.recv", move |_pm| {
            let (net, quic) = &mut *shared.borrow_mut();
            quic.pump();
            {
                let mut pe = peers.get_mut();
                // Leaves reported last tick have had a full task cycle
                // with ownership intact (games despawn via
                // `own(p)`); drop their entries now — peer ids recycle,
                // and a stale entry would mark the departed player's
                // entity as the NEXT player's own. Runs before this
                // tick's joins, so a same-id rejoin starts clean.
                for &p in &pe.left {
                    own.get_mut().clear(p);
                }
                pe.joined.clear();
                pe.left.clear();
                // Lefts BEFORE joins: a reconnect can reclaim its peer
                // id the same tick the old connection's leave lands
                // (the transport always orders the leave first), and
                // processing the leave second would tear down the
                // fresh peer. Games handling both lists should park on
                // `left` before adopting on `joined` for the same
                // reason.
                for p in quic.left_drain() {
                    net.peer_remove(p);
                    if let Some(s) = &sink {
                        s.peer_remove(p);
                    }
                    pe.left.push(p);
                }
                for p in quic.joined_drain() {
                    // Reset-on-join: a `joined` for a peer id we still
                    // hold is a reconnect that superseded its old
                    // connection (no leave fired). The new client
                    // starts empty, so its net state must too — full
                    // reconvergence IS the baseline mechanism.
                    net.peer_remove(p);
                    net.peer_add(p);
                    if let Some(s) = &sink {
                        s.peer_remove(p);
                        s.peer_add(p);
                    }
                    pe.joined.push(p);
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
                }
            }
            // The recorder (if any) is a virtual peer: keep it out of
            // stats, flights, and the net doctor.
            let plist: Vec<u8> = net.peers().filter(|&p| p != RECORDER_PEER).collect();
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
                    e.view = quic.view(p);
                }
            }
            {
                let ev = &mut events.get_mut().0;
                ev.clear();
                ev.extend(quic.events_drain());
            }
        });
        pm.phase_commit_front("net.send", move |pm| {
            let (net, quic) = &mut *commit_shared.borrow_mut();
            // Echo what the sim consumed THIS tick (the task cycle just
            // ran) — the phase turn made the reconcile echo one tick
            // fresher for free.
            if let Some(s) = &commit_sink {
                for (p, seq) in s.applied_seqs() {
                    net.input_processed(p, seq);
                }
            }
            let plist: Vec<u8> = net.peers().filter(|&p| p != RECORDER_PEER).collect();
            // Ship the whole peer→entity table in every header (same
            // bytes for all peers); sorted inside owners_set so the
            // HashMap's iteration order never reaches the wire.
            net.owners_set(commit_own.get().0.iter().map(|(&p, &id)| (p, id.0)).collect());
            // RECORDING: one unbounded-budget frame per tick to disk,
            // self-acked immediately so every frame is exactly that
            // tick's changes and removal recycling never waits on the
            // file. Runs after owners_set (headers must carry the
            // table) and before the flights (same tick label).
            if let Some((w, schema)) = &mut rec {
                use std::io::Write;
                if !header_done {
                    header_done = true;
                    let _ = w.write_all(b"PMREC\x01");
                    let _ = w.write_all(&(pm.loop_rate as u32).to_le_bytes());
                    let _ = w.write_all(&(schema.len() as u32).to_le_bytes());
                    let _ = w.write_all(schema);
                }
                if let Some(snap) = net.snapshot_budgeted(pm, RECORDER_PEER, usize::MAX) {
                    let label = u32::from_le_bytes(snap.bytes[0..4].try_into().unwrap());
                    let seq = u32::from_le_bytes(snap.bytes[4..8].try_into().unwrap());
                    let _ = w.write_all(&(snap.bytes.len() as u32).to_le_bytes());
                    let _ = w.write_all(&snap.bytes);
                    let _ = w.flush();
                    net.ack(RECORDER_PEER, label, seq);
                }
            }
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
    /// Install a REPLAY net task: read `path` (a `PMREC` file written
    /// by the server recorder) and apply one recorded frame per sim
    /// tick, paced by `hz` against the loop clock — the file-reading
    /// sibling of [`connect`](NetClient::connect). `schema` is this
    /// viewer's registered wire schema; it must equal the recording's
    /// (byte compare, same rule as the live handshake) or this returns
    /// a named-diff error instead of misparsing frames. Fills the same
    /// `"net.*"` singles a live connection would (applied log, owner
    /// table, connected flag), so presentation code can't tell the
    /// difference.
    pub fn replay(
        self,
        pm: &mut Pm,
        path: &str,
        hz: f32,
        schema: &[(u8, String, usize, u64)],
    ) -> std::io::Result<()> {
        use std::io::Read;
        let file = std::fs::File::open(path)?;
        let mut r = std::io::BufReader::new(file);
        let mut magic = [0u8; 6];
        r.read_exact(&mut magic)?;
        if &magic != b"PMREC\x01" {
            return Err(std::io::Error::other("not a pm recording (bad magic)"));
        }
        let mut w4 = [0u8; 4];
        r.read_exact(&mut w4)?;
        let _recorded_rate = u32::from_le_bytes(w4); // informational
        r.read_exact(&mut w4)?;
        let slen = u32::from_le_bytes(w4) as usize;
        let mut theirs = vec![0u8; slen];
        r.read_exact(&mut theirs)?;
        let mine = crate::transport::schema_encode(schema);
        if theirs != mine {
            use crate::transport::{schema_decode, schema_diff};
            let diff = match (schema_decode(&theirs), schema_decode(&mine)) {
                (Some(t), Some(m)) => schema_diff(&t, &m),
                _ => "undecodable schema".into(),
            };
            return Err(std::io::Error::other(format!(
                "recording schema mismatch: {diff}"
            )));
        }

        let net = self;
        let status = pm.single::<NetStatus>("net.status");
        let applied = pm.single::<AppliedLog>("net.applied");
        let step = 1.0 / hz.max(1.0);
        let mut accum = 0.0f32;
        // Read-ahead of one frame; `None` after EOF.
        let mut next_frame = |r: &mut std::io::BufReader<std::fs::File>| -> Option<Vec<u8>> {
            let mut w4 = [0u8; 4];
            r.read_exact(&mut w4).ok()?;
            let mut frame = vec![0u8; u32::from_le_bytes(w4) as usize];
            r.read_exact(&mut frame).ok()?;
            Some(frame)
        };
        let mut pending = next_frame(&mut r);
        // Play label — the recorded tick currently being shown; frames
        // apply as the label reaches them.
        let mut play: Option<u32> = None;
        pm.phase_present_front("net.replay", move |pm| {
            applied.get_mut().0.clear();
            {
                let mut st = status.get_mut();
                st.connected = true;
            }
            // Advance the play clock at the recorded cadence, whatever
            // rate this loop renders at.
            accum += pm.loop_dt();
            let mut ticks = 0u32;
            while accum >= step {
                accum -= step;
                ticks += 1;
            }
            if play.is_none() {
                // First frame: adopt its label so playback starts at
                // the recording's keyframe immediately.
                play = pending
                    .as_ref()
                    .and_then(|f| f.get(0..4))
                    .map(|b| u32::from_le_bytes(b.try_into().unwrap()));
                ticks = 0;
            }
            let Some(p) = &mut play else { return };
            *p = p.saturating_add(ticks);
            // Apply every frame due at (or before) the play label.
            while let Some(frame) = &pending {
                let label = frame
                    .get(0..4)
                    .map_or(u32::MAX, |b| u32::from_le_bytes(b.try_into().unwrap()));
                if label > *p {
                    break;
                }
                if let Ok(a) = net.apply(pm, frame) {
                    let mut st = status.get_mut();
                    st.owners = a.owners.clone();
                    st.snapshots += 1;
                    drop(st);
                    applied.get_mut().0.push(a);
                }
                pending = next_frame(&mut r);
                if pending.is_none() {
                    // End of the recording: freeze the world and say so
                    // (viewers read `ClientNet::lost`; Escape still
                    // quits the loop the normal way).
                    status
                        .get_mut()
                        .lost
                        .get_or_insert_with(|| "replay ended".into());
                }
            }
        });
        Ok(())
    }

    /// Install the client net module: moves `self` and the endpoint
    /// into a net task that pumps the wire and trades in the `"net.*"`
    /// singles (see module docs). The input channel (if registered) is
    /// sampled at a fixed `input_hz` cadence (match the server's sim
    /// rate — 60.0 for a 60 Hz server). Connection errors quit the loop.
    pub fn connect(self, pm: &mut Pm, quic: QuicClient, input_hz: f32) {
        let net = self;
        let status = pm.single::<NetStatus>("net.status");
        let commit_status = status.clone();
        let applied = pm.single::<AppliedLog>("net.applied");
        let out = pm.single::<Outbox>("net.out");
        let chan = pm.single::<ClientInputChan>("net.input").get().0.clone();
        let commit_chan = chan.clone();
        let tune = pm.single::<LinkTune>("net.linktune");
        let mut tune_seq = 0u32;
        // Net-doctor state: (applied count, newest label) at last report.
        let mut dbg_prev = (0u32, 0u32);
        let mut dbg_newest = 0u32;
        // One NetClient + endpoint + input-cadence accumulator, shared
        // by the two phase closures — receive/apply in PRESENT, send in
        // COMMIT so the input the game set THIS tick leaves THIS tick
        // (the phase turn's other reclaimed tick of latency).
        let shared = Rc::new(RefCell::new((net, quic, 0.0f32)));
        let commit_shared = shared.clone();
        pm.phase_present_front("net.recv", move |pm| {
            let (net, quic, accum) = &mut *shared.borrow_mut();
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
                let mut st = status.get_mut();
                st.connected = false;
                st.lost = Some(err.to_string());
                drop(st);
                pm.loop_quit();
                return;
            }
            if quic.is_gone() {
                eprintln!("[net] server closed the connection");
                let mut st = status.get_mut();
                st.connected = false;
                st.lost
                    .get_or_insert_with(|| "server closed the connection".into());
                drop(st);
                pm.loop_quit();
                return;
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
                // The input-cadence clock advances in PRESENT so the
                // render extrapolation (`input_alpha`, clamped by its
                // readers) sees this tick's dt; the sends it schedules
                // happen in COMMIT, after the game's input task ran.
                *accum += pm.loop_dt();
                status.get_mut().input_alpha = *accum * input_hz;
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
        pm.phase_commit_front("net.send", move |_pm| {
            let (_net, quic, accum) = &mut *commit_shared.borrow_mut();
            if quic.handshake_done().is_none() || quic.is_gone() {
                return;
            }
            // Reliable events queued during this tick's tasks.
            for (ty, payload) in out.get_mut().drain() {
                quic.event_send(ty, &payload);
            }
            // Fixed-cadence input: the server consumes one per sim
            // tick, and prediction must step the same fixed dt —
            // regardless of what rate this loop runs at. Sent HERE,
            // after the game's input task set this tick's pod (the
            // predictor's replay phase runs right after and picks up
            // the seqs these sends assign).
            if let Some(ch) = &commit_chan {
                ch.frame_begin();
            }
            let step = 1.0 / input_hz;
            let mut ticked = false;
            while *accum >= step {
                *accum -= step;
                ticked = true;
                if let Some(ch) = &commit_chan {
                    ch.send(quic);
                }
            }
            // View pose rides the same cadence (newest-wins, so once
            // per burst is enough).
            if ticked && let Some(pose) = commit_status.get().view {
                quic.view_send(pose);
            }
            commit_status.get_mut().input_alpha = *accum * input_hz;
        });
    }
}
