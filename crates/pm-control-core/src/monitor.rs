//! Segment monitor — the "CLI tool" side of the broadcast protocol.
//!
//! Discovers every publisher from its broadcast schema advertisements,
//! holds subscribe leases on all their packets, and keeps the latest
//! decoded value per signal. Needs no declaring code: names, types, and
//! layout all come off the wire. Feed it the same [`SegmentPort`] a
//! [`NetworkManager`](crate::NetworkManager) would use.
//!
//! ```ignore
//! let mut mon = Monitor::new();
//! loop {
//!     clock::advance(dt);
//!     mon.poll(&mut port);
//!     for p in &mon.nodes {
//!         for e in &p.signals {
//!             println!("{}.{} = {}", p.node, e.meta().name(), e.value_text());
//!         }
//!     }
//! }
//! ```

use alloc::rc::Rc;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use crate::clock;
use crate::net::{
    DATA_HEADER, LEASE_MS, MAX_PACKETS, MTU, Message, SegmentPort, TICK_MS, epoch_of,
    parse_schema, send_named, send_pkt_list,
};
use crate::signal::{AnySignal, RCursor, WireType, wire_value_from_text};

/// The monitor-side fault table row for one [`WireType::Fault`] signal —
/// stamp bookkeeping is fault-*table* state, not signal state, so it
/// rides beside the shadow the same way `Registrar` keeps `faults` beside
/// `signals`.
pub struct MonFault {
    /// The fault's shadow signal (also present in `MonNode::signals`).
    pub sig: Rc<dyn AnySignal>,
    /// When this fault last rose (0 = not stamped). A stamped fault stays
    /// in the fault table — even after its value drops — until cleared.
    pub stamp_ms: u64,
    prev_active: bool,
}

impl MonFault {
    /// Currently raised (independent of the stamp).
    pub fn active(&self) -> bool {
        self.sig.value_text() == "1"
    }
    /// Same rules as the app-side FaultTable: stamp on rise (incl. a fault
    /// already up at discovery), hold the stamp until cleared. 0 means
    /// unstamped, so clamp to 1.
    fn update(&mut self) {
        let v = self.active();
        if v && !self.prev_active && self.stamp_ms == 0 {
            self.stamp_ms = clock::now_ms().max(1);
        }
        self.prev_active = v;
    }
}

/// One publisher heard on the segment. Its signals are *shadows*: real
/// signal objects materialized from the schema ([`WireType::make_signal`])
/// and configured by it (`schema_meta_from_bytes`), so every signal tool —
/// recordings above all — works over local and remote signals alike.
/// Name/node/packet/offset live in each shadow's [`crate::signal::Meta`];
/// `meta().last_write_ms` is stamped by each data arrival (the one
/// freshness field again, 0 = no data yet).
pub struct MonNode {
    pub node: String,
    pub epoch: u32,
    pub last_seen_ms: u64,
    pub signals: Vec<Rc<dyn AnySignal>>,
    /// Fault-table rows over the fault-typed shadows.
    pub faults: Vec<MonFault>,
}

/// One unlock we hold on a publisher's signal: while held, the owner's app
/// has lost the set capability and our value is authoritative.
pub struct MonUnlock {
    pub node: String,
    pub name: String,
    /// The written value's wire bytes, re-sent with every renewal.
    pub value: Vec<u8>,
}

pub struct Monitor {
    pub nodes: Vec<MonNode>,
    /// Unlocks we hold, renewed every tick until locked back.
    pub unlocks: Vec<MonUnlock>,
    next_tick_ms: u64,
    rx: [u8; MTU],
    tx: [u8; MTU],
}

impl Default for Monitor {
    fn default() -> Self {
        Self::new()
    }
}

impl Monitor {
    pub fn new() -> Self {
        Monitor {
            nodes: Vec::new(),
            unlocks: Vec::new(),
            next_tick_ms: 0,
            rx: [0; MTU],
            tx: [0; MTU],
        }
    }

    /// The publisher heard as `node`, if discovered.
    pub fn node(&self, node: &str) -> Option<&MonNode> {
        self.nodes.iter().find(|p| p.node == node)
    }

    fn node_mut(&mut self, node: &str) -> Option<&mut MonNode> {
        self.nodes.iter_mut().find(|p| p.node == node)
    }

    /// The shadow signal `node.name`, if discovered.
    pub fn signal(&self, node: &str, name: &str) -> Option<&Rc<dyn AnySignal>> {
        self.node(node)?.signals.iter().find(|s| *s.meta().name.borrow() == name)
    }

    /// Call once per scan: drains inbound datagrams, prunes publishers that
    /// went silent, and renews the subscribe leases that keep every
    /// discovered packet flowing.
    pub fn poll(&mut self, port: &mut dyn SegmentPort) {
        while let Some(n) = port.recv(&mut self.rx) {
            let n = n.min(MTU);
            if n == 0 {
                continue;
            }
            match Message::from_u8(self.rx[0]) {
                Some(Message::Schema) if n >= 6 => self.handle_schema(n),
                Some(Message::Data) if n > DATA_HEADER => self.handle_data(n),
                _ => {}
            }
        }
        let now = clock::now_ms();
        // Keep silent publishers around a while — a CLI wants to show "last
        // seen 8s ago", not a vanishing row.
        self.nodes.retain(|p| now.saturating_sub(p.last_seen_ms) < 30 * LEASE_MS);
        if now >= self.next_tick_ms {
            self.next_tick_ms = now + TICK_MS;
            self.subscribe_all(port);
            self.renew_unlocks(port);
        }
    }

    /// Held unlocks are the one continuously renewed request: re-send each
    /// one every tick, or the owner's fail-safe relocks after [`LEASE_MS`].
    /// Epochs are looked up fresh, so an unlock survives a publisher
    /// restart (same layout → same epoch, relocked on boot → re-unlocked
    /// here).
    fn renew_unlocks(&mut self, port: &mut dyn SegmentPort) {
        let (nodes, unlocks, tx) = (&self.nodes, &self.unlocks, &mut self.tx);
        for o in unlocks {
            let Some(p) = nodes.iter().find(|p| p.node == o.node) else {
                continue;
            };
            send_named(tx, port, Message::Unlock, p.epoch, &o.name, &o.value);
        }
    }

    fn handle_schema(&mut self, n: usize) {
        let Some(msg) = parse_schema(&self.rx[..n]) else {
            return;
        };
        let pi = match self.nodes.iter().position(|p| p.node == msg.node) {
            Some(pi) => pi,
            None => {
                self.nodes.push(MonNode {
                    node: msg.node.to_string(),
                    epoch: 0,
                    last_seen_ms: 0,
                    signals: Vec::new(),
                    faults: Vec::new(),
                });
                self.nodes.len() - 1
            }
        };
        let p = &mut self.nodes[pi];
        if p.epoch != msg.epoch {
            p.epoch = msg.epoch; // new layout: everything below re-describes it
            p.signals.clear();
            p.faults.clear();
        }
        p.last_seen_ms = clock::now_ms();

        for e in msg.entries() {
            let Some(ty) = e.ty else {
                continue; // newer wire type than we know
            };
            if e.pkt as usize >= MAX_PACKETS || e.name.is_empty() {
                continue;
            }
            match p.signals.iter().find(|s| *s.meta().name.borrow() == e.name) {
                // Metadata isn't layout, so it may change without an epoch
                // bump — refresh it from every beacon.
                Some(s) => s.schema_meta_from_bytes(e.meta),
                None => {
                    let sig = ty.make_signal();
                    *sig.meta().name.borrow_mut() = e.name.to_string();
                    *sig.meta().node.borrow_mut() = p.node.clone();
                    sig.meta().net_packet.set(e.pkt);
                    sig.meta().net_offset.set(e.off as u32);
                    sig.schema_meta_from_bytes(e.meta);
                    if ty == WireType::Fault {
                        p.faults.push(MonFault {
                            sig: sig.clone(),
                            stamp_ms: 0,
                            prev_active: false,
                        });
                    }
                    p.signals.push(sig);
                }
            }
        }
    }

    fn handle_data(&mut self, n: usize) {
        let epoch = epoch_of(&self.rx);
        let pkt = u16::from_le_bytes(self.rx[5..7].try_into().unwrap());
        let (nodes, rx) = (&mut self.nodes, &self.rx);
        let Some(p) = nodes.iter_mut().find(|p| p.epoch == epoch) else {
            return;
        };
        p.last_seen_ms = clock::now_ms();
        for e in &p.signals {
            if e.meta().net_packet.get() != pkt {
                continue;
            }
            // Cursor capped at n: a short datagram can't apply stale bytes.
            let mut r = RCursor::new(&rx[..n]);
            r.off = DATA_HEADER + e.meta().net_offset.get() as usize;
            e.value_from_bytes(&mut r);
        }
        for f in &mut p.faults {
            f.update();
        }
    }

    /// Clear a fault on its owning node — the ST `clear_fault`, spoken over
    /// the wire: the local stamp and display copy drop now, and the owner
    /// gets an unlock-override to false followed by an immediate relock, so
    /// its evaluation resumes and a persisting condition re-trips.
    pub fn clear_fault(&mut self, node: &str, name: &str, port: &mut dyn SegmentPort) {
        let Some(p) = self.node_mut(node) else {
            return;
        };
        let epoch = p.epoch;
        let Some(f) = p.faults.iter_mut().find(|f| *f.sig.meta().name.borrow() == name) else {
            return;
        };
        f.stamp_ms = 0;
        // The owner keeps broadcasting the stale true value until the pulse
        // lands — arm `prev_active` so that window can't re-stamp; the first
        // false sample re-arms rise detection.
        f.prev_active = true;
        f.sig.value_from_bytes(&mut RCursor::new(&[0])); // display copy drops now
        send_named(&mut self.tx, port, Message::Unlock, epoch, name, &[0]); // override: false
        send_named(&mut self.tx, port, Message::Lock, epoch, name, &[]);
    }

    /// Take over a publisher's signal (the CODESYS "force table", spoken
    /// over the wire as an unlock): the owner's app loses the set
    /// capability and our value is authoritative. `text` parses against the
    /// signal's schema type; the Unlock (carrying the value bytes) goes out
    /// now and is renewed every tick, so the owner's copy stays ours until
    /// [`lock`](Self::lock) hands it back — or until we stop polling and
    /// its lease fail-safe relocks. Returns false (holding nothing) when
    /// the signal is unknown, undiscovered, or the text doesn't parse.
    pub fn unlock(
        &mut self,
        node: &str,
        name: &str,
        text: &str,
        port: &mut dyn SegmentPort,
    ) -> bool {
        let Some(p) = self.node(node) else {
            return false;
        };
        if p.epoch == 0 || name.is_empty() || name.len() > 255 {
            return false;
        }
        let epoch = p.epoch;
        let Some(s) = self.signal(node, name) else {
            return false;
        };
        let Some(value) = wire_value_from_text(s.wire_type(), text) else {
            return false;
        };
        send_named(&mut self.tx, port, Message::Unlock, epoch, name, &value);
        self.unlocks.retain(|o| !(o.node == node && o.name == name));
        self.unlocks.push(MonUnlock {
            node: node.to_string(),
            name: name.to_string(),
            value,
        });
        true
    }

    /// Hand an unlocked signal back to its owner: Lock now, stop renewing.
    pub fn lock(&mut self, node: &str, name: &str, port: &mut dyn SegmentPort) {
        let held = self.unlocks.iter().any(|o| o.node == node && o.name == name);
        self.unlocks.retain(|o| !(o.node == node && o.name == name));
        if !held {
            return;
        }
        if let Some(p) = self.node(node) {
            let epoch = p.epoch;
            send_named(&mut self.tx, port, Message::Lock, epoch, name, &[]);
        }
        // Owner unreachable: its lease expiry relocks anyway.
    }

    /// Hand every unlocked signal back — the quit path.
    pub fn lock_all(&mut self, port: &mut dyn SegmentPort) {
        for o in core::mem::take(&mut self.unlocks) {
            if let Some(p) = self.nodes.iter().find(|p| p.node == o.node) {
                send_named(&mut self.tx, port, Message::Lock, p.epoch, &o.name, &[]);
            }
        }
    }

    /// Whether we currently hold the unlock on `node.name`.
    pub fn unlocked(&self, node: &str, name: &str) -> bool {
        self.unlocks.iter().any(|o| o.node == node && o.name == name)
    }

    /// Let go of every publisher's subscribe latch — the exit path of a
    /// briefly-attached tool (a latched packet otherwise streams until the
    /// publisher reboots). Not refcounted: another consumer that still
    /// wants a packet re-latches via its stall detector within a lease.
    pub fn unsubscribe_all(&mut self, port: &mut dyn SegmentPort) {
        let (nodes, tx) = (&self.nodes, &mut self.tx);
        for p in nodes {
            if p.epoch == 0 {
                continue;
            }
            let mut pkts: Vec<u16> = p.signals.iter().map(|e| e.meta().net_packet.get()).collect();
            pkts.sort_unstable();
            pkts.dedup();
            send_pkt_list(tx, port, Message::Unsubscribe, p.epoch, pkts);
        }
    }

    /// Ask for the packets whose data isn't flowing yet (or stalled — a
    /// restarted publisher forgets its subscription latches). Once every
    /// discovered signal streams, this sends nothing.
    fn subscribe_all(&mut self, port: &mut dyn SegmentPort) {
        let now = clock::now_ms();
        let (nodes, tx) = (&self.nodes, &mut self.tx);
        for p in nodes {
            if p.epoch == 0 {
                continue;
            }
            let mut pkts: Vec<u16> = p
                .signals
                .iter()
                .filter(|e| {
                    let seen = e.meta().last_write_ms.get();
                    seen == 0 || now.saturating_sub(seen) >= LEASE_MS
                })
                .map(|e| e.meta().net_packet.get())
                .collect();
            pkts.sort_unstable();
            pkts.dedup();
            send_pkt_list(tx, port, Message::Subscribe, p.epoch, pkts);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::tests::MockPort;
    use crate::net::NetworkManager;
    use crate::{clock, pm_group, PmBool, PmF32, PmI32, Stamp};

    pm_group! {
        struct App {
            speed: PmF32 = PmF32::new().range(0.0, 1.0),
            run: PmBool,
            mode: PmI32 = PmI32::new().text_list("CanRockerSwitch"),
        }
    }

    struct Pub {
        app: App,
        net: NetworkManager,
        port: MockPort,
    }

    fn publisher() -> Pub {
        let app = App::new().node("a");
        let mut net = NetworkManager::new();
        net.add(&app);
        net.bind("a", &[]);
        Pub { app, net, port: MockPort::new() }
    }

    fn scan(p: &mut Pub) {
        p.net.begin_cycle(&mut p.port);
        p.net.end_cycle(&mut p.port);
    }

    /// Publisher and monitor exchanging traffic, 50 ms scans, until `t_ms`.
    fn run(p: &mut Pub, mon: &mut Monitor, mon_port: &mut MockPort, t_ms: u64) {
        while clock::now_ms() < t_ms {
            clock::set(clock::now_ms() + 50);
            scan(p);
            mon_port.inbox.extend(p.port.sent.drain(..));
            mon.poll(mon_port);
            p.port.inbox.extend(mon_port.sent.drain(..));
        }
    }

    /// A monitor fully attached: everything discovered, all data flowing.
    fn attach() -> (Pub, Monitor, MockPort) {
        clock::set(0);
        let mut p = publisher();
        let mut mon = Monitor::new();
        let mut mon_port = MockPort::new();
        run(&mut p, &mut mon, &mut mon_port, 3_000);
        assert_eq!(mon.nodes.len(), 1);
        assert!(mon.nodes[0].signals.iter().all(|s| s.meta().last_write_ms.get() != 0));
        (p, mon, mon_port)
    }

    /// Deliver the monitor's pending sends and let the publisher scan once.
    fn deliver(p: &mut Pub, mon_port: &mut MockPort) {
        p.port.inbox.extend(mon_port.sent.drain(..));
        scan(p);
    }

    #[test]
    fn unlock_takes_over_renews_and_locks_back() {
        let (mut p, mut mon, mut mon_port) = attach();
        p.app.speed.set(1.0);
        assert!(mon.unlock("a", "speed", "2.5", &mut mon_port));
        deliver(&mut p, &mut mon_port);
        assert_eq!(p.app.speed.val(), 2.5);
        assert!(!p.app.speed.meta().locked.get()); // owner's app locked out
        p.app.speed.set(9.0);
        assert_eq!(p.app.speed.val(), 2.5); // app write blocked while unlocked

        // Held far past LEASE_MS while the monitor keeps polling: the tick
        // renewal is doing its job.
        let t = clock::now_ms();
        run(&mut p, &mut mon, &mut mon_port, t + 3 * LEASE_MS);
        assert!(!p.app.speed.meta().locked.get());
        assert_eq!(p.app.speed.val(), 2.5);
        assert!(mon.unlocked("a", "speed"));

        mon.lock("a", "speed", &mut mon_port);
        deliver(&mut p, &mut mon_port);
        assert!(p.app.speed.meta().locked.get());
        assert!(!mon.unlocked("a", "speed"));
        p.app.speed.set(0.9);
        assert_eq!(p.app.speed.val(), 0.9); // owner back in charge
    }

    #[test]
    fn unlock_fail_safe_relocks_when_monitor_vanishes() {
        let (mut p, mut mon, mut mon_port) = attach();
        assert!(mon.unlock("a", "speed", "2.5", &mut mon_port));
        deliver(&mut p, &mut mon_port);
        assert!(!p.app.speed.meta().locked.get());
        // The monitor dies without locking back: renewals stop, the
        // publisher scans on alone and its lease expiry takes the signal
        // back.
        let t = clock::now_ms();
        while clock::now_ms() < t + 2 * LEASE_MS {
            clock::set(clock::now_ms() + 50);
            scan(&mut p);
        }
        assert!(p.app.speed.meta().locked.get());
    }

    #[test]
    fn unlock_rejects_unknown_or_unparseable() {
        let (_p, mut mon, mut mon_port) = attach();
        assert!(!mon.unlock("a", "nope", "1", &mut mon_port));
        assert!(!mon.unlock("ghost", "speed", "1", &mut mon_port));
        assert!(!mon.unlock("a", "speed", "fast", &mut mon_port));
        assert!(mon.unlocks.is_empty());
    }

    #[test]
    fn schema_carries_configured_metadata() {
        let (_p, mon, _mon_port) = attach();
        let sig =
            |name: &str| mon.nodes[0].signals.iter().find(|s| s.meta().name() == name).unwrap();
        // Configured bounds travel; the type's full range doesn't. The
        // configured text list travels; the bool map is synthesized by the
        // shadow's own type — one metadata_text either side of the wire.
        assert_eq!(sig("speed").metadata_text(), "[0.000..1.000]");
        assert_eq!(sig("mode").metadata_text(), "[CanRockerSwitch]");
        assert_eq!(sig("run").metadata_text(), crate::signal::BOOL_META);
    }

    #[test]
    fn unsubscribe_all_stops_the_stream() {
        let (mut p, mut mon, mut mon_port) = attach();
        mon.unsubscribe_all(&mut mon_port);
        deliver(&mut p, &mut mon_port); // latch drops
        p.port.sent.clear();
        // The monitor is gone; nothing re-subscribes, so no Data flows.
        let t = clock::now_ms();
        while clock::now_ms() < t + 3 * LEASE_MS {
            clock::set(clock::now_ms() + 50);
            scan(&mut p);
        }
        assert!(p.port.sent.iter().all(|d| d[0] != Message::Data as u8));
        assert!(p.port.sent.iter().any(|d| d[0] == Message::Schema as u8));
    }

    #[test]
    fn attached_monitor_recovers_its_own_unsubscribe() {
        // The not-refcounted story: an unsubscribe under a consumer that
        // still polls self-heals via the stall detector within a lease.
        let (mut p, mut mon, mut mon_port) = attach();
        mon.unsubscribe_all(&mut mon_port);
        let t = clock::now_ms();
        run(&mut p, &mut mon, &mut mon_port, t + 3 * LEASE_MS);
        let sig = &mon.nodes[0].signals[0];
        assert!(clock::now_ms().saturating_sub(sig.meta().last_write_ms.get()) < LEASE_MS);
    }
}
