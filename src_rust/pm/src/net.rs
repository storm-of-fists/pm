//! Headless sync layer: server-authoritative snapshot-delta replication.
//! Transport-agnostic — `NetServer` produces byte buffers and `NetClient`
//! consumes them; QUIC (quinn-proto) will carry them later.
//!
//! Model (SYNC_DESIGN.md): per peer the server keeps one cursor,
//! `acked_tick`. A snapshot for a peer carries every synced-pool entry
//! changed since that cursor plus every logged removal since it. Acks
//! advance the cursor; a lost snapshot just means the next one re-carries
//! the same still-unacked state. Everything is an upsert, so snapshots are
//! idempotent.
//!
//! Tick semantics: a snapshot is labeled `pm.tick() - 1` — the last
//! *completed* tick — because stamps from the in-progress tick may still
//! be written by tasks that haven't run yet. Entries from the current tick
//! may ride along early; the conservative label only means they get sent
//! again next time, never lost. Run the net-send task at low priority
//! (first in the tick) to avoid the duplicate sends entirely.

use std::cell::RefCell;
use std::rc::Rc;

use bytemuck::Pod;

use crate::id::Id;
use crate::kernel::Pm;
use crate::pool::Pool;

#[derive(Debug, PartialEq, Eq)]
pub enum NetError {
    Truncated,
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

    fn u16(&mut self) -> Result<u16, NetError> {
        Ok(u16::from_le_bytes(self.bytes(2)?.try_into().unwrap()))
    }

    fn u32(&mut self) -> Result<u32, NetError> {
        Ok(u32::from_le_bytes(self.bytes(4)?.try_into().unwrap()))
    }
}

// --- type-erased pool sync adapters ------------------------------------

trait SyncAdapter {
    fn name(&self) -> &str;
    fn value_size(&self) -> usize;
    /// Append `[id u32][value]` entries changed after `tick`; returns count.
    fn pack_since(&self, tick: u32, out: &mut Vec<u8>) -> u32;
    fn apply(&self, pm: &mut Pm, count: u32, r: &mut Reader) -> Result<(), NetError>;
}

struct PoolAdapter<T: Pod> {
    name: String,
    pool: Rc<RefCell<Pool<T>>>,
}

impl<T: Pod> SyncAdapter for PoolAdapter<T> {
    fn name(&self) -> &str {
        &self.name
    }

    fn value_size(&self) -> usize {
        size_of::<T>()
    }

    fn pack_since(&self, tick: u32, out: &mut Vec<u8>) -> u32 {
        let pool = self.pool.borrow();
        let mut count = 0u32;
        for (id, value) in pool.changed_since(tick) {
            out.extend_from_slice(&id.0.to_le_bytes());
            out.extend_from_slice(bytemuck::bytes_of(value));
            count += 1;
        }
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
}

#[derive(Default)]
struct SyncSet {
    adapters: Vec<Box<dyn SyncAdapter>>,
}

impl SyncSet {
    fn pool_sync<T: Pod + 'static>(&mut self, name: &str, pool: &Rc<RefCell<Pool<T>>>) {
        self.adapters.push(Box::new(PoolAdapter { name: name.to_string(), pool: pool.clone() }));
    }

    /// (pool name, value size) per registered pool, in registration order.
    /// Server and client schemas must match; the QUIC handshake will
    /// verify this — until then, tests assert it.
    fn schema(&self) -> Vec<(String, usize)> {
        self.adapters.iter().map(|a| (a.name().to_string(), a.value_size())).collect()
    }
}

// --- snapshot wire format -------------------------------------------------
//
//   u32 tick label (last completed tick)
//   u32 removal count, then count x [id u32]
//   u16 section count, then per section:
//     u16 pool index (registration order)
//     u32 entry count, then count x [id u32][value bytes]

struct Peer {
    peer: u8,
    acked_tick: u32,
    input_seq: u32,
}

/// What a successfully applied snapshot tells the client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Applied {
    /// Snapshot tick label — the ack to send back.
    pub tick: u32,
    /// Last input sequence the server processed for this peer — the
    /// reconciliation point for client-side prediction.
    pub input_seq: u32,
}

/// Server side: owns the peer table, packs per-peer deltas, gates id
/// recycling on acks. Create during init and move into the net task.
pub struct NetServer {
    sync: SyncSet,
    peers: Vec<Peer>,
}

impl NetServer {
    /// Attaching a server holds the kernel's removal log so removed
    /// indices aren't recycled before every peer has acked the removal.
    pub fn new(pm: &mut Pm) -> Self {
        pm.removal_hold_set(true);
        Self { sync: SyncSet::default(), peers: Vec::new() }
    }

    pub fn pool_sync<T: Pod + 'static>(&mut self, name: &str, pool: &Rc<RefCell<Pool<T>>>) {
        self.sync.pool_sync(name, pool);
    }

    pub fn schema(&self) -> Vec<(String, usize)> {
        self.sync.schema()
    }

    pub fn peer_add(&mut self, peer: u8) {
        if !self.peers.iter().any(|p| p.peer == peer) {
            self.peers.push(Peer { peer, acked_tick: 0, input_seq: 0 });
        }
    }

    pub fn peer_remove(&mut self, peer: u8) {
        self.peers.retain(|p| p.peer != peer);
    }

    pub fn peers(&self) -> impl Iterator<Item = u8> + '_ {
        self.peers.iter().map(|p| p.peer)
    }

    /// Record a peer's ack. Out-of-order acks are harmless (cursor only
    /// moves forward).
    pub fn ack(&mut self, peer: u8, tick: u32) {
        if let Some(p) = self.peers.iter_mut().find(|p| p.peer == peer) {
            p.acked_tick = p.acked_tick.max(tick);
        }
    }

    /// Record the newest input sequence consumed for `peer`; echoed in
    /// every snapshot header so the client can reconcile predictions.
    pub fn input_processed(&mut self, peer: u8, seq: u32) {
        if let Some(p) = self.peers.iter_mut().find(|p| p.peer == peer) {
            p.input_seq = p.input_seq.max(seq);
        }
    }

    /// Pack everything `peer` hasn't acked. None if the peer is unknown.
    pub fn snapshot(&self, pm: &Pm, peer: u8) -> Option<Vec<u8>> {
        let state = self.peers.iter().find(|p| p.peer == peer)?;
        let acked = state.acked_tick;
        let label = pm.tick().saturating_sub(1);
        let mut out = Vec::new();
        out.extend_from_slice(&label.to_le_bytes());
        out.extend_from_slice(&state.input_seq.to_le_bytes());

        let removals: Vec<u32> =
            pm.removal_log().iter().filter(|&&(_, t)| t > acked).map(|&(id, _)| id.0).collect();
        out.extend_from_slice(&(removals.len() as u32).to_le_bytes());
        for id in removals {
            out.extend_from_slice(&id.to_le_bytes());
        }

        out.extend_from_slice(&(self.sync.adapters.len() as u16).to_le_bytes());
        for (i, adapter) in self.sync.adapters.iter().enumerate() {
            out.extend_from_slice(&(i as u16).to_le_bytes());
            let count_at = out.len();
            out.extend_from_slice(&0u32.to_le_bytes());
            let count = adapter.pack_since(acked, &mut out);
            out[count_at..count_at + 4].copy_from_slice(&count.to_le_bytes());
        }
        Some(out)
    }

    /// Recycle removal-log entries every peer has acked. Call once per net
    /// tick, after processing acks.
    pub fn prune(&self, pm: &mut Pm) {
        let min = self.peers.iter().map(|p| p.acked_tick).min().unwrap_or(pm.tick());
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
    pub fn new() -> Self {
        Self::default()
    }

    pub fn pool_sync<T: Pod + 'static>(&mut self, name: &str, pool: &Rc<RefCell<Pool<T>>>) {
        self.sync.pool_sync(name, pool);
    }

    pub fn schema(&self) -> Vec<(String, usize)> {
        self.sync.schema()
    }

    /// Apply a snapshot; returns its tick label (the ack to send back)
    /// and the server's input-sequence echo.
    pub fn apply(&self, pm: &mut Pm, snapshot: &[u8]) -> Result<Applied, NetError> {
        let mut r = Reader::new(snapshot);
        let tick = r.u32()?;
        let input_seq = r.u32()?;

        let removal_count = r.u32()?;
        for _ in 0..removal_count {
            let id = Id(r.u32()?);
            if pm.id_alive(id) {
                pm.id_remove(id); // deferred, flushed at end of this tick
            }
        }

        let section_count = r.u16()?;
        for _ in 0..section_count {
            let index = r.u16()?;
            let count = r.u32()?;
            let adapter =
                self.sync.adapters.get(index as usize).ok_or(NetError::UnknownPool(index))?;
            adapter.apply(pm, count, &mut r)?;
        }
        Ok(Applied { tick, input_seq })
    }
}
