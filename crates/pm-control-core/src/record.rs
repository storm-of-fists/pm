//! Recording & playback — the tape, both directions.
//!
//! ONE recorder for both ST recording stories: a [`Recording`] is a set of
//! columns, and a column is any [`AnySignal`] — the registrar's own signals
//! (the machine-side blackbox, the ST `diags[]` sweep) or a `Monitor`'s
//! shadow signals (the bench capture off the wire, pm-mon Ctrl-R). Same
//! file shape either way: a name header row, a metadata row, then one row
//! per sample, `utc_time` first. Column names are node-qualified
//! (`a.speed`) when the signal is node-stamped.
//!
//! [`Playback`] drives the other direction with the tape as the
//! *authoritative* value source — which is the whole simulation story in
//! one mechanism: matched signals are unlocked (the app's `set` is
//! blocked, and inbound subscribed data — which only applies to locked
//! signals — can't fight the tape either; set `net.playback` to shut
//! inbound out entirely, the ST `SignalManagerMode.Playback` gate), and
//! cells apply through `wire_value_from_text` → `value_from_bytes`, the
//! same path a Monitor unlock-write travels. When the tape ends (or [`stop`]),
//! the signals relock and the app owns its values again. A node in
//! playback publishes tape values over its NetworkManager like any live
//! node — replaying a field incident at the bench is just: same app, same
//! node name, tape on ("replay as ghost node"). ST declared the Playback
//! mode but never built the applying side — this part is ours.
//!
//! [`SnapshotTrigger`] is the ST end_cycle fault-snapshot arming: the
//! first fault to rise starts a 15 s delay ([`SNAPSHOT_DELAY_MS`], the ST
//! `fault_snapshot_trigger`), so the blackbox copy holds post-fault
//! context; the host consumes [`due`] and copies the file to
//! `<fault_name>.csv`.
//!
//! File IO stays host-side: `pm_control_host::BlackboxFile` owns the
//! machine file (create + header, append, the wrap, the snapshot copy);
//! pm-mon writes its own capture files. Time is caller-supplied UTC ms
//! (the core clock is relative; ST used the RTC here).
//!
//! Flagged deviations / inherited constraints:
//! - no ST trailing comma per row;
//! - ST skipped a truncated row (`rec.ovf`); rows here compose in a
//!   growable String and can't truncate;
//! - CSV cells are comma-split: string *values* must not contain commas
//!   (same family as the save-file "no spaces in strings" constraint);
//! - `Playback` parses the whole file up front (bench/sim memory, not a
//!   micro's).
//!
//! [`stop`]: Playback::stop
//! [`due`]: SnapshotTrigger::due

use alloc::rc::Rc;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::fmt::Write as _;

use crate::clock;
use crate::fault::PmFault;
use crate::signal::{AnySignal, RCursor, Registrar, wire_value_from_text};

/// ST `fault_snapshot_trigger` t_on: the blackbox copy fires this long
/// after the first fault rises, so the snapshot carries the aftermath.
pub const SNAPSHOT_DELAY_MS: u64 = 15_000;

/// A column's name in the file: `node.name` when node-stamped, bare
/// otherwise. Playback matches by the same rule.
fn column_name(s: &dyn AnySignal) -> String {
    let node = s.meta().node();
    let name = s.meta().name();
    if node.is_empty() { name } else { alloc::format!("{node}.{name}") }
}

/// One in-progress capture over a fixed set of columns (the ST header is
/// written once at recording start — signals added later join the *next*
/// recording). Columns are shared handles, so values are read at sample
/// time with no lookup; a column with no data yet (a shadow signal the
/// segment hasn't served) leaves an empty cell rather than a guess.
pub struct Recording {
    pub columns: Vec<Rc<dyn AnySignal>>,
    /// The two header rows, composed once — the host writes these at file
    /// creation, and the blackbox wrap point sits just after them.
    pub header: String,
    /// Data rows appended so far.
    pub rows: u64,
    buf: String,
}

impl Recording {
    pub fn new(columns: Vec<Rc<dyn AnySignal>>) -> Recording {
        let mut header = String::from("utc_time");
        for s in &columns {
            let _ = write!(header, ",{}", column_name(s.as_ref()));
        }
        header.push_str("\n[unit=ms]");
        for s in &columns {
            let _ = write!(header, ",[type={}]", s.wire_type().text());
            let m = s.metadata_text();
            if !m.is_empty() {
                let _ = write!(header, " {m}");
            }
        }
        header.push('\n');
        Recording { columns, header, rows: 0, buf: String::new() }
    }

    /// Append one data row — time, then every column's current value. A
    /// never-written column (`last_write_ms == 0` — a shadow the segment
    /// hasn't served, a local signal nothing has set) leaves an empty
    /// cell: "no writer has produced this value" beats a default
    /// masquerading as one, and playback skips empty cells anyway.
    pub fn sample(&mut self, utc_ms: u64) {
        let _ = write!(self.buf, "{utc_ms}");
        for s in &self.columns {
            self.buf.push(',');
            if s.meta().last_write_ms.get() != 0 {
                s.value_to_text(&mut self.buf);
            }
        }
        self.buf.push('\n');
        self.rows += 1;
    }

    /// Drain pending row text — the host appends this to its file. The
    /// headers are NOT included; they're [`header`](Recording::header),
    /// written once at file creation.
    pub fn take(&mut self) -> String {
        core::mem::take(&mut self.buf)
    }
}

/// The fault-snapshot arming (ST end_cycle): watch the registered faults,
/// and when the first one rises, schedule a blackbox copy for 15 s later.
pub struct SnapshotTrigger {
    faults: Vec<PmFault>,
    prev: Vec<bool>,
    /// First-riser fault name + the ambient-clock ms its snapshot is due.
    wait: Option<(String, u64)>,
}

impl SnapshotTrigger {
    pub fn new(reg: &Registrar) -> SnapshotTrigger {
        SnapshotTrigger {
            prev: reg.faults.iter().map(|f| f.val()).collect(),
            faults: reg.faults.clone(),
            wait: None,
        }
    }

    /// Call once per scan: the first rise arms the delay.
    pub fn update(&mut self) {
        for (f, prev) in self.faults.iter().zip(self.prev.iter_mut()) {
            let v = f.val();
            if v && !*prev && self.wait.is_none() {
                self.wait = Some((f.meta().name(), clock::now_ms() + SNAPSHOT_DELAY_MS));
            }
            *prev = v;
        }
    }

    /// The armed snapshot, once its 15 s have passed — the host copies the
    /// blackbox file to `<returned name>.csv` and the trigger disarms
    /// (later faults arm a fresh one, like the ST pending-name reset).
    pub fn due(&mut self) -> Option<String> {
        match &self.wait {
            Some((_, due)) if clock::now_ms() >= *due => {
                self.wait.take().map(|(name, _)| name)
            }
            _ => None,
        }
    }
}

/// One playable tape: a recording matched against a live registration.
/// While running, the tape owns the matched signals (they're unlocked);
/// rows apply on the recording's own time deltas against the ambient
/// clock. Signals the tape doesn't cover stay app-owned; columns the app
/// doesn't have are ignored.
pub struct Playback {
    /// Per CSV data column: the matched signal, or None (unknown name).
    columns: Vec<Option<Rc<dyn AnySignal>>>,
    /// `(recorded utc_ms, comma-joined value cells)` per data row.
    rows: Vec<(u64, String)>,
    next: usize,
    started_ms: u64,
    running: bool,
}

impl Playback {
    /// Parse a recording and match its columns to the registration by
    /// column name (node-qualified when stamped, so a tape only drives the
    /// node it was cut from). Garbled lines (a wrap boundary can leave
    /// one) and unknown columns are skipped, not errors.
    pub fn new(reg: &Registrar, csv: &str) -> Playback {
        let mut lines = csv.lines();
        let columns: Vec<Option<Rc<dyn AnySignal>>> = match lines.next() {
            Some(hdr) => hdr
                .split(',')
                .skip(1) // utc_time
                .map(|col| {
                    reg.signals.iter().find(|s| column_name(s.as_ref()) == col).cloned()
                })
                .collect(),
            None => Vec::new(),
        };
        lines.next(); // metadata row: display-only, never parsed back
        let mut rows = Vec::new();
        for line in lines {
            let Some((t, cells)) = line.split_once(',') else {
                continue;
            };
            let Ok(t) = t.parse::<u64>() else {
                continue; // wrap-torn or foreign line
            };
            rows.push((t, cells.to_string()));
        }
        Playback { columns, rows, next: 0, started_ms: 0, running: false }
    }

    /// How many columns found their signal — sanity check before starting.
    pub fn matched(&self) -> usize {
        self.columns.iter().filter(|c| c.is_some()).count()
    }

    pub fn running(&self) -> bool {
        self.running
    }

    /// Take over: unlock every matched signal and anchor the tape's clock.
    /// The first row applies on the next [`step`](Playback::step).
    pub fn start(&mut self) {
        if self.rows.is_empty() {
            return;
        }
        for sig in self.columns.iter().flatten() {
            sig.meta().locked.set(false);
        }
        self.next = 0;
        self.started_ms = clock::now_ms();
        self.running = true;
    }

    /// Apply every row now due (recorded deltas from the first row, laid
    /// against the ambient clock from [`start`](Playback::start)). Call
    /// once per scan, before the app runs, so the scan computes on tape
    /// values. Relocks and returns false once the tape ends.
    pub fn step(&mut self) -> bool {
        if !self.running {
            return false;
        }
        let elapsed = clock::now_ms() - self.started_ms;
        let t0 = self.rows[0].0;
        while self.next < self.rows.len() && self.rows[self.next].0 - t0 <= elapsed {
            let (_, cells) = &self.rows[self.next];
            for (cell, col) in cells.split(',').zip(&self.columns) {
                let Some(sig) = col else { continue };
                if cell.is_empty() {
                    continue; // a bench capture's not-yet-seen cell
                }
                if let Some(bytes) = wire_value_from_text(sig.wire_type(), cell) {
                    sig.value_from_bytes(&mut RCursor::new(&bytes));
                }
            }
            self.next += 1;
        }
        if self.next == self.rows.len() {
            self.stop();
        }
        self.running
    }

    /// Hand the values back to the app: relock every matched signal.
    /// Automatic when the tape runs out.
    pub fn stop(&mut self) {
        for sig in self.columns.iter().flatten() {
            sig.meta().locked.set(true);
        }
        self.running = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::NetworkManager;
    use crate::net::tests::MockPort;
    use crate::{Message, Monitor, PmBool, PmF32, PmFault, Stamp, pm_group};

    pm_group! {
        struct App {
            speed: PmF32 = PmF32::new().range(0.0, 1.0),
            run: PmBool,
            over_temp_flt: PmFault = PmFault::new(),
        }
    }

    fn mon_columns(mon: &Monitor, names: &[&str]) -> Vec<Rc<dyn AnySignal>> {
        names
            .iter()
            .map(|name| {
                mon.nodes[0].signals.iter().find(|s| s.meta().name() == *name).unwrap().clone()
            })
            .collect()
    }

    /// A publisher and a Monitor exchanging real datagrams until the
    /// monitor has discovered and is receiving everything.
    fn segment() -> (App, NetworkManager, MockPort, Monitor, MockPort) {
        crate::clock::set(0);
        let app = App::new().node("a");
        let mut net = NetworkManager::new();
        net.add(&app);
        net.bind("a", &[]);
        let mut net_port = MockPort::new();
        let mut mon = Monitor::new();
        let mut mon_port = MockPort::new();
        while crate::clock::now_ms() < 3_000 {
            crate::clock::set(crate::clock::now_ms() + 50);
            net.begin_cycle(&mut net_port);
            net.end_cycle(&mut net_port);
            mon_port.inbox.extend(net_port.sent.drain(..));
            mon.poll(&mut mon_port);
            net_port.inbox.extend(mon_port.sent.drain(..));
        }
        assert!(mon.nodes[0].signals.iter().all(|s| s.meta().last_write_ms.get() != 0));
        (app, net, net_port, mon, mon_port)
    }

    #[test]
    fn bench_capture_headers_then_one_row_per_sample() {
        let (app, mut net, mut net_port, mut mon, mut mon_port) = segment();
        app.speed.set(1.5);
        app.run.set(true);
        crate::clock::set(crate::clock::now_ms() + 50);
        net.begin_cycle(&mut net_port);
        net.end_cycle(&mut net_port);
        mon_port.inbox.extend(net_port.sent.drain(..));
        mon.poll(&mut mon_port);

        let mut rec = Recording::new(mon_columns(&mon, &["speed", "run"]));
        assert_eq!(
            rec.header,
            "utc_time,a.speed,a.run\n\
             [unit=ms],[type=f32] [0.000..1.000],[type=bool] [0=Inactive; 1=Active]\n"
        );
        rec.sample(1_752_000_000_000);
        assert_eq!(rec.take(), "1752000000000,1.000,1\n"); // 1.5 clamped by the range
        rec.sample(1_752_000_000_050);
        assert_eq!(rec.take(), "1752000000050,1.000,1\n");
        assert_eq!(rec.take(), ""); // drained
        assert_eq!(rec.rows, 2);
    }

    #[test]
    fn discovered_but_unserved_column_stays_empty() {
        // Schema heard, no data yet: the shadow signal exists but its cell
        // is honest about having seen nothing.
        crate::clock::set(0);
        let app = App::new().node("a");
        let mut net = NetworkManager::new();
        net.add(&app);
        net.bind("a", &[]);
        let mut net_port = MockPort::new();
        let mut mon = Monitor::new();
        let mut mon_port = MockPort::new();
        crate::clock::set(50);
        net.begin_cycle(&mut net_port);
        net.end_cycle(&mut net_port);
        mon_port.inbox.extend(
            net_port.sent.drain(..).filter(|d| d[0] == Message::Schema as u8),
        );
        mon.poll(&mut mon_port);

        let mut rec = Recording::new(mon_columns(&mon, &["speed", "run"]));
        rec.sample(7);
        assert_eq!(rec.take().lines().last().unwrap(), "7,,");
    }

    #[test]
    fn blackbox_records_the_whole_registration() {
        crate::clock::set(1_000); // t=0 writes read as "never written"
        let app = App::new();
        let reg = Registrar::collect(&app);
        let mut rec = Recording::new(reg.signals.clone());
        assert_eq!(
            rec.header,
            "utc_time,speed,run,over_temp_flt\n\
             [unit=ms],[type=f32] [0.000..1.000],[type=bool] [0=Inactive; 1=Active],[type=fault] [0=Inactive; 1=Active]\n"
        );
        app.speed.set(0.5);
        app.run.set(true);
        app.over_temp_flt.set(false); // evaluated every scan, like a real app
        rec.sample(1_752_000_000_000);
        assert_eq!(rec.take(), "1752000000000,0.500,1,0\n");
    }

    #[test]
    fn never_written_local_signal_records_an_empty_cell() {
        crate::clock::set(1_000);
        let app = App::new();
        let reg = Registrar::collect(&app);
        let mut rec = Recording::new(reg.signals.clone());
        app.speed.set(0.5); // run + fault never touched by anything
        rec.sample(7);
        assert_eq!(rec.take().lines().last().unwrap(), "7,0.500,,");
    }

    #[test]
    fn first_fault_rise_arms_a_delayed_snapshot() {
        crate::clock::set(1_000);
        let app = App::new();
        let reg = Registrar::collect(&app);
        let mut trig = SnapshotTrigger::new(&reg);

        trig.update();
        assert_eq!(trig.due(), None); // no fault, nothing armed

        app.over_temp_flt.set(true);
        trig.update();
        assert_eq!(trig.due(), None); // armed but not due

        crate::clock::set(1_000 + SNAPSHOT_DELAY_MS - 1);
        assert_eq!(trig.due(), None);
        crate::clock::set(1_000 + SNAPSHOT_DELAY_MS);
        assert_eq!(trig.due(), Some("over_temp_flt".into()));
        assert_eq!(trig.due(), None); // consumed; trigger disarmed
    }

    #[test]
    fn playback_owns_matched_signals_then_hands_back() {
        crate::clock::set(1_000); // t=0 writes read as "never written"
        // Record three scans of a changing app.
        let a = App::new();
        let reg_a = Registrar::collect(&a);
        let mut rec = Recording::new(reg_a.signals.clone());
        a.run.set(true);
        for (t, spd) in [(1_000u64, 0.25f32), (1_020, 0.50), (1_040, 0.75)] {
            a.speed.set(spd);
            rec.sample(t);
        }
        let rows = rec.take();
        let tape = alloc::format!("{}{}", rec.header, rows);

        // A fresh boot plays it back.
        let b = App::new();
        let reg_b = Registrar::collect(&b);
        let mut pb = Playback::new(&reg_b, &tape);
        assert_eq!(pb.matched(), 3);
        pb.start();
        assert!(!b.speed.meta().locked.get());
        b.speed.set(0.99); // app write while the tape owns it: blocked
        assert_eq!(b.speed.val(), 0.0);

        assert!(pb.step()); // elapsed 0: row 0 due immediately
        assert_eq!(b.speed.val(), 0.25);
        assert!(b.run.val());

        crate::clock::set(1_019);
        assert!(pb.step());
        assert_eq!(b.speed.val(), 0.25); // row 1 not due yet

        crate::clock::set(1_020);
        assert!(pb.step());
        assert_eq!(b.speed.val(), 0.50);

        crate::clock::set(1_500); // way past the end: catch up and finish
        assert!(!pb.step());
        assert_eq!(b.speed.val(), 0.75);
        assert!(b.speed.meta().locked.get()); // handed back
        b.speed.set(0.1);
        assert_eq!(b.speed.val(), 0.1); // app owns again
        assert!(!pb.step()); // stepping a stopped tape is a no-op
    }

    #[test]
    fn playback_skips_unknown_columns_and_torn_lines() {
        crate::clock::set(0);
        let app = App::new();
        let reg = Registrar::collect(&app);
        let tape = "utc_time,ghost,speed\n\
                    [unit=ms],[type=?],[type=f32]\n\
                    1000,42,0.25\n\
                    junk from a wrap boundary\n\
                    1020,43,0.75\n";
        let mut pb = Playback::new(&reg, tape);
        assert_eq!(pb.matched(), 1);
        pb.start();
        crate::clock::set(100);
        assert!(!pb.step()); // both rows due; tape ends
        assert_eq!(app.speed.val(), 0.75);
        assert!(app.run.meta().locked.get()); // untouched column never unlocked
    }

    /// The ghost node: a fresh node in playback publishes tape values onto
    /// the segment like any live node — a Monitor can't tell the difference.
    #[test]
    fn replayed_node_publishes_tape_values_to_the_segment() {
        crate::clock::set(1_000); // t=0 writes read as "never written"
        // The "field incident": speed climbs, then the machine is gone.
        let a = App::new().node("a");
        let reg_a = Registrar::collect(&a);
        let mut rec = Recording::new(reg_a.signals.clone());
        a.run.set(true);
        for (t, spd) in [(0u64, 0.25f32), (1_000, 0.50), (2_000, 0.75)] {
            a.speed.set(spd);
            rec.sample(t);
        }
        let rows = rec.take();
        let tape = alloc::format!("{}{}", rec.header, rows);
        drop(a);

        // The bench: same app shape, same node name, tape on.
        let ghost = App::new().node("a");
        let reg = Registrar::collect(&ghost);
        let mut net = NetworkManager::new();
        net.add(&ghost);
        net.bind("a", &[]);
        net.playback = true; // the ST mode gate: no inbound data at all
        let mut pb = Playback::new(&reg, &tape);
        assert_eq!(pb.matched(), 3); // node-qualified names line up
        pb.start();

        let mut net_port = MockPort::new();
        let mut mon = Monitor::new();
        let mut mon_port = MockPort::new();
        let mut seen = Vec::new();
        while crate::clock::now_ms() < 5_000 {
            crate::clock::set(crate::clock::now_ms() + 50);
            pb.step();
            net.begin_cycle(&mut net_port);
            net.end_cycle(&mut net_port);
            mon_port.inbox.extend(net_port.sent.drain(..));
            mon.poll(&mut mon_port);
            net_port.inbox.extend(mon_port.sent.drain(..));
            if let Some(n) = mon.nodes.iter().find(|n| n.node == "a")
                && let Some(s) = n.signals.iter().find(|s| s.meta().name() == "speed")
                && s.meta().last_write_ms.get() != 0
            {
                let v = s.value_text();
                if seen.last() != Some(&v) {
                    seen.push(v);
                }
            }
        }
        assert_eq!(seen, ["0.250", "0.500", "0.750"]); // the incident, replayed
        assert!(!pb.running()); // tape ended, ghost signals handed back
    }
}
