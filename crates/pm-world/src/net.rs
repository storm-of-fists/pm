//! Headless sync layer: server-authoritative snapshot-delta replication.
//! Transport-agnostic — `NetServer` produces byte buffers and `NetClient`
//! consumes them; QUIC (quinn-proto) will carry them later.
//!
//! Model (the README networking notes): per peer the server tracks, per
//! entity slot, the change-tick the peer last confirmed and the one in
//! flight (Tribes-style prioritized replication, per-entity rather than
//! Quake 3's per-client snapshot diffs). A snapshot carries unconfirmed
//! entries in rotation order up to a byte budget, plus unacked removals.
//! Every send also carries a per-peer **send sequence** — one tick may
//! send SEVERAL snapshots (a flight: the datagram is size-capped, so
//! freshness scales by count, not size) and the tick label alone would
//! be ambiguous between them. An ack echoes (tick, seq): it confirms
//! exactly what that one send carried and declares older unacked sends
//! lost (their entries resend). Everything is an upsert, so snapshots
//! are idempotent. Change-sparse pools converge to silence; change-dense
//! pools stream through the budget round-robin — one mechanism, both
//! behaviors. Pools pack smallest-dirty-first each snapshot, so a
//! change-dense pool (a horde) takes what the sparse pools left over —
//! registration order never decides who fits (a dense pool registered
//! early would otherwise starve every pool after it, forever). The
//! single `acked_tick` cursor remains only as the removal-log gate and
//! the lag-comp anchor.
//!
//! Tick semantics: a snapshot is labeled `pm.tick() - 1` — the last
//! *completed* tick — because stamps from the in-progress tick may still
//! be written by tasks that haven't run yet. Entries from the current tick
//! may ride along early; the conservative label only means they get sent
//! again next time, never lost. Run the net-send task at low priority
//! (first in the tick) to avoid the duplicate sends entirely.

// TODO(roadmap): known limits, deliberate until a workload demands
// otherwise — per-peer pack scan is O(entities × pools) per DATAGRAM,
// and a multi-datagram flight multiplies it (plus the dirty_bytes
// counting scan per send): fine at horde scale, the first thing to fold
// into a per-tick dirty journal near ~10k entities (the tick journal —
// `PmServer::journal_pool` — is the store that scan would derive from,
// the v2 item-2 stage still queued); removal recycling is
// ack-gated ONLY (a peer that stops acking without disconnecting stalls
// id recycling until the idle timeout reaps it — an ack-OR-timer release
// is the fix if that ever bites); u32 ticks and send seqs last ~2.2
// years. (Reconnect LANDED 2026-07-22 as the pm/3 session-token
// handshake: same token inside the grace window → same peer id, fresh
// cursors, and full reconvergence — the delta cursors were the baseline
// mechanism all along. See transport.rs FRAME_AUTH/FRAME_WELCOME.)
// (Foveal relevancy LANDED 2026-07-22 as predicted — a SORT KEY, not a
// scheduler: `PmServer::interest_pool` installs a per-peer scorer and
// `pack_dirty` visits dirty entries in importance × staleness order;
// the budget keeps doing all the throttling. NOT per-entity due-times,
// NOT culling. The angle-off-view-center refinement still wants the
// client view-pose report — just another input channel, when needed.)
// TODO(roadmap): both sync modifiers are LANDED — interp as
// `PmClient::interp_pool`, duration as `PmServer::ttl_pool` (transient
// entries expire) + `PmServer::history_pool` (past-tick ring; rewind to
// `ServerNet::acked_tick(peer) - interp ticks` = lag-compensated contact
// resolution, proven in drive's scoring). The ownership table replicates
// whole in every snapshot header (2026-07-14) — `ClientNet::owner_of`
// replaced the hand-carried peer fields avatars used to need.
// TODO(roadmap): recording, playback, and saving (requested 2026-07-15).
// The machinery is already here, don't invent a second format:
// - RECORDING: the snapshot stream IS the demo format — a recorder is a
//   virtual peer with an unbounded budget whose snapshots get written to
//   disk (tick labels included) instead of a socket; per-peer inputs
//   alongside give a full replay of causes, not just effects.
// - PLAYBACK: a client whose net task reads that file on the tick clock
//   instead of a QUIC endpoint — apply() and interp_pool work unchanged.
// - SAVING: a world save is "every synced pool + id state at one tick" —
//   the SyncSet adapters already serialize exactly that (a save is an
//   unbudgeted keyframe snapshot; loading is apply() into a fresh Pm).
use std::any::Any;
use std::cell::RefCell;
use std::collections::{BTreeMap, VecDeque};
use std::rc::Rc;

use bytemuck::Pod;

use crate::id::Id;
use crate::kernel::{Pm, PoolHandle};
use crate::paged::PagedArray;
use crate::pool::Pool;
use crate::transport::EVENT_USER_BASE;

/// Client-side reliable-event outbox (the `"net.out"` single): `EventTx`
/// senders push tagged frames; the net task — the one owner of the QUIC
/// handle — drains it and sends once connected. Internal plumbing behind
/// the typed event channels; games never touch it.
#[derive(Default)]
pub struct Outbox {
    events: Vec<(u16, Vec<u8>)>,
}

impl Outbox {
    pub fn send(&mut self, ty: u16, payload: &[u8]) {
        self.events.push((ty, payload.to_vec()));
    }

    pub fn drain(&mut self) -> Vec<(u16, Vec<u8>)> {
        std::mem::take(&mut self.events)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum NetError {
    Truncated,
    /// Snapshot referenced a pool key (`pool_key`) this end never
    /// registered with `sync`.
    UnknownPool(u16),
}

// --- byte reading -------------------------------------------------------

struct Reader<'a> {
    data: &'a [u8],
}

impl<'a> Reader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data }
    }

    fn bytes(&mut self, n: usize) -> Result<&'a [u8], NetError> {
        if self.data.len() < n {
            return Err(NetError::Truncated);
        }
        let (head, rest) = self.data.split_at(n);
        self.data = rest;
        Ok(head)
    }

    fn u8(&mut self) -> Result<u8, NetError> {
        Ok(self.bytes(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, NetError> {
        Ok(u16::from_le_bytes(self.bytes(2)?.try_into().unwrap()))
    }

    fn u32(&mut self) -> Result<u32, NetError> {
        Ok(u32::from_le_bytes(self.bytes(4)?.try_into().unwrap()))
    }
}

// --- type-erased pool sync adapters ------------------------------------

/// Per-peer, per-pool replication state — the unification of "delta"
/// and "stream" behavior. For every entity slot the server tracks the
/// change-tick the peer last *confirmed* (acked a snapshot carrying it)
/// and the change-tick currently *in flight* (sent, ack pending). An
/// entry needs sending iff it changed past both. Change-sparse pools
/// converge to silence; change-dense pools rotate through the byte
/// budget via `cursor` — same bookkeeping, opposite emergent behavior.
struct PeerPool {
    confirmed: PagedArray<u32>,
    inflight_tick: PagedArray<u32>,
    /// Send sequence of the snapshot carrying this slot's in-flight
    /// entry — seq, not tick label, because a flight puts several sends
    /// inside one tick.
    inflight_seq: PagedArray<u32>,
    /// Dense-index rotation start, so a budget-limited snapshot resumes
    /// where the last one stopped instead of restarting at entity 0.
    cursor: usize,
}

impl PeerPool {
    fn new() -> Self {
        Self {
            confirmed: PagedArray::new(0),
            inflight_tick: PagedArray::new(0),
            inflight_seq: PagedArray::new(0),
            cursor: 0,
        }
    }
}

/// (pool index, slot, change-tick) recorded per snapshot so an ack can
/// confirm exactly what that snapshot carried.
type SentEntry = (u16, u32, u32);

/// Shared eligibility scan behind [`SyncAdapter::dirty_bytes`]: wire bytes
/// for the entries that changed past both the confirmed and the in-flight
/// tick — the same predicate `pack_dirty` sends by, minus the packing.
fn dirty_bytes<T>(pool: &Pool<T>, pp: &PeerPool, entry_size: usize) -> usize {
    let ids = pool.ids();
    let ticks = pool.changed_ticks();
    let mut n = 0usize;
    for (i, id) in ids.iter().enumerate() {
        let tick = ticks[i];
        let slot = id.slot();
        if tick > pp.confirmed.get(slot) && tick > pp.inflight_tick.get(slot) {
            n += 1;
        }
    }
    n * entry_size
}

/// A per-peer interest scorer (v2 item 4): game-defined POSITIVE
/// importance for an entry as seen by `peer`. The packer multiplies it
/// by the entry's STALENESS (ticks since that peer last confirmed it) —
/// the priority-accumulator model — so low-importance entries send at a
/// lower cadence instead of never, and importance decides who fills the
/// budget first. Install via `PmServer::interest_pool`.
type InterestFn<T> = Rc<dyn Fn(u8, Id, &T) -> f32>;

trait SyncAdapter {
    fn name(&self) -> &str;
    #[cfg(test)] // test seam (SyncSet::schema)
    fn value_size(&self) -> usize;
    /// Wire bytes this pool would pack for `pp` given no budget — the
    /// sort key for smallest-dirty-first packing (see `snapshot_budgeted`).
    fn dirty_bytes(&self, pp: &PeerPool) -> usize;
    /// Append `[id u32][value]` entries the peer hasn't confirmed, in
    /// rotation order, while `budget` lasts; returns count. `seq` is the
    /// send this packing belongs to (stamped per entry so an ack can
    /// settle exactly this send).
    fn pack_dirty(
        &self,
        pp: &mut PeerPool,
        peer: u8,
        now: u32,
        seq: u32,
        budget: &mut usize,
        out: &mut Vec<u8>,
        sent: &mut Vec<SentEntry>,
        pool_idx: u16,
    ) -> u32;
    fn apply(&self, pm: &mut Pm, count: u32, r: &mut Reader) -> Result<(), NetError>;
    /// Install an interest scorer; `f` is an `&InterestFn<T>` behind
    /// `Any` — false when `T` isn't this adapter's element type.
    fn interest_set(&mut self, f: &dyn Any) -> bool;
}

struct PoolAdapter<T: Pod> {
    name: String,
    pool: Rc<RefCell<Pool<T>>>,
    interest: Option<InterestFn<T>>,
}

impl<T: Pod> SyncAdapter for PoolAdapter<T> {
    fn name(&self) -> &str {
        &self.name
    }

    #[cfg(test)]
    fn value_size(&self) -> usize {
        size_of::<T>()
    }

    fn dirty_bytes(&self, pp: &PeerPool) -> usize {
        dirty_bytes(&self.pool.borrow(), pp, 4 + size_of::<T>())
    }

    fn pack_dirty(
        &self,
        pp: &mut PeerPool,
        peer: u8,
        now: u32,
        seq: u32,
        budget: &mut usize,
        out: &mut Vec<u8>,
        sent: &mut Vec<SentEntry>,
        pool_idx: u16,
    ) -> u32 {
        let pool = self.pool.borrow();
        let ids = pool.ids();
        let values = pool.values();
        let ticks = pool.changed_ticks();
        let n = ids.len();
        if n == 0 {
            return 0;
        }
        let entry_size = 4 + size_of::<T>();
        let mut count = 0u32;
        // Interest (v2 item 4): visit dirty entries in importance ×
        // staleness order instead of rotation — the budget still does
        // ALL the throttling, the score only decides who goes first,
        // and staleness guarantees nothing starves.
        if let Some(score) = &self.interest {
            let mut order: Vec<(f32, usize)> = Vec::new();
            for i in 0..n {
                let tick = ticks[i];
                let slot = ids[i].slot();
                if tick <= pp.confirmed.get(slot) || tick <= pp.inflight_tick.get(slot) {
                    continue;
                }
                let stale = now.saturating_sub(pp.confirmed.get(slot)) as f32;
                order.push((score(peer, ids[i], &values[i]) * (1.0 + stale), i));
            }
            order.sort_unstable_by(|a, b| b.0.total_cmp(&a.0));
            for (_, i) in order {
                if *budget < entry_size {
                    break;
                }
                let slot = ids[i].slot();
                *budget -= entry_size;
                out.extend_from_slice(&ids[i].0.to_le_bytes());
                out.extend_from_slice(bytemuck::bytes_of(&values[i]));
                pp.inflight_tick.set(slot, ticks[i]);
                pp.inflight_seq.set(slot, seq);
                sent.push((pool_idx, slot, ticks[i]));
                count += 1;
            }
            return count;
        }
        let start = if pp.cursor >= n { 0 } else { pp.cursor };
        let mut resume_at = start;
        for k in 0..n {
            let i = (start + k) % n;
            let tick = ticks[i];
            let slot = ids[i].slot();
            if tick <= pp.confirmed.get(slot) || tick <= pp.inflight_tick.get(slot) {
                continue;
            }
            if *budget < entry_size {
                resume_at = i;
                break;
            }
            *budget -= entry_size;
            out.extend_from_slice(&ids[i].0.to_le_bytes());
            out.extend_from_slice(bytemuck::bytes_of(&values[i]));
            pp.inflight_tick.set(slot, tick);
            pp.inflight_seq.set(slot, seq);
            sent.push((pool_idx, slot, tick));
            count += 1;
        }
        pp.cursor = resume_at;
        count
    }

    fn apply(&self, pm: &mut Pm, count: u32, r: &mut Reader) -> Result<(), NetError> {
        let mut pool = self.pool.borrow_mut();
        for _ in 0..count {
            let id = Id(r.u32()?);
            let value: T = bytemuck::pod_read_unaligned(r.bytes(size_of::<T>())?);
            pm.id_sync(id);
            pool.add(id, value);
        }
        Ok(())
    }

    fn interest_set(&mut self, f: &dyn Any) -> bool {
        match f.downcast_ref::<InterestFn<T>>() {
            Some(f) => {
                self.interest = Some(f.clone());
                true
            }
            None => false,
        }
    }
}

/// A synced pod with a compact wire representation: the pool keeps the
/// game's ergonomic struct (full-precision `f32` fields); `Repr` is the
/// small pod that actually rides the wire, converted at the pack/apply
/// boundary only. Derive it — `#[derive(pm::Wire)]` with per-field
/// `#[wire(i16, scale = 64.0)]` quantization attributes — and register
/// with [`wire_pool`](crate::kernel::Pm::wire_pool) instead of
/// `sync_pool`. Both ends must use `wire_pool`: the handshake schema
/// carries the REPR size, so a mismatched end is rejected loudly.
///
/// Quantization is lossy by design (the client sees `Repr`-precision
/// values; the server keeps full precision locally), so quantize
/// server-owned pools — for a *predicted* pool the reconcile threshold
/// must exceed the quantization step or corrections never settle.
pub trait Wire: Copy + 'static {
    /// The pod that rides the wire in place of `Self`.
    type Repr: Pod;
    fn to_repr(&self) -> Self::Repr;
    fn from_repr(repr: Self::Repr) -> Self;
}

/// [`PoolAdapter`] for `T: Wire` — identical bookkeeping, but entries
/// cross the wire as `T::Repr` (quantized at pack, dequantized at apply).
struct WireAdapter<T: Wire> {
    name: String,
    pool: Rc<RefCell<Pool<T>>>,
    /// Scores the GAME value (full precision), not the wire repr.
    interest: Option<InterestFn<T>>,
}

impl<T: Wire> SyncAdapter for WireAdapter<T> {
    fn name(&self) -> &str {
        &self.name
    }

    #[cfg(test)]
    fn value_size(&self) -> usize {
        size_of::<T::Repr>()
    }

    fn dirty_bytes(&self, pp: &PeerPool) -> usize {
        dirty_bytes(&self.pool.borrow(), pp, 4 + size_of::<T::Repr>())
    }

    fn pack_dirty(
        &self,
        pp: &mut PeerPool,
        peer: u8,
        now: u32,
        seq: u32,
        budget: &mut usize,
        out: &mut Vec<u8>,
        sent: &mut Vec<SentEntry>,
        pool_idx: u16,
    ) -> u32 {
        let pool = self.pool.borrow();
        let ids = pool.ids();
        let values = pool.values();
        let ticks = pool.changed_ticks();
        let n = ids.len();
        if n == 0 {
            return 0;
        }
        let entry_size = 4 + size_of::<T::Repr>();
        let mut count = 0u32;
        // Interest: importance × staleness order (see PoolAdapter — the
        // horde pools are wire pools, so this is the path that matters).
        if let Some(score) = &self.interest {
            let mut order: Vec<(f32, usize)> = Vec::new();
            for i in 0..n {
                let tick = ticks[i];
                let slot = ids[i].slot();
                if tick <= pp.confirmed.get(slot) || tick <= pp.inflight_tick.get(slot) {
                    continue;
                }
                let stale = now.saturating_sub(pp.confirmed.get(slot)) as f32;
                order.push((score(peer, ids[i], &values[i]) * (1.0 + stale), i));
            }
            order.sort_unstable_by(|a, b| b.0.total_cmp(&a.0));
            for (_, i) in order {
                if *budget < entry_size {
                    break;
                }
                let slot = ids[i].slot();
                *budget -= entry_size;
                out.extend_from_slice(&ids[i].0.to_le_bytes());
                out.extend_from_slice(bytemuck::bytes_of(&values[i].to_repr()));
                pp.inflight_tick.set(slot, ticks[i]);
                pp.inflight_seq.set(slot, seq);
                sent.push((pool_idx, slot, ticks[i]));
                count += 1;
            }
            return count;
        }
        let start = if pp.cursor >= n { 0 } else { pp.cursor };
        let mut resume_at = start;
        for k in 0..n {
            let i = (start + k) % n;
            let tick = ticks[i];
            let slot = ids[i].slot();
            if tick <= pp.confirmed.get(slot) || tick <= pp.inflight_tick.get(slot) {
                continue;
            }
            if *budget < entry_size {
                resume_at = i;
                break;
            }
            *budget -= entry_size;
            out.extend_from_slice(&ids[i].0.to_le_bytes());
            out.extend_from_slice(bytemuck::bytes_of(&values[i].to_repr()));
            pp.inflight_tick.set(slot, tick);
            pp.inflight_seq.set(slot, seq);
            sent.push((pool_idx, slot, tick));
            count += 1;
        }
        pp.cursor = resume_at;
        count
    }

    fn apply(&self, pm: &mut Pm, count: u32, r: &mut Reader) -> Result<(), NetError> {
        let mut pool = self.pool.borrow_mut();
        for _ in 0..count {
            let id = Id(r.u32()?);
            let repr: T::Repr = bytemuck::pod_read_unaligned(r.bytes(size_of::<T::Repr>())?);
            pm.id_sync(id);
            pool.add(id, T::from_repr(repr));
        }
        Ok(())
    }

    fn interest_set(&mut self, f: &dyn Any) -> bool {
        match f.downcast_ref::<InterestFn<T>>() {
            Some(f) => {
                self.interest = Some(f.clone());
                true
            }
            None => false,
        }
    }
}

/// Stable 16-bit wire identity for a named channel (pool, event, or the
/// input channel), derived from its name (FNV-1a, folded to 16 bits).
/// Everything is addressed on the wire by this key, never by registration
/// order — so server and client may register in any order. Collisions are
/// caught at registration by the one [`WireReg`] guard.
pub(crate) fn pool_key(name: &str) -> u16 {
    let mut h: u32 = 0x811c_9dc5;
    for &b in name.as_bytes() {
        h ^= b as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    (h ^ (h >> 16)) as u16
}

/// Stable wire tag for a named event channel, in the user-tag space
/// (`>= EVENT_USER_BASE`) so it can't collide with internal frame types.
/// Same name → same tag on both ends; derived from [`pool_key`].
pub(crate) fn event_tag(name: &str) -> u16 {
    let span = u16::MAX - EVENT_USER_BASE;
    EVENT_USER_BASE + pool_key(name) % span
}

/// What a wire-registry entry is — three API views (sync vs send vs set)
/// over one table.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum WireKind {
    Pool,
    Event,
    Input,
}

impl WireKind {
    pub(crate) fn byte(self) -> u8 {
        match self {
            WireKind::Pool => b'p',
            WireKind::Event => b'e',
            WireKind::Input => b'i',
        }
    }
}

/// THE wire registry (the `"net.reg"` single): every named, typed,
/// name-hashed channel — synced pools, event channels, the input channel —
/// in one table with one hash keyspace and one collision panic. The QUIC
/// handshake schema is this table, so a name/size/kind disagreement
/// between server and client fails the connection instead of
/// mis-delivering.
///
/// Strict schema EQUALITY is deliberate (decided over a subset rule):
/// local pools/singles never register here, so server-only state
/// (metrics) and client-only state (draw pools) are already free —
/// everything that IS here crosses the wire, must agree on both ends to
/// parse at all, and a client-side extra is always a bug. Equality turns
/// every drift into a loud connect error. Revisit only if spectator
/// clients (no input channel) or version-skewed fleets become real.
#[derive(Default)]
pub(crate) struct WireReg {
    entries: Vec<(WireKind, String, usize)>,
}

impl WireReg {
    pub(crate) fn register(&mut self, kind: WireKind, name: &str, size: usize) {
        if kind == WireKind::Input
            && let Some((_, prev, _)) = self.entries.iter().find(|(k, ..)| *k == WireKind::Input)
        {
            panic!(
                "one continuous input channel per connection: '{prev}' is already \
                 registered — clone its InputTx/InputRx instead of registering '{name}' \
                 (a pod with more fields beats a second channel)"
            );
        }
        let key = pool_key(name);
        if let Some((pk, pn, ps)) = self.entries.iter().find(|(_, n, _)| pool_key(n) == key) {
            // Re-registering the same event channel (setup helper + task
            // both grab it) shares the tag; anything else is a real clash.
            if *pk == kind && kind == WireKind::Event && pn == name && *ps == size {
                return;
            }
            if pn == name {
                panic!("wire channel '{name}' registered twice with a different kind or size");
            }
            panic!(
                "wire name-hash collision: '{name}' and '{pn}' both key to {key:#06x} — rename one"
            );
        }
        // The event wire tag folds the key into the user-tag span; two
        // distinct keys can alias there only across the span boundary, so
        // guard that edge too — same panic, same place.
        if kind == WireKind::Event {
            let tag = event_tag(name);
            if let Some((_, pn, _)) = self
                .entries
                .iter()
                .find(|(k, n, _)| *k == WireKind::Event && event_tag(n) == tag)
            {
                panic!(
                    "event name-hash collision: '{name}' and '{pn}' both tag to {tag:#06x} — rename one"
                );
            }
        }
        self.entries.push((kind, name.to_string(), size));
    }

    /// (kind, name, size) of everything registered, for the QUIC
    /// handshake. Order-independent: the transport sorts by name.
    pub(crate) fn schema(&self) -> Vec<(u8, String, usize)> {
        self.entries
            .iter()
            .map(|(k, n, s)| (k.byte(), n.clone(), *s))
            .collect()
    }
}

#[derive(Default)]
pub(crate) struct SyncSet {
    adapters: Vec<Box<dyn SyncAdapter>>,
}

impl SyncSet {
    pub(crate) fn pool_wire<T: Wire>(&mut self, name: &str, pool: &PoolHandle<T>) {
        self.adapters.push(Box::new(WireAdapter {
            name: name.to_string(),
            pool: pool.rc().clone(),
            interest: None,
        }));
    }

    /// Attach an interest scorer to the already-registered pool `name`.
    pub(crate) fn interest<T: 'static>(&mut self, name: &str, f: InterestFn<T>) {
        let ad = self
            .adapters
            .iter_mut()
            .find(|a| a.name() == name)
            .unwrap_or_else(|| panic!("interest: pool '{name}' is not synced"));
        assert!(
            ad.interest_set(&f),
            "interest: pool '{name}' does not hold {}",
            std::any::type_name::<T>()
        );
    }

    pub(crate) fn pool_sync<T: Pod + 'static>(&mut self, name: &str, pool: &PoolHandle<T>) {
        self.adapters.push(Box::new(PoolAdapter {
            name: name.to_string(),
            pool: pool.rc().clone(),
            interest: None,
        }));
    }

    /// The adapter for a wire key (`pool_key`), looked up by name hash so
    /// registration order is irrelevant.
    fn adapter_by_key(&self, key: u16) -> Option<&dyn SyncAdapter> {
        self.adapters
            .iter()
            .find(|a| pool_key(a.name()) == key)
            .map(|b| b.as_ref())
    }

    /// (kind, pool name, value size) per registered pool — the pool rows
    /// of the handshake schema. The full schema (events + input channel
    /// included) lives in [`WireReg`]; this exists for the sync layer's
    /// own tests.
    #[cfg(test)]
    pub(crate) fn schema(&self) -> Vec<(u8, String, usize)> {
        self.adapters
            .iter()
            .map(|a| (WireKind::Pool.byte(), a.name().to_string(), a.value_size()))
            .collect()
    }
}

// --- snapshot wire format -------------------------------------------------
//
//   u32 tick label (last completed tick)
//   u32 send seq (per-peer, one per snapshot SENT — a multi-datagram
//     flight puts several seqs inside one tick label; acks echo both)
//   u32 input seq echo (last input this peer's sim consumed)
//   u8 owner count, then count x [peer u8][entity id u32] — the full
//     peer→controlled-entity table, same bytes for every peer; riding
//     every header makes it loss-robust with no reliability machinery
//   u32 removal count, then count x [id u32]
//   u16 section count, then per section:
//     u16 pool key (name hash; see `pool_key`, order-independent)
//     u32 entry count, then count x [id u32][value bytes]

/// In-flight snapshots older than this many ticks past the newest label
/// are declared lost even without a later ack (covers a silent ack gap:
/// the entries become resendable again).
const INFLIGHT_EXPIRY_TICKS: u32 = 60;

struct Peer {
    acked_tick: u32,
    /// Next send sequence (starts at 1; 0 means "no send"). Monotonic
    /// per peer — the ack currency now that a tick label can cover a
    /// whole flight of sends. Same wrap horizon as ticks (~2.2 years).
    next_seq: u32,
    input_seq: u32,
    pools: Vec<PeerPool>,
    /// What each unacked send carried — (send seq, tick label, entries),
    /// oldest first — so an ack can confirm exactly that send's entries
    /// and declare everything older lost (a later send arrived; earlier
    /// ones didn't).
    sent: VecDeque<(u32, u32, Vec<SentEntry>)>,
}

impl Peer {
    /// Settle a send: confirm its entries if the peer `received` it, and
    /// either way clear their in-flight markers (unless a newer send
    /// superseded them) so lost entries re-qualify for packing.
    fn settle(&mut self, seq: u32, entries: Vec<SentEntry>, received: bool) {
        for (pool_idx, slot, sent_tick) in entries {
            let pp = &mut self.pools[pool_idx as usize];
            if received {
                let c = pp.confirmed.get(slot);
                pp.confirmed.set(slot, c.max(sent_tick));
            }
            if pp.inflight_seq.get(slot) == seq {
                pp.inflight_tick.set(slot, 0);
                pp.inflight_seq.set(slot, 0);
            }
        }
    }
}

/// What a successfully applied snapshot tells the client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Applied {
    /// Snapshot tick label — half of the ack to send back.
    pub tick: u32,
    /// The snapshot's send sequence — the other half of the ack. Also
    /// the apply-order key: a flight's datagrams may arrive reordered,
    /// and seq order is send order.
    pub seq: u32,
    /// Last input sequence the server processed for this peer — the
    /// reconciliation point for client-side prediction.
    pub input_seq: u32,
    /// The full peer→controlled-entity table, sorted by peer. The net
    /// module mirrors it into the client's net status; games read it via
    /// [`ClientNet::mine`](crate::ClientNet::mine) /
    /// [`owner_of`](crate::ClientNet::owner_of).
    pub owners: Vec<(u8, Id)>,
}

/// One packed snapshot datagram plus the flight-control readouts the
/// send loop steers by (see `snapshot_budgeted`).
pub struct Snapshot {
    /// The datagram payload.
    pub bytes: Vec<u8>,
    /// Entries packed across all pools. Zero with `more` set means the
    /// budget couldn't fit even one entry — stop extending the flight.
    pub entries: u32,
    /// Dirty entries remained beyond the budget — the "another datagram
    /// this tick would carry real freshness" signal.
    pub more: bool,
}

/// Server side: owns the peer table, packs per-peer deltas, gates id
/// recycling on acks. Create during init and move into the net task.
pub struct NetServer {
    sync: SyncSet,
    peers: BTreeMap<u8, Peer>,
    /// The peer→controlled-entity table, shipped whole in every snapshot
    /// header (same bytes for every peer) — see `owners_set`.
    owners: Vec<(u8, u32)>,
}

impl NetServer {
    /// Attaching a server holds the kernel's removal log so removed
    /// indices aren't recycled before every peer has acked the removal.
    #[cfg(test)] // test seam: manual bind path
    pub fn new(pm: &mut Pm) -> Self {
        pm.removal_hold_set(true);
        Self {
            sync: SyncSet::default(),
            peers: BTreeMap::new(),
            owners: Vec::new(),
        }
    }

    /// Build a server around an already-populated sync set (what
    /// `Pm::serve` hands over from the `"net.sync"` registration).
    pub(crate) fn with_sync(sync: SyncSet) -> Self {
        Self {
            sync,
            peers: BTreeMap::new(),
            owners: Vec::new(),
        }
    }

    #[cfg(test)] // test seam: manual bind path
    pub fn pool_sync<T: Pod + 'static>(&mut self, name: &str, pool: &PoolHandle<T>) {
        self.sync.pool_sync(name, pool);
    }

    #[cfg(test)] // test seam: manual bind path
    pub fn pool_wire<T: Wire>(&mut self, name: &str, pool: &PoolHandle<T>) {
        self.sync.pool_wire(name, pool);
    }

    #[cfg(test)] // test seam: manual bind path
    pub fn schema(&self) -> Vec<(u8, String, usize)> {
        self.sync.schema()
    }

    pub fn peer_add(&mut self, peer: u8) {
        self.peers.entry(peer).or_insert_with(|| Peer {
            acked_tick: 0,
            next_seq: 1,
            input_seq: 0,
            pools: (0..self.sync.adapters.len())
                .map(|_| PeerPool::new())
                .collect(),
            sent: VecDeque::new(),
        });
    }

    pub fn peer_remove(&mut self, peer: u8) {
        self.peers.remove(&peer);
    }

    pub fn peers(&self) -> impl Iterator<Item = u8> + '_ {
        self.peers.keys().copied()
    }

    /// Record a peer's ack of the send `(tick, seq)`: that send's entries
    /// are confirmed for the peer, and every older unacked send is
    /// declared lost (its entries become resendable). `tick` advances the
    /// acked cursor even when `seq` matches nothing (entry-less
    /// keepalives never enter the sent queue but their acks still drive
    /// the removal gate and the lag-comp anchor). Out-of-order acks are
    /// harmless — a late ack for an already-settled seq is ignored,
    /// costing at most a redundant resend (snapshots are idempotent
    /// upserts).
    pub fn ack(&mut self, peer: u8, tick: u32, seq: u32) {
        let Some(p) = self.peers.get_mut(&peer) else {
            return;
        };
        p.acked_tick = p.acked_tick.max(tick);
        while let Some(&(s, _, _)) = p.sent.front() {
            if s > seq {
                break;
            }
            let (s, _, entries) = p.sent.pop_front().unwrap();
            p.settle(s, entries, s == seq);
        }
    }

    /// The newest snapshot tick `peer` has acknowledged (0 for an unknown
    /// peer or before the first ack). Because acks and inputs share the
    /// client→server path, this is also ≈ the newest snapshot the client
    /// *had* when it sent the inputs now arriving — the anchor for
    /// lag-compensation rewind (see `PmServer::history_pool`).
    pub fn acked_tick(&self, peer: u8) -> u32 {
        self.peers.get(&peer).map_or(0, |p| p.acked_tick)
    }

    /// Record the newest input sequence consumed for `peer`; echoed in
    /// every snapshot header so the client can reconcile predictions.
    pub fn input_processed(&mut self, peer: u8, seq: u32) {
        if let Some(p) = self.peers.get_mut(&peer) {
            p.input_seq = p.input_seq.max(seq);
        }
    }

    /// Replace the peer→controlled-entity table shipped in every snapshot
    /// header. One table for all peers: every client sees who controls
    /// what — its own entry ([`ClientNet::mine`](crate::ClientNet::mine))
    /// and everyone else's (no hand-carried peer fields in pods).
    /// Idempotent; call it every tick. Sorted here so the wire bytes are
    /// deterministic regardless of the caller's map order.
    pub fn owners_set(&mut self, mut owners: Vec<(u8, u32)>) {
        owners.sort_unstable();
        self.owners = owners;
    }

    /// Pack everything `peer` hasn't confirmed, without a size cap.
    /// Prefer `snapshot_budgeted` when the transport bounds datagrams.
    #[cfg(test)] // test seam: manual bind path
    pub fn snapshot(&mut self, pm: &Pm, peer: u8) -> Option<Vec<u8>> {
        self.snapshot_budgeted(pm, peer, usize::MAX).map(|s| s.bytes)
    }

    /// Pack at most `budget` bytes of unconfirmed state for `peer`,
    /// oldest-rotation first. None if the peer is unknown.
    ///
    /// What doesn't fit stays unconfirmed and rotates into later
    /// snapshots, so a change-dense pool larger than the budget streams
    /// through it round-robin while change-sparse pools still converge
    /// to silence — one mechanism, both behaviors. Pools pack
    /// smallest-dirty-first, so the budget squeezes the change-dense
    /// pools (whose rotation absorbs it as a lower per-entity rate), never
    /// the sparse ones: a 200-entry horde that outweighs the whole budget
    /// costs the horde freshness, not the scoreboard single registered
    /// after it. Removals are always included (small, and they gate id
    /// recycling).
    ///
    /// Each call is one SEND (it consumes a send sequence): call again in
    /// the same tick while [`Snapshot::more`] says entries didn't fit and
    /// the wire can take another datagram — that loop is the multi-
    /// datagram flight, and packed entries going in-flight is what makes
    /// the next call resume instead of repeating.
    pub fn snapshot_budgeted(&mut self, pm: &Pm, peer: u8, budget: usize) -> Option<Snapshot> {
        let state = self.peers.get_mut(&peer)?;
        // Lazily grow per-pool state for pools registered after peer_add.
        while state.pools.len() < self.sync.adapters.len() {
            state.pools.push(PeerPool::new());
        }
        let acked = state.acked_tick;
        let label = pm.tick().saturating_sub(1);
        let seq = state.next_seq;
        state.next_seq += 1;

        // A long silent ack gap (every ack lost, or none sent): declare
        // stale in-flight snapshots lost so their entries resend.
        while let Some(&(_, l, _)) = state.sent.front() {
            if l.saturating_add(INFLIGHT_EXPIRY_TICKS) >= label {
                break;
            }
            let (s, _, entries) = state.sent.pop_front().unwrap();
            state.settle(s, entries, false);
        }

        let mut out = Vec::new();
        out.extend_from_slice(&label.to_le_bytes());
        out.extend_from_slice(&seq.to_le_bytes());
        out.extend_from_slice(&state.input_seq.to_le_bytes());
        out.push(self.owners.len() as u8);
        for &(peer, id) in &self.owners {
            out.push(peer);
            out.extend_from_slice(&id.to_le_bytes());
        }

        let removals: Vec<u32> = pm
            .removal_log()
            .iter()
            .filter(|&&(_, t)| t > acked)
            .map(|&(id, _)| id.0)
            .collect();
        out.extend_from_slice(&(removals.len() as u32).to_le_bytes());
        for id in removals {
            out.extend_from_slice(&id.to_le_bytes());
        }

        out.extend_from_slice(&(self.sync.adapters.len() as u16).to_le_bytes());
        let mut sent = Vec::new();
        // 6 bytes of section header (index + count) per pool.
        let mut remaining = budget.saturating_sub(out.len() + 6 * self.sync.adapters.len());
        let budget_at_entries = remaining;
        // Pack smallest-dirty-first (ties by registration index, so the
        // order is deterministic): sparse pools — scoreboard singles,
        // events-as-entries, the odd moved prop — always fit, and a
        // change-dense pool takes only what they left, instead of a pool
        // registered early eating the whole budget and starving everything
        // after it forever. Sections are addressed by name hash, so the
        // client applies them in any order.
        let mut order: Vec<(usize, usize)> = self
            .sync
            .adapters
            .iter()
            .enumerate()
            .map(|(i, a)| (a.dirty_bytes(&state.pools[i]), i))
            .collect();
        order.sort_unstable();
        // `dirty_bytes` counts by the exact predicate `pack_dirty` sends
        // by, so bytes-packed < bytes-dirty ⇔ something didn't fit.
        let dirty_total: usize = order.iter().map(|&(b, _)| b).sum();
        let mut entries = 0u32;
        for (_, i) in order {
            let adapter = &self.sync.adapters[i];
            // Wire identity is the name hash, not the table index `i` — `i`
            // stays a local cursor into this peer's `pools`/`sent` tables.
            out.extend_from_slice(&pool_key(adapter.name()).to_le_bytes());
            let count_at = out.len();
            out.extend_from_slice(&0u32.to_le_bytes());
            let count = adapter.pack_dirty(
                &mut state.pools[i],
                peer,
                label,
                seq,
                &mut remaining,
                &mut out,
                &mut sent,
                i as u16,
            );
            out[count_at..count_at + 4].copy_from_slice(&count.to_le_bytes());
            entries += count;
        }
        let more = budget_at_entries - remaining < dirty_total;
        if !sent.is_empty() {
            state.sent.push_back((seq, label, sent));
        }
        Some(Snapshot {
            bytes: out,
            entries,
            more,
        })
    }

    /// Attach an interest scorer to a synced pool — the seam behind
    /// [`PmServer::interest_pool`](crate::PmServer::interest_pool)
    /// (which is the documented front door; this exists for direct
    /// `NetServer` embedders and tests).
    pub fn interest<T: 'static>(&mut self, name: &str, f: Rc<dyn Fn(u8, Id, &T) -> f32>) {
        self.sync.interest(name, f);
    }

    /// Recycle removal-log entries every peer has acked. Call once per net
    /// tick, after processing acks.
    pub fn prune(&self, pm: &mut Pm) {
        let min = self
            .peers
            .values()
            .map(|p| p.acked_tick)
            .min()
            .unwrap_or(pm.tick());
        pm.removal_release_upto(min);
    }
}

/// Client side: applies snapshots into registered pools. Removals go
/// through the normal deferred path; ids foreign to this peer are never
/// recycled locally (`Pm::local_peer`).
#[derive(Default)]
pub struct NetClient {
    sync: SyncSet,
}

impl NetClient {
    #[cfg(test)] // test seam: manual bind path
    pub fn new() -> Self {
        Self::default()
    }

    /// Build a client around an already-populated sync set (what
    /// `Pm::connect` hands over from the `"net.sync"` registration).
    pub(crate) fn with_sync(sync: SyncSet) -> Self {
        Self { sync }
    }

    #[cfg(test)] // test seam: manual bind path
    pub fn pool_sync<T: Pod + 'static>(&mut self, name: &str, pool: &PoolHandle<T>) {
        self.sync.pool_sync(name, pool);
    }

    #[cfg(test)] // test seam: manual bind path
    pub fn pool_wire<T: Wire>(&mut self, name: &str, pool: &PoolHandle<T>) {
        self.sync.pool_wire(name, pool);
    }

    #[cfg(test)] // test seam: manual bind path
    pub fn schema(&self) -> Vec<(u8, String, usize)> {
        self.sync.schema()
    }

    /// Apply a snapshot; returns its (tick, seq) labels (the ack to send
    /// back) and the server's input-sequence echo.
    pub fn apply(&self, pm: &mut Pm, snapshot: &[u8]) -> Result<Applied, NetError> {
        let mut r = Reader::new(snapshot);
        let tick = r.u32()?;
        let seq = r.u32()?;
        let input_seq = r.u32()?;
        let owner_count = r.u8()?;
        let mut owners = Vec::with_capacity(owner_count as usize);
        for _ in 0..owner_count {
            let peer = r.u8()?;
            let id = Id(r.u32()?);
            owners.push((peer, id));
        }

        let removal_count = r.u32()?;
        for _ in 0..removal_count {
            let id = Id(r.u32()?);
            if pm.id_alive(id) {
                pm.id_remove(id); // deferred, flushed at end of this tick
            }
        }

        let section_count = r.u16()?;
        for _ in 0..section_count {
            let key = r.u16()?;
            let count = r.u32()?;
            let adapter = self
                .sync
                .adapter_by_key(key)
                .ok_or(NetError::UnknownPool(key))?;
            adapter.apply(pm, count, &mut r)?;
        }
        Ok(Applied {
            tick,
            seq,
            input_seq,
            owners,
        })
    }
}
