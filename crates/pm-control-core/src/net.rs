//! NetworkManager — broadcast pub/sub signal exchange over datagrams.
//!
//! Every message is broadcast to the segment; content decides who cares.
//! Publishers continuously advertise their schema (discovery); consumers
//! send explicit subscribe leases for the packets they want; data flows
//! only while somebody holds a lease. A [`Monitor`](crate::Monitor) on the
//! segment can therefore discover and watch every published signal with no
//! declaring code at all.
//!
//! ```text
//! Data         [0][epoch u32][pkt u16][payload…]
//! Schema       [2][epoch u32][len u8][node…] ([pkt u16][off u16][ty u8][nlen u8][mlen u8][name…][meta…])*
//! Subscribe    [3][epoch u32] ([pkt u16])*
//! Unlock       [4][epoch u32][len u8][name…][raw value bytes]
//! Lock         [5][epoch u32][len u8][name…]
//! LockAll      [6][epoch u32]
//! Unsubscribe  [7][epoch u32] ([pkt u16])*
//! ```
//!
//! Schema entries carry no size — a value's size follows from its type tag
//! ([`WireType::byte_size`]). `meta` is the entry's metadata block,
//! `[flags u8]` then per flag: typed `lo`/`hi` bounds
//! ([`SCHEMA_META_BOUNDS`](crate::signal::SCHEMA_META_BOUNDS), only when
//! configured) and the text-list name
//! ([`SCHEMA_META_MAP`](crate::signal::SCHEMA_META_MAP), only when set).
//! `mlen` keeps entries skippable even when the type tag is newer than the
//! reader. Metadata rides the always-on beacon rather than a request/
//! response round trip — only configured metadata pays wire bytes.
//!
//! The `epoch` is an FNV-1a hash of the publisher's node name and packet
//! layout — both the publisher's address on the segment and its layout
//! version (0 is reserved for "unknown"). Schema is the authority on
//! epochs: a changed epoch in a schema message invalidates and rebinds;
//! data with an unknown epoch is simply ignored.
//!
//! Subscription is a one-shot latch: the publisher remembers it and keeps
//! streaming the packet every scan. Consumers only send Subscribe while
//! expected data is *not* arriving (initial bind, a lost datagram, a
//! restarted publisher — data stalled ≥ [`LEASE_MS`] triggers a re-request
//! on the next tick), so a fully resolved network carries data plus the
//! periodic schema beacon and nothing else. The one continuously renewed
//! request is an unlock override: it must be re-sent every [`TICK_MS`] or
//! the publisher fail-safe relocks after [`LEASE_MS`] — an override held
//! by a vanished node must not stay in charge.
//!
//! Unsubscribe drops the latch — how a briefly-attached tool lets go
//! instead of leaving the publisher streaming until reboot. It isn't
//! refcounted: if another consumer still wanted the packet, its stall
//! detector re-subscribes within [`LEASE_MS`] + a tick, the same self-heal
//! that covers a publisher restart.
//!
//! Deliberate deviation, flagged for review: this protocol is NOT
//! wire-compatible with the ST side anymore (broadcast addressing,
//! subscribe leases, typed schema entries). The ST one-peer push protocol
//! is gone.
//!
//! Everything OS-shaped stays out: the manager never opens a socket. The
//! host hands in a [`SegmentPort`] each cycle whose `send` broadcasts to the
//! segment — std hosts wrap a broadcast/multicast `UdpSocket`, micros wrap
//! their network stack.
//!
//! Lifecycle: `add(&group)` (or `add_signal`/`add_registered`) →
//! `bind(local_name, remotes)` → each scan `begin_cycle(port)` … app logic …
//! `end_cycle(port)`.

use alloc::collections::BTreeMap;
use alloc::rc::Rc;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use crate::clock;
use crate::pm_group;
use crate::prof::PmProf;
use crate::signal::{AnySignal, PmBool, PmI32, RCursor, Register, Registrar, UNBOUND, WCursor, WireType};

/// First byte of every datagram. Same shape as [`WireType`]: the
/// discriminant is the wire tag, `from_u8` is the decode gate (unknown
/// tags — newer peers — drop silently).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Message {
    Data = 0,
    Schema = 2,
    Subscribe = 3,
    Unlock = 4,
    Lock = 5,
    LockAll = 6,
    Unsubscribe = 7,
}

impl Message {
    pub fn from_u8(v: u8) -> Option<Message> {
        Some(match v {
            0 => Message::Data,
            2 => Message::Schema,
            3 => Message::Subscribe,
            4 => Message::Unlock,
            5 => Message::Lock,
            6 => Message::LockAll,
            7 => Message::Unsubscribe,
            _ => return None,
        })
    }
}

pub const MAX_PACKETS: usize = 64;
pub const PACKET_CAP: usize = 1024;
pub const DATA_HEADER: usize = 7;
pub const MTU: usize = 1500;
/// Cadence of periodic traffic: schema adverts, unlock renewals, and
/// subscribe re-requests while data is stalled.
pub const TICK_MS: u64 = 250;
/// Expiry horizon: an unrenewed unlock override relocks, a silent peer goes
/// offline, and stalled subscribed data triggers a re-subscribe.
pub const LEASE_MS: u64 = 1_000;

const SCHEMA_HEADER: usize = 6; // [2][epoch u32][node len u8] + node bytes
const ENTRY_HEADER: usize = 7; // [pkt u16][off u16][ty u8][name len u8][meta len u8]
const MAX_SCHEMA_ADV_PER_TICK: usize = 100;

/// Broadcast datagram transport, implemented by the host: `send` must reach
/// every participant on the segment (broadcast/multicast socket).
pub trait SegmentPort {
    /// Pop the next pending datagram into `buf`; `None` when drained.
    fn recv(&mut self, buf: &mut [u8]) -> Option<usize>;
    /// Broadcast one datagram to the segment.
    fn send(&mut self, data: &[u8]);
}

// --------------------------------------------------------------- wire codec
//
// The message shapes, parsed and composed in exactly one place — the
// NetworkManager and the Monitor are two consumers of the same protocol,
// not two implementations of it.

/// The `epoch u32` every message carries at bytes 1..5.
pub(crate) fn epoch_of(buf: &[u8]) -> u32 {
    u32::from_le_bytes(buf[1..5].try_into().unwrap())
}

/// Parse `[tag][epoch][len u8][name…]`, returning the name and the offset
/// where trailing value bytes start (the Unlock/Lock shape).
fn parse_named(buf: &[u8]) -> Option<(&str, usize)> {
    let nm = *buf.get(5)? as usize;
    let name = core::str::from_utf8(buf.get(6..6 + nm)?).ok()?;
    Some((name, 6 + nm))
}

/// A parsed Schema message: header fields plus the raw entry stream.
pub(crate) struct SchemaMsg<'a> {
    pub epoch: u32,
    pub node: &'a str,
    entries: &'a [u8],
}

/// Parse a Schema datagram's header; `None` on truncation, a non-UTF-8
/// node name, or the reserved epoch 0.
pub(crate) fn parse_schema(buf: &[u8]) -> Option<SchemaMsg<'_>> {
    if buf.len() < SCHEMA_HEADER {
        return None;
    }
    let epoch = epoch_of(buf);
    if epoch == 0 {
        return None;
    }
    let nlen = buf[5] as usize;
    let node = core::str::from_utf8(buf.get(SCHEMA_HEADER..SCHEMA_HEADER + nlen)?).ok()?;
    Some(SchemaMsg { epoch, node, entries: &buf[SCHEMA_HEADER + nlen..] })
}

/// One schema entry: `[pkt u16][off u16][ty u8][nlen u8][mlen u8][name…][meta…]`.
pub(crate) struct SchemaEntry<'a> {
    pub pkt: u16,
    pub off: u16,
    /// `None`: a newer wire type than we know (skippable thanks to `mlen`).
    pub ty: Option<WireType>,
    pub name: &'a str,
    pub meta: &'a [u8],
}

impl<'a> SchemaMsg<'a> {
    /// Iterate the entries; stops at the first truncated one.
    pub fn entries(&self) -> SchemaEntries<'a> {
        SchemaEntries { buf: self.entries, off: 0 }
    }
}

pub(crate) struct SchemaEntries<'a> {
    buf: &'a [u8],
    off: usize,
}

impl<'a> Iterator for SchemaEntries<'a> {
    type Item = SchemaEntry<'a>;
    fn next(&mut self) -> Option<SchemaEntry<'a>> {
        let b = self.buf;
        if self.off + ENTRY_HEADER > b.len() {
            return None;
        }
        let pkt = u16::from_le_bytes(b[self.off..self.off + 2].try_into().unwrap());
        let off = u16::from_le_bytes(b[self.off + 2..self.off + 4].try_into().unwrap());
        let ty = WireType::from_u8(b[self.off + 4]);
        let nm_len = b[self.off + 5] as usize;
        let m_len = b[self.off + 6] as usize;
        let mut p = self.off + ENTRY_HEADER;
        if p + nm_len + m_len > b.len() {
            self.off = b.len(); // truncated entry: stop
            return None;
        }
        let name = core::str::from_utf8(&b[p..p + nm_len]).unwrap_or("");
        p += nm_len;
        let meta = &b[p..p + m_len];
        self.off = p + m_len;
        Some(SchemaEntry { pkt, off, ty, name, meta })
    }
}

/// Compose and send `[tag][epoch u32][len u8][name…][value…]` — the shape
/// Unlock and Lock share (Lock just carries no value bytes).
pub(crate) fn send_named(
    tx: &mut [u8],
    port: &mut dyn SegmentPort,
    msg: Message,
    epoch: u32,
    name: &str,
    value: &[u8],
) {
    let nm = name.as_bytes();
    if epoch == 0 || nm.is_empty() || nm.len() > 255 {
        return;
    }
    tx[0] = msg as u8;
    tx[1..5].copy_from_slice(&epoch.to_le_bytes());
    tx[5] = nm.len() as u8;
    tx[6..6 + nm.len()].copy_from_slice(nm);
    tx[6 + nm.len()..6 + nm.len() + value.len()].copy_from_slice(value);
    port.send(&tx[..6 + nm.len() + value.len()]);
}

/// Compose and send `[tag][epoch u32]([pkt u16])*` — the Subscribe/
/// Unsubscribe shape. Sends nothing when `pkts` is empty.
pub(crate) fn send_pkt_list(
    tx: &mut [u8],
    port: &mut dyn SegmentPort,
    msg: Message,
    epoch: u32,
    pkts: impl IntoIterator<Item = u16>,
) {
    let mut off = 5;
    for pkt in pkts {
        tx[off..off + 2].copy_from_slice(&pkt.to_le_bytes());
        off += 2;
    }
    if off == 5 {
        return;
    }
    tx[0] = msg as u8;
    tx[1..5].copy_from_slice(&epoch.to_le_bytes());
    port.send(&tx[..off]);
}

/// Consumer-side state for one remote publisher we subscribe to.
struct Peer {
    name: String,
    epoch: u32, // 0 = never heard / invalidated
    last_seen_ms: u64,
    online: bool,
    /// Subscriber indices (into `signals`) owned by this peer.
    signal_idxs: Vec<usize>,
    /// Resolved subscriber indices reachable from each remote packet id.
    subs_by_packet: Vec<Vec<usize>>,
    /// Last time data arrived per remote packet (0 = never); a subscribed
    /// packet going stale is what triggers a re-subscribe.
    last_data: Vec<u64>,
    unresolved: usize,
}

impl Peer {
    fn new(name: &str) -> Self {
        Peer {
            name: name.to_string(),
            epoch: 0,
            last_seen_ms: 0,
            online: false,
            signal_idxs: Vec::new(),
            subs_by_packet: alloc::vec![Vec::new(); MAX_PACKETS],
            last_data: alloc::vec![0; MAX_PACKETS],
            unresolved: 0,
        }
    }

    fn invalidate(&mut self, signals: &[Rc<dyn AnySignal>]) {
        self.epoch = 0;
        for &i in &self.signal_idxs {
            signals[i].meta().net_packet.set(UNBOUND);
            signals[i].meta().net_offset.set(0);
        }
        for chain in &mut self.subs_by_packet {
            chain.clear();
        }
        for t in &mut self.last_data {
            *t = 0;
        }
        self.unresolved = self.signal_idxs.len();
    }
}

pub struct NetworkManager {
    /// Block inbound Data application (recording playback owns the values).
    pub playback: bool,

    /// The manager's own health signals (`net.*`), when opted in.
    health: Option<NetHealth>,

    local_name: String,

    /// After `bind`: publishers first (`..publisher_count`), subscribers after.
    signals: Vec<Rc<dyn AnySignal>>,
    publisher_count: usize,
    by_name: BTreeMap<String, Vec<usize>>,
    peers: Vec<Peer>,

    staging_closed: bool,
    local_epoch: u32,
    schema_cursor: usize,
    unknown_node_count: usize,
    schema_mismatch_count: usize,
    config_error: bool,
    connected: bool,

    /// Publisher side: latched per-packet subscriptions (sticky until reboot).
    pkt_subscribed: Vec<bool>,
    /// Publisher side: unlock-override lease deadline per publisher index.
    unlock_lease: BTreeMap<usize, u64>,
    /// Consumer side: subscriber indices we hold unlocked on their owners.
    held: Vec<usize>,
    pending_locks: Vec<usize>,
    pending_lock_all: Vec<usize>,
    /// Consumer side: one-shot override pulses (the fault-clear path).
    /// Value bytes are captured at [`pulse`](Self::pulse) time — inbound
    /// data may lawfully overwrite the local copy before the flush.
    pending_pulses: Vec<(usize, Vec<u8>)>,

    next_tick_ms: u64,
    rx: [u8; MTU],
    tx: [u8; MTU],
}

impl Default for NetworkManager {
    fn default() -> Self {
        Self::new()
    }
}

impl NetworkManager {
    pub fn new() -> Self {
        NetworkManager {
            playback: false,
            health: None,
            local_name: String::new(),
            signals: Vec::new(),
            publisher_count: 0,
            by_name: BTreeMap::new(),
            peers: Vec::new(),
            staging_closed: false,
            local_epoch: 0,
            schema_cursor: 0,
            unknown_node_count: 0,
            schema_mismatch_count: 0,
            config_error: false,
            connected: false,
            pkt_subscribed: alloc::vec![false; MAX_PACKETS],
            unlock_lease: BTreeMap::new(),
            held: Vec::new(),
            pending_locks: Vec::new(),
            pending_lock_all: Vec::new(),
            pending_pulses: Vec::new(),
            next_tick_ms: 0,
            rx: [0; MTU],
            tx: [0; MTU],
        }
    }

    // ------------------------------------------------------------ staging

    pub fn add_signal(&mut self, sig: Rc<dyn AnySignal>) {
        if self.staging_closed {
            self.config_error = true;
            return;
        }
        self.signals.push(sig);
    }

    /// Stage everything under a `Register` root (a `pm_group!` struct, a
    /// single signal, …); collects the registrar pass internally.
    pub fn add(&mut self, root: &impl Register) {
        self.add_registered(&Registrar::collect(root));
    }

    /// Stage everything a `Registrar::collect` pass found.
    pub fn add_registered(&mut self, reg: &Registrar) {
        for s in &reg.signals {
            self.add_signal(s.clone());
        }
    }

    /// Publish the manager's own health under `net.*` (status snapshot +
    /// begin/end cycle timing) — call before [`bind`](Self::bind); the
    /// manager updates the signals every cycle from then on.
    pub fn publish_health(&mut self) {
        if self.staging_closed {
            self.config_error = true;
            return;
        }
        self.health.get_or_insert_with(NetHealth::new);
    }

    /// The health group, when [`publish_health`](Self::publish_health) is
    /// on — the app can read (or record) its own link state directly.
    pub fn health(&self) -> Option<&NetHealth> {
        self.health.as_ref()
    }

    /// Close staging and classify every staged signal:
    /// * `node == local_name` → publisher, packed into ≤`MAX_PACKETS`
    ///   packets of ≤`PACKET_CAP` bytes, in staging order;
    /// * `node` in `remotes` → subscriber of that peer (unresolved until
    ///   the peer's schema names it);
    /// * `node == ""` → local-only, dropped from networking;
    /// * anything else → dropped, counted in `unknown_node_count`.
    pub fn bind(&mut self, local_name: &str, remotes: &[&str]) {
        if self.staging_closed {
            return;
        }
        self.staging_closed = true;
        self.local_name = local_name.to_string();
        if local_name.is_empty() || local_name.len() > 255 {
            self.config_error = true;
        }
        self.peers = remotes.iter().map(|r| Peer::new(r)).collect();

        // The manager's own health rides the same pipe: stage it as local
        // publishers under the `net.` prefix, behind the app's signals.
        if let Some(h) = &self.health {
            for s in Registrar::collect(h).signals {
                let name = alloc::format!("net.{}", s.meta().name());
                *s.meta().name.borrow_mut() = name;
                *s.meta().node.borrow_mut() = self.local_name.clone();
                self.signals.push(s);
            }
        }

        let staged = core::mem::take(&mut self.signals);

        // Pass A: publishers keep staging order; the allocator packs greedily.
        let mut cur_pkt: Option<u16> = None;
        let mut cur_off: usize = 0;
        let mut packet_count: usize = 0;
        for s in &staged {
            let node = s.meta().node();
            if node.is_empty() || node != self.local_name {
                continue;
            }
            let sz = s.byte_size();
            let fits = sz > 0 && sz <= PACKET_CAP && s.meta().name().len() <= 255;
            let need_new = cur_pkt.is_none() || cur_off + sz > PACKET_CAP;
            if !fits || (need_new && packet_count >= MAX_PACKETS) {
                self.config_error = true; // dropped from networking
                continue;
            }
            if need_new {
                cur_pkt = Some(packet_count as u16);
                cur_off = 0;
                packet_count += 1;
            }
            s.meta().net_packet.set(cur_pkt.unwrap());
            s.meta().net_offset.set(cur_off as u32);
            cur_off += sz;
            self.signals.push(s.clone());
        }
        self.publisher_count = self.signals.len();

        // Pass B: subscribers behind publishers, assigned to their peer;
        // locals and unknowns drop.
        for s in staged {
            let node = s.meta().node();
            if node.is_empty() || node == self.local_name {
                continue;
            }
            let Some(peer) = self.peers.iter_mut().find(|p| p.name == node) else {
                self.unknown_node_count += 1;
                continue;
            };
            peer.signal_idxs.push(self.signals.len());
            peer.unresolved += 1;
            self.signals.push(s);
        }

        for (i, s) in self.signals.iter().enumerate() {
            self.by_name.entry(s.meta().name()).or_default().push(i);
        }
        self.local_epoch = self.calc_epoch();
    }

    // -------------------------------------------------------------- cycle

    /// Top of scan: drain inbound datagrams, expire peer liveness and
    /// override leases.
    pub fn begin_cycle(&mut self, port: &mut dyn SegmentPort) {
        if !self.staging_closed {
            return;
        }
        let t0_us = clock::now_us();
        while let Some(n) = port.recv(&mut self.rx) {
            if n > 0 {
                self.handle_datagram(n.min(MTU));
            }
        }
        let now = clock::now_ms();

        let (peers, signals) = (&mut self.peers, &self.signals);
        for peer in peers.iter_mut() {
            if peer.online && now.saturating_sub(peer.last_seen_ms) >= LEASE_MS {
                peer.online = false;
                peer.invalidate(signals);
            }
        }
        // Fail safe: a vanished override holder must not stay in charge.
        self.unlock_lease.retain(|&i, &mut deadline| {
            if now >= deadline {
                signals[i].meta().locked.set(true);
                false
            } else {
                true
            }
        });

        self.connected = !self.peers.is_empty()
            && self.peers.iter().all(|p| p.online)
            && self.peers.iter().all(|p| p.unresolved == 0);

        if let Some(h) = &self.health {
            h.begin.record_us(clock::now_us().saturating_sub(t0_us));
            h.set_status(&self.status());
        }
    }

    /// Bottom of scan: publish leased data every scan; on the tick, renew
    /// our subscriptions and held overrides and advertise the schema.
    pub fn end_cycle(&mut self, port: &mut dyn SegmentPort) {
        if !self.staging_closed {
            return;
        }
        let t0_us = clock::now_us();
        self.send_data(port);
        let now = clock::now_ms();
        if now >= self.next_tick_ms {
            self.next_tick_ms = now + TICK_MS;
            self.advertise_schema(port);
            self.send_subscribes(port);
            self.refresh_unlocks(port);
        }
        self.flush_pulses(port);
        self.flush_locks(port);
        if let Some(h) = &self.health {
            h.end.record_us(clock::now_us().saturating_sub(t0_us));
        }
    }

    // ----------------------------------------------------------- inbound

    fn handle_datagram(&mut self, n: usize) {
        match Message::from_u8(self.rx[0]) {
            Some(Message::Data) if n > DATA_HEADER => self.handle_data(n),
            Some(Message::Schema) if n >= SCHEMA_HEADER => self.handle_schema(n),
            Some(Message::Subscribe) if n >= 5 => self.handle_subscription(n, true),
            Some(Message::Unlock) if n >= 6 => self.handle_unlock(n),
            Some(Message::Lock) if n >= 6 => self.handle_lock(n),
            Some(Message::LockAll) if n >= 5 => self.handle_lock_all(),
            Some(Message::Unsubscribe) if n >= 5 => self.handle_subscription(n, false),
            _ => {}
        }
    }

    fn handle_data(&mut self, n: usize) {
        let epoch = epoch_of(&self.rx);
        if epoch == 0 || epoch == self.local_epoch {
            return; // unknown publisher or our own broadcast
        }
        let Some(pi) = self.peers.iter().position(|p| p.epoch == epoch) else {
            return;
        };
        let now = clock::now_ms();
        self.peers[pi].online = true;
        self.peers[pi].last_seen_ms = now;
        let pkt = u16::from_le_bytes(self.rx[5..7].try_into().unwrap()) as usize;
        if pkt >= MAX_PACKETS {
            return;
        }
        self.peers[pi].last_data[pkt] = now;
        if self.playback {
            return;
        }
        for &j in &self.peers[pi].subs_by_packet[pkt] {
            if self.held.contains(&j) {
                continue; // we hold the override: our copy is authoritative
            }
            let sig = &self.signals[j];
            if sig.meta().locked.get() {
                // Cursor capped at n: a short datagram can't apply stale bytes.
                let mut r = RCursor::new(&self.rx[..n]);
                r.off = DATA_HEADER + sig.meta().net_offset.get() as usize;
                sig.value_from_bytes(&mut r);
            }
        }
    }

    fn handle_schema(&mut self, n: usize) {
        let Some(msg) = parse_schema(&self.rx[..n]) else {
            return;
        };
        if msg.node == self.local_name {
            return; // our own broadcast
        }
        let Some(pi) = self.peers.iter().position(|p| p.name == msg.node) else {
            return; // a publisher we don't subscribe to
        };

        let pc = self.publisher_count;
        let (peers, signals, by_name) = (&mut self.peers, &self.signals, &self.by_name);
        let peer = &mut peers[pi];
        if peer.epoch != msg.epoch {
            // First contact or the peer's layout changed: rebind from this
            // schema stream.
            peer.invalidate(signals);
            peer.epoch = msg.epoch;
        }
        peer.online = true;
        peer.last_seen_ms = clock::now_ms();

        let mut mismatches = 0usize;
        for e in msg.entries() {
            // Entry metadata is for tools; our signals know theirs.
            if e.pkt as usize >= MAX_PACKETS {
                continue;
            }
            let Some(candidates) = by_name.get(e.name) else {
                continue;
            };
            for &i in candidates {
                let m = signals[i].meta();
                if i < pc || m.resolved() || m.node() != peer.name {
                    continue;
                }
                match e.ty {
                    Some(t) if compatible(t, signals[i].wire_type()) => {}
                    // Unknown tag or declared type disagrees with the wire.
                    _ => {
                        mismatches += 1;
                        continue;
                    }
                }
                m.net_packet.set(e.pkt);
                m.net_offset.set(e.off as u32);
                peer.subs_by_packet[e.pkt as usize].push(i);
                peer.unresolved -= 1;
            }
        }
        self.schema_mismatch_count += mismatches;
    }

    /// Subscribe latches on (`true`, sticky until reboot) or off (`false` —
    /// any remaining consumer re-latches via its stall detector).
    fn handle_subscription(&mut self, n: usize, on: bool) {
        if epoch_of(&self.rx) != self.local_epoch {
            return; // addressed to some other publisher
        }
        let mut off = 5;
        while off + 2 <= n {
            let pkt = u16::from_le_bytes(self.rx[off..off + 2].try_into().unwrap()) as usize;
            if pkt < MAX_PACKETS {
                self.pkt_subscribed[pkt] = on;
            }
            off += 2;
        }
    }

    fn handle_unlock(&mut self, n: usize) {
        if epoch_of(&self.rx) != self.local_epoch {
            return;
        }
        let Some((name, val_off)) = parse_named(&self.rx[..n]) else {
            return;
        };
        let Some(i) = self.find_publisher(name) else {
            return;
        };
        let sig = &self.signals[i];
        sig.meta().locked.set(false);
        let mut r = RCursor::new(&self.rx[..n]);
        r.off = val_off;
        sig.value_from_bytes(&mut r);
        self.unlock_lease.insert(i, clock::now_ms() + LEASE_MS);
    }

    fn handle_lock(&mut self, n: usize) {
        if epoch_of(&self.rx) != self.local_epoch {
            return;
        }
        let Some((name, _)) = parse_named(&self.rx[..n]) else {
            return;
        };
        if let Some(i) = self.find_publisher(name) {
            self.signals[i].meta().locked.set(true);
            self.unlock_lease.remove(&i);
        }
    }

    fn handle_lock_all(&mut self) {
        if epoch_of(&self.rx) == self.local_epoch {
            self.lock_all_local();
        }
    }

    fn find_publisher(&self, name: &str) -> Option<usize> {
        self.by_name
            .get(name)?
            .iter()
            .copied()
            .find(|&i| i < self.publisher_count)
    }

    /// A subscriber index by name — the consumer-side mirror of
    /// [`find_publisher`](Self::find_publisher).
    fn find_subscriber(&self, name: &str) -> Option<usize> {
        let pc = self.publisher_count;
        self.by_name.get(name)?.iter().copied().find(|&i| i >= pc)
    }

    /// A peer's current epoch, `None` while unheard/invalidated (epoch 0).
    fn peer_epoch(&self, node: &str) -> Option<u32> {
        let e = self.peers.iter().find(|p| p.name == node)?.epoch;
        (e != 0).then_some(e)
    }

    // ---------------------------------------------------------- outbound

    fn send_data(&mut self, port: &mut dyn SegmentPort) {
        let mut i = 0;
        while i < self.publisher_count {
            let pkt = self.signals[i].meta().net_packet.get();
            let start = i;
            while i < self.publisher_count && self.signals[i].meta().net_packet.get() == pkt {
                i += 1;
            }
            if !self.pkt_subscribed[pkt as usize] {
                continue; // nobody ever asked for this packet
            }
            let mut w = WCursor::new(&mut self.tx);
            w.off = DATA_HEADER;
            for s in &self.signals[start..i] {
                s.value_to_bytes(&mut w);
            }
            let end = w.off;
            self.tx[0] = Message::Data as u8;
            self.tx[1..5].copy_from_slice(&self.local_epoch.to_le_bytes());
            self.tx[5..7].copy_from_slice(&pkt.to_le_bytes());
            port.send(&self.tx[..end]);
        }
    }

    fn schema_header(&mut self) -> usize {
        let nm = self.local_name.as_bytes();
        self.tx[0] = Message::Schema as u8;
        self.tx[1..5].copy_from_slice(&self.local_epoch.to_le_bytes());
        self.tx[5] = nm.len() as u8;
        self.tx[SCHEMA_HEADER..SCHEMA_HEADER + nm.len()].copy_from_slice(nm);
        SCHEMA_HEADER + nm.len()
    }

    fn advertise_schema(&mut self, port: &mut dyn SegmentPort) {
        if self.publisher_count == 0 {
            return;
        }
        // Rate limit: at most MAX_SCHEMA_ADV_PER_TICK names per tick,
        // round-robin so every publisher is covered over successive ticks.
        let hdr = self.schema_header();
        let mut off = hdr;
        let to_send = MAX_SCHEMA_ADV_PER_TICK.min(self.publisher_count);
        for _ in 0..to_send {
            if self.schema_cursor >= self.publisher_count {
                self.schema_cursor = 0;
            }
            let s = &self.signals[self.schema_cursor];
            let (pkt, val_off, name) =
                (s.meta().net_packet.get(), s.meta().net_offset.get() as u16, s.meta().name());
            let nm = name.as_bytes();
            // The metadata block stages in a scratch field so its length
            // byte can precede it; 255 is the mlen ceiling by construction.
            let mut meta = [0u8; 255];
            let mut mw = WCursor::new(&mut meta);
            s.schema_meta_to_bytes(&mut mw);
            let m_len = mw.off;
            if off + ENTRY_HEADER + nm.len() + m_len > PACKET_CAP {
                port.send(&self.tx[..off]);
                off = hdr;
            }
            self.tx[off..off + 2].copy_from_slice(&pkt.to_le_bytes());
            self.tx[off + 2..off + 4].copy_from_slice(&val_off.to_le_bytes());
            self.tx[off + 4] = s.wire_type() as u8;
            self.tx[off + 5] = nm.len() as u8;
            self.tx[off + 6] = m_len as u8;
            self.tx[off + 7..off + 7 + nm.len()].copy_from_slice(nm);
            self.tx[off + 7 + nm.len()..off + 7 + nm.len() + m_len]
                .copy_from_slice(&meta[..m_len]);
            off += ENTRY_HEADER + nm.len() + m_len;
            self.schema_cursor += 1;
        }
        if off > hdr {
            port.send(&self.tx[..off]);
        }
    }

    /// Request only the resolved packets whose data isn't flowing yet (or
    /// stalled — a restarted publisher forgets its latches). Once every
    /// packet streams, this sends nothing.
    fn send_subscribes(&mut self, port: &mut dyn SegmentPort) {
        let now = clock::now_ms();
        let (peers, tx) = (&self.peers, &mut self.tx);
        for peer in peers {
            if peer.epoch == 0 {
                continue;
            }
            let stalled = peer.subs_by_packet.iter().enumerate().filter_map(|(pkt, chain)| {
                if chain.is_empty() {
                    return None; // nothing bound to this packet
                }
                let last = peer.last_data[pkt];
                let flowing = last != 0 && now.saturating_sub(last) < LEASE_MS;
                (!flowing).then_some(pkt as u16)
            });
            send_pkt_list(tx, port, Message::Subscribe, peer.epoch, stalled);
        }
    }

    fn refresh_unlocks(&mut self, port: &mut dyn SegmentPort) {
        for k in 0..self.held.len() {
            let sig = &self.signals[self.held[k]];
            let Some(epoch) = self.peer_epoch(&sig.meta().node()) else {
                continue;
            };
            let mut val = [0u8; crate::signal::STRING_WIRE_BYTES]; // the widest wire value
            let mut w = WCursor::new(&mut val);
            sig.value_to_bytes(&mut w);
            let (name, len) = (sig.meta().name(), w.off);
            send_named(&mut self.tx, port, Message::Unlock, epoch, &name, &val[..len]);
        }
    }

    /// Each pending pulse is one Unlock carrying our local copy's value
    /// followed straight away by a Lock — a momentary override. Loss is
    /// benign: an orphaned Unlock relocks at lease expiry, an orphaned Lock
    /// is a no-op.
    fn flush_pulses(&mut self, port: &mut dyn SegmentPort) {
        let pending = core::mem::take(&mut self.pending_pulses);
        for (i, val) in pending {
            let name = self.signals[i].meta().name();
            let Some(epoch) = self.peer_epoch(&self.signals[i].meta().node()) else {
                continue; // owner unreachable; nothing to clear
            };
            send_named(&mut self.tx, port, Message::Unlock, epoch, &name, &val);
            send_named(&mut self.tx, port, Message::Lock, epoch, &name, &[]);
        }
    }

    fn flush_locks(&mut self, port: &mut dyn SegmentPort) {
        let pending = core::mem::take(&mut self.pending_locks);
        for i in pending {
            let name = self.signals[i].meta().name();
            let Some(epoch) = self.peer_epoch(&self.signals[i].meta().node()) else {
                continue; // owner unreachable: its lease expiry relocks anyway
            };
            send_named(&mut self.tx, port, Message::Lock, epoch, &name, &[]);
        }
        let pending = core::mem::take(&mut self.pending_lock_all);
        for pi in pending {
            let epoch = self.peers[pi].epoch;
            if epoch == 0 {
                continue;
            }
            self.tx[0] = Message::LockAll as u8;
            self.tx[1..5].copy_from_slice(&epoch.to_le_bytes());
            port.send(&self.tx[..5]);
        }
    }

    // --------------------------------------------------------- overrides

    /// Take over one of a peer's publishers: `set_raw` your local copy to
    /// the desired value, then call this. The manager holds the unlock — the
    /// lease is renewed every tick and your copy stays authoritative (the
    /// publisher's echo is ignored) — until [`lock`](Self::lock) hands it
    /// back. If we vanish, the publisher's lease expiry fail-safe relocks.
    pub fn unlock(&mut self, name: &str) {
        let Some(i) = self.find_subscriber(name) else {
            return;
        };
        if !self.held.contains(&i) {
            self.held.push(i);
        }
        self.pending_locks.retain(|&j| j != i);
    }

    /// Momentary override of a peer's publisher: send our local copy's value
    /// (as it is *right now* — captured before inbound data can overwrite
    /// it) once, then hand ownership straight back. This is the fault-clear
    /// path (ST `clear_fault`): `set_raw` the local copy false, `pulse` it,
    /// and the owner applies false, relocks, and resumes evaluating — a
    /// persisting condition re-trips. Returns whether a pulse was queued;
    /// a name that isn't one of our subscriptions (e.g. a fault we own
    /// ourselves) is a no-op returning false.
    pub fn pulse(&mut self, name: &str) -> bool {
        let Some(i) = self.find_subscriber(name) else {
            return false;
        };
        let mut val = alloc::vec![0u8; self.signals[i].byte_size()];
        let mut w = WCursor::new(&mut val);
        self.signals[i].value_to_bytes(&mut w);
        self.pending_pulses.retain(|&(j, _)| j != i);
        self.pending_pulses.push((i, val));
        true
    }

    /// Hand ownership of a peer's publisher back to it.
    pub fn lock(&mut self, name: &str) {
        let Some(i) = self.find_subscriber(name) else {
            return;
        };
        self.held.retain(|&j| j != i);
        self.pending_locks.push(i);
    }

    /// Hand everything back to every peer at once.
    pub fn lock_all(&mut self) {
        self.held.clear();
        self.pending_lock_all = (0..self.peers.len()).collect();
    }

    /// Relock all local publishers (drops every override we granted).
    pub fn lock_all_local(&mut self) {
        for s in &self.signals[..self.publisher_count] {
            s.meta().locked.set(true);
        }
        self.unlock_lease.clear();
    }

    // ------------------------------------------------------------- epoch

    /// FNV-1a over the node name and publisher layout: any change to the
    /// name, names, packing, or sizes yields a new epoch, which forces
    /// consumers to rebind. 0 is reserved as "unknown".
    fn calc_epoch(&self) -> u32 {
        let mut h: u32 = 2_166_136_261;
        h = fnv_fold(h, self.local_name.as_bytes());
        h = fnv_fold(h, &[0]);
        for s in &self.signals[..self.publisher_count] {
            let m = s.meta();
            h = fnv_fold(h, m.name().as_bytes());
            h = fnv_fold(h, &[0]); // separator: ("ab","c") != ("a","bc")
            h = fnv_fold(h, &m.net_packet.get().to_le_bytes());
            h = fnv_fold(h, &m.net_offset.get().to_le_bytes());
            h = fnv_fold(h, &(s.byte_size() as u32).to_le_bytes());
        }
        if h == 0 { 1 } else { h }
    }

    // ------------------------------------------------------------ status

    /// One snapshot of the manager's health — plain fields, no getters.
    pub fn status(&self) -> NetStatus {
        NetStatus {
            connected: self.connected,
            remote_online: !self.peers.is_empty() && self.peers.iter().all(|p| p.online),
            config_error: self.config_error,
            publishers: self.publisher_count,
            subscribers: self.signals.len() - self.publisher_count,
            unresolved: self.peers.iter().map(|p| p.unresolved).sum(),
            unknown_nodes: self.unknown_node_count,
            schema_mismatches: self.schema_mismatch_count,
            epoch: self.local_epoch,
        }
    }

    /// Subscriptions no peer's schema has named yet (diagnostics).
    pub fn unresolved_names(&self) -> Vec<String> {
        self.signals[self.publisher_count..]
            .iter()
            .filter(|s| !s.meta().resolved())
            .map(|s| s.meta().name())
            .collect()
    }
}

pm_group! {
    /// The manager's own health as signals — [`NetStatus`] plus cycle
    /// timing, dogfooding the framework on itself. Opt in with
    /// [`NetworkManager::publish_health`] and the group publishes under
    /// `net.*` like any app signal: pm-mon shows segment health with zero
    /// declaring code, and a recording captures network state *alongside*
    /// app state (an incident tape that shows the link dropped 200 ms
    /// before the fault).
    #[derive(Clone)]
    pub struct NetHealth {
        pub connected: PmBool,
        pub remote_online: PmBool,
        pub config_error: PmBool,
        pub publishers: PmI32,
        pub subscribers: PmI32,
        pub unresolved: PmI32,
        pub unknown_nodes: PmI32,
        pub schema_mismatches: PmI32,
        /// `begin_cycle` timing (inbound drain + lease bookkeeping).
        pub begin: PmProf,
        /// `end_cycle` timing (data + periodic sends). Ships next scan —
        /// the packet leaves before this scan's measurement completes.
        pub end: PmProf,
    }
}

impl NetHealth {
    /// Copy one status snapshot into the signals.
    fn set_status(&self, st: &NetStatus) {
        self.connected.set(st.connected);
        self.remote_online.set(st.remote_online);
        self.config_error.set(st.config_error);
        self.publishers.set(st.publishers as i32);
        self.subscribers.set(st.subscribers as i32);
        self.unresolved.set(st.unresolved as i32);
        self.unknown_nodes.set(st.unknown_nodes as i32);
        self.schema_mismatches.set(st.schema_mismatches as i32);
    }
}

/// Health snapshot returned by [`NetworkManager::status`].
#[derive(Clone, Copy, Debug, Default)]
pub struct NetStatus {
    /// Every remote peer online and every subscription resolved.
    pub connected: bool,
    /// All remote peers currently heard from (false when no remotes bound).
    pub remote_online: bool,
    pub config_error: bool,
    pub publishers: usize,
    pub subscribers: usize,
    pub unresolved: usize,
    /// Staged signals whose node was in nobody's remotes list.
    pub unknown_nodes: usize,
    /// Schema entries whose wire size disagreed with the local declaration.
    pub schema_mismatches: usize,
    /// Our epoch: segment address + layout version.
    pub epoch: u32,
}

/// Subscriber/publisher type agreement: bool-shaped tags interchange (a
/// plain bool may watch a fault), everything else must match exactly — a
/// stronger check than the old size comparison (i64 vs u64 no longer
/// slips through at 8 bytes).
fn compatible(a: WireType, b: WireType) -> bool {
    use WireType::{Bool, Fault};
    a == b || matches!((a, b), (Bool, Fault) | (Fault, Bool))
}

fn fnv_fold(mut h: u32, bytes: &[u8]) -> u32 {
    for &b in bytes {
        h = (h ^ b as u32).wrapping_mul(16_777_619);
    }
    h
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::monitor::Monitor;
    use crate::signal::WireType;
    use crate::{clock, pm_group, PmBool, PmF32, PmFault, Stamp};
    use std::collections::VecDeque;

    pub(crate) struct MockPort {
        pub sent: Vec<Vec<u8>>,
        pub inbox: VecDeque<Vec<u8>>,
    }

    impl MockPort {
        pub fn new() -> Self {
            MockPort { sent: Vec::new(), inbox: VecDeque::new() }
        }
    }

    impl SegmentPort for MockPort {
        fn recv(&mut self, buf: &mut [u8]) -> Option<usize> {
            let d = self.inbox.pop_front()?;
            buf[..d.len()].copy_from_slice(&d);
            Some(d.len())
        }
        fn send(&mut self, data: &[u8]) {
            self.sent.push(data.to_vec());
        }
    }

    /// "Broadcast": move everything each side sent into the other's inbox.
    fn shuttle(a: &mut MockPort, b: &mut MockPort) {
        b.inbox.extend(a.sent.drain(..));
        a.inbox.extend(b.sent.drain(..));
    }

    // The same group compiles on both nodes; ownership comes from `node()`.
    pm_group! {
        struct App {
            speed: PmF32 = PmF32::new().node("a"),
            cmd: PmBool = PmBool::new().node("b"),
            local_only: PmBool,
        }
    }

    struct Node {
        app: App,
        net: NetworkManager,
        port: MockPort,
    }

    fn node(local: &str, remote: &str) -> Node {
        let app = App::new();
        let mut net = NetworkManager::new();
        net.add(&app);
        net.bind(local, &[remote]);
        Node { app, net, port: MockPort::new() }
    }

    fn scan(n: &mut Node) {
        n.net.begin_cycle(&mut n.port);
        n.net.end_cycle(&mut n.port);
    }

    /// Run both nodes with traffic flowing until `t_ms`, 50 ms scans.
    fn run_until(a: &mut Node, b: &mut Node, t_ms: u64) {
        while clock::now_ms() < t_ms {
            clock::set(clock::now_ms() + 50);
            scan(a);
            scan(b);
            shuttle(&mut a.port, &mut b.port);
        }
    }

    fn connect() -> (Node, Node) {
        clock::set(0);
        let mut a = node("a", "b");
        let mut b = node("b", "a");
        run_until(&mut a, &mut b, 500); // schema + subscribe round trips
        assert!(a.net.status().connected && b.net.status().connected);
        (a, b)
    }

    #[test]
    fn bind_classifies_and_stamps_epoch() {
        clock::set(0);
        let a = node("a", "b");
        assert_eq!(a.net.status().publishers, 1); // speed
        assert_eq!(a.net.status().subscribers, 1); // cmd
        assert_eq!(a.net.status().unresolved, 1);
        assert_ne!(a.net.status().epoch, 0);
        assert!(!a.net.status().config_error); // local_only dropped silently
        assert_eq!(a.net.unresolved_names(), vec!["cmd"]);
    }

    #[test]
    fn same_layout_different_node_gets_different_epoch() {
        clock::set(0);
        let a = node("a", "b");
        let b = node("b", "a");
        // The epoch is the publisher's segment address: it must differ even
        // when two nodes publish identically-shaped packets.
        assert_ne!(a.net.status().epoch, b.net.status().epoch);
    }

    #[test]
    fn unknown_node_is_counted_not_subscribed() {
        clock::set(0);
        let app = App::new();
        let mut net = NetworkManager::new();
        net.add(&app);
        net.bind("a", &[]); // no remotes registered at all
        assert_eq!(net.status().subscribers, 0);
        assert_eq!(net.status().unknown_nodes, 1); // cmd's node "b" unknown
    }

    #[test]
    fn no_data_without_a_subscriber() {
        clock::set(0);
        let mut a = node("a", "b");
        while clock::now_ms() < 600 {
            clock::set(clock::now_ms() + 50);
            scan(&mut a);
        }
        // Discovery runs, but no data flows: nobody asked.
        assert!(a.port.sent.iter().any(|d| d[0] == Message::Schema as u8));
        assert!(a.port.sent.iter().all(|d| d[0] != Message::Data as u8));
    }

    #[test]
    fn schema_then_subscribe_connects_and_flows() {
        let (mut a, mut b) = connect();

        a.app.speed.set(0.7);
        b.app.cmd.set(true);
        run_until(&mut a, &mut b, 700);

        assert_eq!(b.app.speed.val(), 0.7); // a → b
        assert!(a.app.cmd.val()); // b → a
        assert_eq!(a.net.status().unresolved, 0);
    }

    #[test]
    fn steady_state_sends_no_more_subscribes() {
        let (mut a, mut b) = connect();
        run_until(&mut a, &mut b, 1_200); // data flowing both ways

        // Fully resolved network: data + schema beacon only, no requests.
        let mut subscribes = 0;
        while clock::now_ms() < 2_400 {
            clock::set(clock::now_ms() + 50);
            scan(&mut a);
            scan(&mut b);
            subscribes += a
                .port
                .sent
                .iter()
                .chain(b.port.sent.iter())
                .filter(|d| d[0] == Message::Subscribe as u8)
                .count();
            shuttle(&mut a.port, &mut b.port);
        }
        assert_eq!(subscribes, 0);
        assert!(a.net.status().connected && b.net.status().connected);
    }

    #[test]
    fn publisher_restart_resumes_data() {
        let (mut a, mut b) = connect();
        a.app.speed.set(0.7);
        run_until(&mut a, &mut b, 800);
        assert_eq!(b.app.speed.val(), 0.7);

        // "a" reboots: fresh manager, subscription latches lost, but the
        // same layout means the same epoch. b's stalled data triggers a
        // re-subscribe and the stream resumes — no lease chatter needed.
        let mut a = node("a", "b");
        a.app.speed.set(0.4);
        run_until(&mut a, &mut b, clock::now_ms() + 2_000);
        assert_eq!(b.app.speed.val(), 0.4);
        assert!(b.net.status().connected);
    }

    #[test]
    fn playback_blocks_inbound_data() {
        let (mut a, mut b) = connect();
        b.net.playback = true;
        a.app.speed.set(0.9);
        run_until(&mut a, &mut b, 700);
        assert_eq!(b.app.speed.val(), 0.0); // schema/locks still ran; data didn't
        assert!(b.net.status().connected);
    }

    #[test]
    fn unlock_overrides_publisher_until_locked_back() {
        let (mut a, mut b) = connect();

        // b takes over a's "speed": seed override from b's local copy.
        b.app.speed.set_raw(0.9);
        b.net.unlock("speed");
        run_until(&mut a, &mut b, 800);

        assert!(!a.app.speed.meta().locked.get());
        assert_eq!(a.app.speed.val(), 0.9);
        a.app.speed.set(0.1); // app write bounces off the unlock
        assert_eq!(a.app.speed.val(), 0.9);
        assert_eq!(b.app.speed.val(), 0.9); // holder's copy is authoritative

        b.net.lock("speed");
        run_until(&mut a, &mut b, 900);
        assert!(a.app.speed.meta().locked.get());
        a.app.speed.set(0.1);
        assert_eq!(a.app.speed.val(), 0.1);
    }

    #[test]
    fn override_relocks_when_holder_vanishes() {
        let (mut a, mut b) = connect();

        // Peer takes an override, then goes silent.
        b.app.speed.set_raw(0.9);
        b.net.unlock("speed");
        run_until(&mut a, &mut b, 800);
        assert!(!a.app.speed.meta().locked.get());

        // a keeps scanning but no traffic arrives for > the lease.
        let deadline = clock::now_ms() + LEASE_MS + 200;
        while clock::now_ms() < deadline {
            clock::set(clock::now_ms() + 50);
            scan(&mut a);
            a.port.sent.clear(); // nothing delivered either way
        }

        assert!(a.app.speed.meta().locked.get()); // fail-safe relock
        assert!(!a.net.status().remote_online);
        assert!(!a.net.status().connected);
        drop(b);
    }

    #[test]
    fn schema_epoch_change_forces_rebind() {
        let (mut a, mut b) = connect();

        // Forge a schema from "b" whose layout (epoch) changed, no entries.
        let mut forged = alloc::vec![Message::Schema as u8];
        forged.extend_from_slice(&(b.net.status().epoch ^ 1).to_le_bytes());
        forged.push(1);
        forged.push(b'b');
        a.port.inbox.push_back(forged);
        clock::set(clock::now_ms() + 50);
        scan(&mut a);

        assert_eq!(a.net.status().unresolved, 1); // invalidated
        assert!(!a.net.status().connected);

        // The real peer's continuous schema re-binds on the next exchange.
        run_until(&mut a, &mut b, clock::now_ms() + 400);
        assert!(a.net.status().connected);
    }

    pm_group! {
        struct FaultApp {
            run: PmBool = PmBool::new().node("a"),
            over_flt: PmFault = PmFault::new().latch().node("a"),
        }
    }

    fn fault_node(local: &str, remote: &str) -> (FaultApp, NetworkManager, MockPort) {
        let app = FaultApp::new();
        let mut net = NetworkManager::new();
        net.add(&app);
        net.bind(local, &[remote]);
        (app, net, MockPort::new())
    }

    /// Display node b watches owner a's latched fault, stamps it in its
    /// FaultTable, clears it over the wire, and sees a re-trip when the
    /// condition persists — the whole ST clear_fault story.
    #[test]
    fn fault_table_clears_remote_fault_over_the_wire() {
        clock::set(0);
        let (a_app, mut a_net, mut a_port) = fault_node("a", "b");
        let (b_app, mut b_net, mut b_port) = fault_node("b", "a");
        let mut table = crate::fault_table::FaultTable::new();
        table.add(&b_app); // b is the display: table over its subscribed copy

        let mut cond = true; // the raw condition at the owner
        let step = |a_app: &FaultApp,
                        a_net: &mut NetworkManager,
                        a_port: &mut MockPort,
                        b_net: &mut NetworkManager,
                        b_port: &mut MockPort,
                        table: &mut crate::fault_table::FaultTable,
                        cond: bool| {
            clock::set(clock::now_ms() + 50);
            a_net.begin_cycle(a_port);
            b_net.begin_cycle(b_port);
            a_app.over_flt.set(cond); // owner evaluates every scan
            a_net.end_cycle(a_port);
            b_net.end_cycle(b_port);
            table.update(b_net.status().connected);
            shuttle(a_port, b_port);
        };

        for _ in 0..12 {
            step(&a_app, &mut a_net, &mut a_port, &mut b_net, &mut b_port, &mut table, cond);
        }
        assert!(b_net.status().connected);
        assert!(b_app.over_flt.val(), "fault arrived at the display");
        assert_eq!(table.records.len(), 1);
        assert!(table.records[0].active);
        let stamp = table.records[0].stamp_ms;

        // Condition drops, but the fault is latched at the owner: row stays.
        cond = false;
        for _ in 0..6 {
            step(&a_app, &mut a_net, &mut a_port, &mut b_net, &mut b_port, &mut table, cond);
        }
        assert!(a_app.over_flt.val(), "latched at the owner");
        assert_eq!(table.records.len(), 1);

        // Clear from the display: owner unlatches, row vanishes.
        table.clear(table.records[0].index, &mut b_net);
        for _ in 0..6 {
            step(&a_app, &mut a_net, &mut a_port, &mut b_net, &mut b_port, &mut table, cond);
        }
        assert!(!a_app.over_flt.val(), "owner cleared by the pulse");
        assert!(a_app.over_flt.meta().locked.get(), "owner relocked");
        assert!(table.records.is_empty());

        // Condition returns: evaluation resumed, so it re-trips and re-stamps.
        cond = true;
        for _ in 0..6 {
            step(&a_app, &mut a_net, &mut a_port, &mut b_net, &mut b_port, &mut table, cond);
        }
        assert!(a_app.over_flt.val());
        assert_eq!(table.records.len(), 1);
        assert!(table.records[0].stamp_ms > stamp, "a fresh stamp, not the old one");
    }

    /// The Monitor builds the same fault table from the wire alone and can
    /// clear a fault with no declaring code.
    #[test]
    fn monitor_stamps_and_clears_faults() {
        clock::set(0);
        let (a_app, mut a_net, mut a_port) = fault_node("a", "unused");
        let mut mon = Monitor::new();
        let mut mport = MockPort::new();

        let run = |a_app: &FaultApp, a_net: &mut NetworkManager, a_port: &mut MockPort,
                       mon: &mut Monitor, mport: &mut MockPort, cond: bool, scans: u32| {
            for _ in 0..scans {
                clock::set(clock::now_ms() + 50);
                a_net.begin_cycle(a_port);
                a_app.over_flt.set(cond);
                a_net.end_cycle(a_port);
                mon.poll(mport);
                shuttle(a_port, mport);
            }
        };

        run(&a_app, &mut a_net, &mut a_port, &mut mon, &mut mport, true, 16);
        let flt = |mon: &Monitor| {
            let f = &mon.nodes[0].faults[0];
            assert_eq!(*f.sig.meta().name.borrow(), "over_flt");
            (f.active(), f.stamp_ms)
        };
        let sig = mon.nodes[0].signals.iter().find(|e| e.meta().name() == "over_flt").unwrap();
        assert_eq!(sig.wire_type(), WireType::Fault, "faultness travels in the schema");
        let (active, stamp) = flt(&mon);
        assert!(active);
        assert!(stamp > 0, "monitor stamped the rise");

        // Latched: condition gone, still active, still stamped.
        run(&a_app, &mut a_net, &mut a_port, &mut mon, &mut mport, false, 6);
        assert!(flt(&mon).0);

        mon.clear_fault("a", "over_flt", &mut mport);
        run(&a_app, &mut a_net, &mut a_port, &mut mon, &mut mport, false, 6);
        assert!(!a_app.over_flt.val(), "owner cleared from the monitor");
        assert!(a_app.over_flt.meta().locked.get());
        let (active, stamp) = flt(&mon);
        assert!(!active);
        assert_eq!(stamp, 0, "row gone until a new rise");
    }

    /// The dogfood story: a node opts into health, and a Monitor on the
    /// segment sees the node's own link state and cycle timing as plain
    /// signals — no declaring code anywhere.
    #[test]
    fn health_publishes_status_and_cycle_timing() {
        // A fine clock that ticks on every read — unlike the scan clock,
        // which is frozen inside a cycle.
        thread_local! {
            static FAKE_US: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
        }
        fn fake_us() -> u64 {
            FAKE_US.with(|c| {
                let v = c.get() + 100;
                c.set(v);
                v
            })
        }
        clock::set(0);
        clock::install_us(fake_us);
        let app = App::new();
        let mut net = NetworkManager::new();
        net.publish_health();
        net.add(&app);
        net.bind("a", &[]);
        // speed + 8 status signals + 2 profs × 3.
        assert_eq!(net.status().publishers, 15);

        let mut port = MockPort::new();
        let mut mon = Monitor::new();
        let mut mport = MockPort::new();
        while clock::now_ms() < 3_000 {
            clock::set(clock::now_ms() + 50);
            net.begin_cycle(&mut port);
            net.end_cycle(&mut port);
            mport.inbox.extend(port.sent.drain(..));
            mon.poll(&mut mport);
            port.inbox.extend(mport.sent.drain(..));
        }

        // The manager's own numbers, read off the wire by a stranger.
        let sig = |name: &str| mon.signal("a", name).unwrap().value_text();
        assert_eq!(sig("net.publishers"), "15");
        assert_eq!(sig("net.unresolved"), "0");
        assert_eq!(sig("net.connected"), "0"); // no remotes bound: never "connected"
        assert_eq!(sig("net.config_error"), "0");
        // The cycle profs measured real (fake-fine-clock) time.
        assert!(net.health().unwrap().begin.last_us.val() > 0);
        assert!(mon.signal("a", "net.begin.avg_us").is_some());
        assert!(mon.signal("a", "net.end.max_us").is_some());
    }

    #[test]
    fn health_after_bind_is_a_config_error() {
        clock::set(0);
        let mut net = NetworkManager::new();
        net.bind("a", &[]);
        net.publish_health();
        assert!(net.status().config_error);
        assert!(net.health().is_none());
    }

    #[test]
    fn monitor_discovers_subscribes_and_decodes() {
        clock::set(0);
        let mut a = node("a", "b");
        a.app.speed.set(0.7);

        let mut mon = Monitor::new();
        let mut mport = MockPort::new();
        while clock::now_ms() < 800 {
            clock::set(clock::now_ms() + 50);
            scan(&mut a);
            mon.poll(&mut mport);
            shuttle(&mut a.port, &mut mport);
        }

        let p = mon.nodes.iter().find(|p| p.node == "a").expect("publisher discovered");
        let e = p.signals.iter().find(|e| e.meta().name() == "speed").expect("signal entry");
        assert_eq!(e.wire_type(), WireType::F32);
        // Data flowed because the monitor itself held the lease — no other
        // subscriber exists.
        assert_eq!(e.value_text(), "0.700");
    }
}
