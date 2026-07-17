//! pm-mon — live signal monitor TUI for a segment of pm-control-core nodes.
//!
//! Needs no app code: nodes, signal names, types, and values are all
//! discovered from the broadcast schema/data on the wire.
//!
//! ```text
//! cargo run -p pm-control-host --bin pm-mon             # real broadcast segment on :42500
//! cargo run -p pm-control-host --bin pm-mon -- --bind 127.0.0.1:42514 127.0.0.1:42511 127.0.0.1:42512
//! ```
//!
//! The second form watches the loopback demo — run
//! `cargo run -p pm-control-host --example loopback -- --forever` in another
//! terminal first.
//!
//! One live screen: a node strip at the top (`hmi (2)  drive (2)`), your
//! pinned signals in the middle, and a search bar at the bottom whose
//! results pop up as you type — the query is a case-insensitive regex
//! (plain text works unchanged). ↑/↓ moves, Enter pins/unpins, Ctrl-A
//! pins/unpins the whole result set, Esc clears the search (again to
//! quit), Ctrl-C quits. Anything that hasn't been heard from for 2 s
//! turns red. Quitting locks back every unlock and unsubscribes, so a
//! briefly-attached pm-mon leaves the segment as it found it.
//!
//! Ctrl-U unlocks the signal under the cursor — takes the set capability
//! away from the owning app (the CODESYS "force table" story): type a
//! value (the prompt shows the signal's bounds and text-list map off the
//! schema), Enter takes the signal over (its app is locked out, row turns
//! yellow ⚡), Ctrl-U on it again locks it back. The unlock is renewed
//! every tick; if pm-mon dies, the owner's lease fail-safe relocks within
//! a second. Bounds are informational — an unlock write applies raw at
//! the owner, so out-of-range tests stay possible.
//!
//! Ctrl-R records the pinned signals: one CSV row per scan, straight off
//! the wire, into `rec_<date-time>.csv` in the working directory (the ST
//! recording file shape — names row, metadata row, then time + values).
//! Ctrl-R again stops. The strip shows a red ● REC with the row count
//! while a capture runs.
//!
//! Tab flips to the fault table: every fault heard on the segment (they
//! self-identify in the schema), stamped when it rose, listed until cleared.
//! Enter clears the selected fault at its owner — the owner resumes
//! evaluating, so a persisting condition re-trips. Esc flips back.

use pm_control_core::{AnySignal, Monitor, Recording, clock};
use pm_control_host::UdpSegmentPort;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::terminal;
use regex_lite::Regex;
use std::io::Write as _;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Visible rows of the search results before they scroll.
const LIST_ROWS: usize = 12;
/// A node or signal silent this long shows red.
const STALE_MS: u64 = 2_000;

#[derive(PartialEq)]
enum Page {
    Signals,
    Faults,
}

/// One fault-table row: (node, name, stamp_ms, active).
type FaultRow = (String, String, u64, bool);

/// Every fault on the segment that is stamped or currently active, oldest
/// stamp first (a just-re-tripped fault without a stamp yet sorts last).
fn fault_rows(mon: &Monitor) -> Vec<FaultRow> {
    let mut v: Vec<FaultRow> = Vec::new();
    for n in &mon.nodes {
        for f in &n.faults {
            if f.stamp_ms != 0 || f.active() {
                v.push((n.node.clone(), f.sig.meta().name(), f.stamp_ms, f.active()));
            }
        }
    }
    v.sort_by(|a, b| {
        let key = |r: &FaultRow| (if r.2 == 0 { u64::MAX } else { r.2 }, r.0.clone(), r.1.clone());
        key(a).cmp(&key(b))
    });
    v
}

/// An open unlock prompt: the signal being taken over and the value text
/// typed so far.
struct UnlockEdit {
    node: String,
    name: String,
    input: String,
}

/// Leave the segment as we found it: hand every unlocked signal back and
/// drop our subscribe latches (data we asked for would otherwise stream
/// until the publishers reboot).
fn detach(mon: &mut Monitor, port: &mut UdpSegmentPort) {
    mon.lock_all(port);
    mon.unsubscribe_all(port);
}

/// A running capture: rows compose in core, land in the file every loop.
struct Rec {
    recording: Recording,
    file: std::fs::File,
    path: String,
}

fn utc_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

/// `2026-07-14_18-32-05` from UTC ms — the ST `date_time_string` recording
/// name, without an RTC/timezone story yet (civil-from-days per Howard
/// Hinnant's algorithm).
fn date_time_name(ms: u64) -> String {
    let secs = ms / 1000;
    let (h, m, s) = (secs / 3600 % 24, secs / 60 % 60, secs % 60);
    let z = (secs / 86400) as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}_{h:02}-{m:02}-{s:02}")
}

#[test]
fn date_time_name_matches_utc() {
    assert_eq!(date_time_name(1_752_000_000_000), "2025-07-08_18-40-00");
    assert_eq!(date_time_name(1_782_860_399_000), "2026-06-30_22-59-59");
    assert_eq!(date_time_name(0), "1970-01-01_00-00-00");
}

/// Compact age like `43s`, `3m12s`, `1h02m`.
fn fmt_age(ms: u64) -> String {
    let s = ms / 1000;
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m{:02}s", s / 60, s % 60)
    } else {
        format!("{}h{:02}m", s / 3600, (s % 3600) / 60)
    }
}

/// Raw mode + alternate screen, restored on drop (incl. early return).
struct Screen;

impl Screen {
    fn enter() -> Screen {
        terminal::enable_raw_mode().expect("pm-mon needs a terminal");
        print!("\x1b[?1049h\x1b[?25l"); // alt screen, hide cursor
        let _ = std::io::stdout().flush();
        Screen
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        print!("\x1b[?25h\x1b[?1049l");
        let _ = std::io::stdout().flush();
        let _ = terminal::disable_raw_mode();
    }
}

fn main() {
    let mut bind = String::from("0.0.0.0:42500");
    let mut peers: Vec<String> = Vec::new();
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--bind" => bind = args.next().expect("--bind needs an address"),
            "--help" | "-h" => {
                println!("pm-mon [--bind ADDR] [PEER ...]");
                println!("regex search, ↑/↓ move, Enter pin/unpin, Ctrl-A pin all, Esc clear/quit");
                println!("Ctrl-U unlocks the signal under the cursor (type value, Enter); again locks back");
                println!("Tab flips to the fault table; there Enter clears the fault at its owner");
                println!("Ctrl-R records the pinned signals to rec_<date-time>.csv; again to stop");
                println!("quitting locks back all unlocks and unsubscribes from the segment");
                return;
            }
            peer => peers.push(peer.into()),
        }
    }
    if peers.is_empty() {
        peers.push("255.255.255.255:42500".into());
    }
    let peers: Vec<&str> = peers.iter().map(String::as_str).collect();
    let mut port = UdpSegmentPort::bind(&bind, &peers);

    let mut mon = Monitor::new();
    let mut page = Page::Signals;
    let mut query = String::new();
    let mut selected: Vec<(String, String)> = Vec::new(); // (node, name)
    let mut rec: Option<Rec> = None;
    let mut edit: Option<UnlockEdit> = None;
    let mut cursor = 0usize;
    let mut fcursor = 0usize;
    let mut scroll = 0usize;
    let start = Instant::now();
    let mut last_draw = Instant::now() - Duration::from_secs(1);

    let _screen = Screen::enter();
    loop {
        clock::set(start.elapsed().as_millis() as u64);
        mon.poll(&mut port);

        // One row per loop, ST per-scan style; a failed write ends the
        // capture rather than silently recording nothing.
        if let Some(r) = &mut rec {
            r.recording.sample(utc_ms());
            if r.file.write_all(r.recording.take().as_bytes()).is_err() {
                rec = None;
            }
        }

        let mut dirty = false;
        while event::poll(Duration::ZERO).unwrap_or(false) {
            let Ok(Event::Key(k)) = event::read() else { continue };
            if k.kind != KeyEventKind::Press {
                continue;
            }
            dirty = true;
            match k.code {
                KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                    detach(&mut mon, &mut port);
                    return;
                }
                // ---- unlock prompt (swallows everything but Ctrl-C)
                KeyCode::Esc if edit.is_some() => edit = None,
                KeyCode::Enter if edit.is_some() => {
                    let e = edit.take().unwrap();
                    if mon.unlock(&e.node, &e.name, e.input.trim(), &mut port) {
                        let item = (e.node, e.name);
                        if !selected.contains(&item) {
                            selected.push(item); // an unlocked signal stays visible
                        }
                    } else {
                        edit = Some(e); // no parse / signal gone: keep editing
                    }
                }
                KeyCode::Char(ch)
                    if edit.is_some()
                        && !k.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                {
                    edit.as_mut().unwrap().input.push(ch);
                }
                KeyCode::Backspace if edit.is_some() => {
                    edit.as_mut().unwrap().input.pop();
                }
                _ if edit.is_some() => {}
                // Ctrl-U: unlock the signal under the cursor (prompt for a
                // value); on an already-unlocked one, lock it back instead.
                KeyCode::Char('u')
                    if k.modifiers.contains(KeyModifiers::CONTROL) && page == Page::Signals =>
                {
                    let list = candidates(&mon, &query);
                    if let Some((node, name)) = list.get(cursor.min(list.len().saturating_sub(1)))
                    {
                        let (node, name) = (node.clone(), name.clone());
                        if mon.unlocked(&node, &name) {
                            mon.lock(&node, &name, &mut port);
                        } else {
                            let input = mon
                                .signal(&node, &name)
                                .map(|s| s.value_text())
                                .unwrap_or_default();
                            edit = Some(UnlockEdit { node, name, input });
                        }
                    }
                }
                // Ctrl-A: pin the whole result set (unpin when it already is).
                KeyCode::Char('a')
                    if k.modifiers.contains(KeyModifiers::CONTROL) && page == Page::Signals =>
                {
                    let list = candidates(&mon, &query);
                    if !list.is_empty() {
                        if list.iter().all(|item| selected.contains(item)) {
                            selected.retain(|s| !list.contains(s));
                        } else {
                            for item in list {
                                if !selected.contains(&item) {
                                    selected.push(item);
                                }
                            }
                        }
                    }
                }
                // Ctrl-R: record the pinned signals; again to stop. Rows are
                // on disk every loop, so stopping is just dropping the file.
                KeyCode::Char('r') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                    rec = match rec {
                        Some(_) => None,
                        None if selected.is_empty() => None,
                        None => {
                            // Pinned signals resolve to shared handles once, at
                            // start; a pin whose node has left the segment is
                            // skipped (it can't be read — flagged deviation from
                            // the old empty-column placeholder).
                            let columns: Vec<std::rc::Rc<dyn AnySignal>> = selected
                                .iter()
                                .filter_map(|(node, name)| mon.signal(node, name).cloned())
                                .collect();
                            let recording = Recording::new(columns);
                            let path = format!("rec_{}.csv", date_time_name(utc_ms()));
                            std::fs::File::create(&path).ok().and_then(|mut file| {
                                file.write_all(recording.header.as_bytes())
                                    .ok()
                                    .map(|_| Rec { recording, file, path })
                            })
                        }
                    };
                }
                KeyCode::Tab => {
                    page = if page == Page::Signals { Page::Faults } else { Page::Signals };
                }
                // ---- fault-table page
                KeyCode::Esc if page == Page::Faults => page = Page::Signals,
                KeyCode::Up if page == Page::Faults => fcursor = fcursor.saturating_sub(1),
                KeyCode::Down if page == Page::Faults => fcursor += 1, // clamped in draw
                KeyCode::Enter if page == Page::Faults => {
                    let rows = fault_rows(&mon);
                    if let Some((node, name, _, _)) =
                        rows.get(fcursor.min(rows.len().saturating_sub(1)))
                    {
                        let (node, name) = (node.clone(), name.clone());
                        mon.clear_fault(&node, &name, &mut port);
                    }
                }
                _ if page == Page::Faults => {}
                // ---- signals page
                KeyCode::Esc => {
                    if query.is_empty() {
                        detach(&mut mon, &mut port);
                        return;
                    }
                    query.clear();
                    cursor = 0;
                }
                // Plain characters only — Ctrl/Alt chords must not type.
                KeyCode::Char(c)
                    if !k.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                {
                    query.push(c);
                    cursor = 0;
                }
                KeyCode::Backspace => {
                    query.pop();
                    cursor = 0;
                }
                KeyCode::Up => cursor = cursor.saturating_sub(1),
                KeyCode::Down => cursor += 1, // clamped against the list below
                KeyCode::Enter => {
                    let list = candidates(&mon, &query);
                    if let Some(item) = list.get(cursor.min(list.len().saturating_sub(1))) {
                        match selected.iter().position(|s| s == item) {
                            Some(i) => {
                                selected.remove(i);
                            }
                            None => selected.push(item.clone()),
                        }
                    }
                }
                _ => {}
            }
        }

        // Redraw immediately on input, else every 250 ms for live values.
        if dirty || last_draw.elapsed() >= Duration::from_millis(250) {
            last_draw = Instant::now();
            match page {
                Page::Signals => {
                    let list = candidates(&mon, &query);
                    cursor = cursor.min(list.len().saturating_sub(1));
                    if cursor < scroll {
                        scroll = cursor;
                    }
                    if cursor >= scroll + LIST_ROWS {
                        scroll = cursor + 1 - LIST_ROWS;
                    }
                    scroll = scroll.min(list.len().saturating_sub(1));
                    draw(&mon, &rec, &edit, &selected, &query, &list, cursor, scroll, clock::now_ms());
                }
                Page::Faults => {
                    let rows = fault_rows(&mon);
                    fcursor = fcursor.min(rows.len().saturating_sub(1));
                    draw_faults(&mon, &rec, &rows, fcursor, clock::now_ms());
                }
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Signals matching the query as sorted (node, name) pairs; results only
/// pop up once something is typed. The query is a case-insensitive regex
/// over `node.name`; while it doesn't parse (mid-typing `[`, `(`, …) it
/// falls back to plain substring match, so typing never goes dead.
fn candidates(mon: &Monitor, query: &str) -> Vec<(String, String)> {
    if query.is_empty() {
        return Vec::new();
    }
    let re = Regex::new(&format!("(?i){query}")).ok();
    let q = query.to_lowercase();
    let mut v: Vec<(String, String)> = Vec::new();
    for n in &mon.nodes {
        for s in &n.signals {
            let full = format!("{}.{}", n.node, s.meta().name());
            let hit = match &re {
                Some(re) => re.is_match(&full),
                None => full.to_lowercase().contains(&q),
            };
            if hit {
                v.push((n.node.clone(), s.meta().name()));
            }
        }
    }
    v.sort();
    v
}

enum Fresh {
    Ok,
    Stale,
    Never,
}

/// `"node.name  value"` plus how fresh its data is.
fn signal_cell(mon: &Monitor, node: &str, name: &str, now: u64) -> (String, Fresh) {
    let (value, fresh) = match mon.signal(node, name) {
        Some(s) if s.meta().last_write_ms.get() == 0 => ("-".into(), Fresh::Never),
        Some(s) => {
            let v = s.value_text();
            let v = if v.is_empty() { "-".into() } else { v };
            if now.saturating_sub(s.meta().last_write_ms.get()) >= STALE_MS {
                (v, Fresh::Stale)
            } else {
                (v, Fresh::Ok)
            }
        }
        None => ("offline".into(), Fresh::Stale),
    };
    (format!("{:<36} {:>14}", format!("{node}.{name}"), value), fresh)
}

/// SGR prefix for a row: red when stale, dim when never seen, yellow when
/// unlocked by us (and fresh — stale news beats the unlock marker), inverse
/// under the cursor (combined when both apply).
fn sgr(fresh: &Fresh, unlocked: bool, under_cursor: bool) -> String {
    let mut s = String::new();
    if under_cursor {
        s += "\x1b[7m";
    }
    match fresh {
        Fresh::Stale => s += "\x1b[31m",
        Fresh::Never => s += "\x1b[2m",
        Fresh::Ok if unlocked => s += "\x1b[33m",
        Fresh::Ok => {}
    }
    s
}

/// The top line both pages share: `hmi (2)  drive (2)`, red when silent,
/// plus a slow-blinking fault indicator when the table isn't empty (the
/// ST `blink_fault` policy: 700 ms slow blink gated on the fault count).
fn strip_row(mon: &Monitor, rec: &Option<Rec>, now: u64) -> String {
    if mon.nodes.is_empty() {
        return "\x1b[2m(listening — no nodes heard yet)\x1b[0m".into();
    }
    let strip: Vec<String> = mon
        .nodes
        .iter()
        .map(|n| {
            let color = if now.saturating_sub(n.last_seen_ms) >= STALE_MS {
                "\x1b[1;31m"
            } else {
                "\x1b[1m"
            };
            format!("{color}{} ({})\x1b[0m", n.node, n.signals.len())
        })
        .collect();
    let mut line = strip.join("  ");
    let faults = fault_rows(mon).len();
    if faults > 0 {
        let blink = if now % 1400 < 700 { "\x1b[1;31m" } else { "\x1b[2;31m" };
        line += &format!("   {blink}⚠ {faults} fault{}\x1b[0m \x1b[2m(Tab)\x1b[0m", if faults == 1 { "" } else { "s" });
    }
    if !mon.unlocks.is_empty() {
        line += &format!("   \x1b[1;33m⚡ {} unlocked\x1b[0m", mon.unlocks.len());
    }
    if let Some(r) = rec {
        line += &format!(
            "   \x1b[1;31m● REC\x1b[0m {} \x1b[2m({} rows)\x1b[0m",
            r.path, r.recording.rows
        );
    }
    line
}

#[allow(clippy::too_many_arguments)] // a draw call is naturally this wide
fn draw(
    mon: &Monitor,
    rec: &Option<Rec>,
    edit: &Option<UnlockEdit>,
    selected: &[(String, String)],
    query: &str,
    list: &[(String, String)],
    cursor: usize,
    scroll: usize,
    now: u64,
) {
    let mut out = String::from("\x1b[H");
    let mut row = |s: &str| {
        out += s;
        out += "\x1b[K\r\n"; // overwrite in place: no full-clear flicker
    };

    row(&strip_row(mon, rec, now));
    row("");

    // --- pinned signals
    for (node, name) in selected {
        let unlocked = mon.unlocked(node, name);
        let mark = if unlocked { "⚡" } else { " " };
        let (text, fresh) = signal_cell(mon, node, name, now);
        row(&format!("{} {mark} {text}\x1b[0m", sgr(&fresh, unlocked, false)));
    }
    if !selected.is_empty() {
        row("");
    }

    // --- unlock prompt / search bar, then results
    match edit {
        Some(e) => {
            // Bounds and text-list hints ride the schema now — show them
            // beside the value being typed.
            let hint = mon
                .signal(&e.node, &e.name)
                .map(|s| match s.metadata_text() {
                    m if m.is_empty() => m,
                    m => format!(" {m}"),
                })
                .unwrap_or_default();
            row(&format!(
                "\x1b[1;33munlock\x1b[0m {}.{} = {}\u{2588}\x1b[2m{hint}  Enter take over · Esc cancel\x1b[0m",
                e.node, e.name, e.input
            ));
        }
        None => {
            let hint = if query.is_empty() {
                "  \x1b[2mregex search · Enter pin · Ctrl-A pin all · Ctrl-U unlock · Ctrl-R record · Tab faults · Esc quit\x1b[0m"
            } else {
                ""
            };
            row(&format!("\x1b[1m>\x1b[0m {query}\u{2588}{hint}"));
        }
    }
    if scroll > 0 {
        row(&format!("    \x1b[2m… {scroll} more above\x1b[0m"));
    }
    let end = (scroll + LIST_ROWS).min(list.len());
    for (i, (node, name)) in list[scroll..end].iter().enumerate() {
        let i = scroll + i;
        let unlocked = mon.unlocked(node, name);
        let mark = if unlocked {
            "⚡"
        } else if selected.contains(&(node.clone(), name.clone())) {
            "●"
        } else {
            " "
        };
        let (text, fresh) = signal_cell(mon, node, name, now);
        row(&format!("{}  {mark} {text}\x1b[0m", sgr(&fresh, unlocked, i == cursor)));
    }
    if end < list.len() {
        row(&format!("    \x1b[2m… {} more below\x1b[0m", list.len() - end));
    }
    if !query.is_empty() && list.is_empty() {
        row("    \x1b[2m(no matching signals)\x1b[0m");
    }

    out += "\x1b[J"; // clear anything left over below
    print!("{out}");
    let _ = std::io::stdout().flush();
}

fn draw_faults(mon: &Monitor, rec: &Option<Rec>, rows: &[FaultRow], cursor: usize, now: u64) {
    let mut out = String::from("\x1b[H");
    let mut row = |s: &str| {
        out += s;
        out += "\x1b[K\r\n";
    };

    row(&strip_row(mon, rec, now));
    row("");

    let active = rows.iter().filter(|r| r.3).count();
    if rows.is_empty() {
        row("\x1b[1mfaults\x1b[0m — \x1b[2mnone\x1b[0m");
    } else {
        row(&format!("\x1b[1mfaults\x1b[0m — {} listed · {active} active", rows.len()));
    }
    row("");

    for (i, (node, name, stamp, active)) in rows.iter().enumerate() {
        let age = if *stamp == 0 { "-".into() } else { fmt_age(now.saturating_sub(*stamp)) };
        let state = if *active { "\x1b[1;31mACTIVE\x1b[0m" } else { "\x1b[2mdown\x1b[0m" };
        let sel = if i == cursor { "\x1b[7m" } else { "" };
        row(&format!(
            "{sel}  {age:>7}  {:<40}\x1b[0m {state}",
            format!("{node}.{name}")
        ));
    }
    row("");
    row("\x1b[2mEnter clear · ↑/↓ · Tab signals · Esc back\x1b[0m");

    out += "\x1b[J";
    print!("{out}");
    let _ = std::io::stdout().flush();
}
