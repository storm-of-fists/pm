//! pm-control-host — the std side of a pm-control-core node.
//!
//! Everything a real OS process needs around the `no_std` core lives here:
//! sockets, the save file, and the blackbox file today; login lands next
//! (ROADMAP.md phase 1).
//! The `pm-mon` binary drives the core `Monitor`/`Recording` off the wire.

use pm_control_core::{SaveSet, SegmentPort};
use std::fs::File;
use std::io::{Seek, SeekFrom, Write};
use std::net::{SocketAddr, UdpSocket};
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::Instant;

static US_EPOCH: OnceLock<Instant> = OnceLock::new();

fn us_since_start() -> u64 {
    US_EPOCH.get().map_or(0, |t0| t0.elapsed().as_micros() as u64)
}

/// Install the std fine clock (`Instant`-backed microseconds) behind
/// `pm_control_core::clock::now_us` — call once at process start so
/// [`PmProf`](pm_control_core::PmProf) sections and `NetworkManager`
/// health timing measure real time. Micros install a cycle-counter fn
/// instead; without either, profs read 0.
pub fn install_us_clock() {
    US_EPOCH.get_or_init(Instant::now);
    pm_control_core::clock::install_us(us_since_start);
}

/// [`SegmentPort`] over a std UDP socket: nonblocking receive, and "broadcast"
/// = send to every peer on the segment list (a real broadcast address is
/// just a one-entry list).
pub struct UdpSegmentPort {
    pub sock: UdpSocket,
    pub peers: Vec<SocketAddr>,
}

impl UdpSegmentPort {
    /// Bind and go nonblocking; broadcast peers are allowed. Panics on a bad
    /// address — a node that can't reach its segment has nothing else to do.
    pub fn bind(bind: &str, peers: &[&str]) -> UdpSegmentPort {
        let sock = UdpSocket::bind(bind).expect("bind udp socket");
        sock.set_nonblocking(true).unwrap();
        let _ = sock.set_broadcast(true);
        let peers = peers
            .iter()
            .map(|p| p.parse().expect("peer address like 10.0.0.255:42500"))
            .collect();
        UdpSegmentPort { sock, peers }
    }
}

impl SegmentPort for UdpSegmentPort {
    fn recv(&mut self, buf: &mut [u8]) -> Option<usize> {
        self.sock.recv(buf).ok() // WouldBlock → None
    }
    fn send(&mut self, data: &[u8]) {
        for peer in &self.peers {
            let _ = self.sock.send_to(data, peer);
        }
    }
}

/// The save file — persists a [`SaveSet`] and hydrates it on boot.
/// Replaces the ST `File` FB + SysFile plumbing; the line format and load
/// semantics live in core ([`SaveSet::to_text`]/[`from_text`]).
///
/// Boot: `load()` once. Every scan (or wherever the app likes): `persist()`.
/// Persisting before loading is refused — the ST `save_loaded` guard: never
/// clobber a file you haven't read.
///
/// Flagged deviations from ST:
/// - ST rewrote the file every scan; `persist` skips identical content
///   (same durability, far less flash/USB wear).
/// - Writes go through a `.tmp` + rename, so a power cut mid-write leaves
///   the old file intact instead of a torn one (ST seeked to 0 and wrote
///   in place).
///
/// [`from_text`]: SaveSet::from_text
pub struct SaveFile {
    pub path: PathBuf,
    loaded: bool,
    last_text: String,
}

impl SaveFile {
    pub fn open(path: impl Into<PathBuf>) -> SaveFile {
        SaveFile { path: path.into(), loaded: false, last_text: String::new() }
    }

    /// Hydrate the save set from the file, once at boot. A missing or
    /// empty file is a fresh machine, not an error — the set keeps its
    /// defaults and the first `persist` creates the file. Returns how
    /// many signals were hydrated.
    pub fn load(&mut self, saves: &SaveSet) -> usize {
        let text = std::fs::read_to_string(&self.path).unwrap_or_default();
        let applied = saves.from_text(&text);
        self.last_text = text;
        self.loaded = true;
        applied
    }

    /// Rewrite the file from the set's current values when they changed.
    /// Call as often as you like — every scan matches the ST cadence.
    /// Returns whether a write happened.
    pub fn persist(&mut self, saves: &SaveSet) -> std::io::Result<bool> {
        if !self.loaded {
            return Ok(false); // the save_loaded guard
        }
        let mut text = String::new();
        saves.to_text(&mut text);
        if text == self.last_text {
            return Ok(false);
        }
        let tmp = self.path.with_extension("tmp");
        std::fs::write(&tmp, &text)?;
        std::fs::rename(&tmp, &self.path)?;
        self.last_text = text;
        Ok(true)
    }
}

/// The blackbox file — a rolling window of the machine's own signals.
/// Replaces the ST `active_recordings[0]` + SysFile plumbing; the header
/// and row text come from a core `Recording` over the registrar's signals.
///
/// Boot: [`create`] truncates and writes the two header rows. Every scan:
/// [`append`] with `recording.take()`. When the row region reaches
/// `row_cap_bytes`, the write position wraps back to just past the headers
/// and overwrites — ST wrapped on a 1-minute timer (`blackbox_wrap`);
/// capping on bytes instead is a flagged deviation (the timer was a proxy
/// for file size, and rows are timestamped so a reader reorders the two
/// runs; one torn line at the boundary is possible, and core `Playback`
/// skips it).
///
/// [`snapshot`] copies the whole file to `<name>.csv` beside it — the ST
/// fault-snapshot `cp`; feed it the core `SnapshotTrigger`'s `due()`.
///
/// [`create`]: BlackboxFile::create
/// [`append`]: BlackboxFile::append
/// [`snapshot`]: BlackboxFile::snapshot
pub struct BlackboxFile {
    pub path: PathBuf,
    file: File,
    data_start: u64,
    wrap_at: u64,
    pos: u64,
}

impl BlackboxFile {
    pub fn create(
        path: impl Into<PathBuf>,
        header: &str,
        row_cap_bytes: u64,
    ) -> std::io::Result<BlackboxFile> {
        let path = path.into();
        let mut file = File::create(&path)?;
        file.write_all(header.as_bytes())?;
        let data_start = header.len() as u64;
        Ok(BlackboxFile { path, file, data_start, wrap_at: data_start + row_cap_bytes, pos: data_start })
    }

    /// Append drained rows, wrapping to the data start when the cap is hit.
    pub fn append(&mut self, rows: &str) -> std::io::Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        if self.pos + rows.len() as u64 > self.wrap_at && self.pos > self.data_start {
            self.pos = self.data_start;
            self.file.seek(SeekFrom::Start(self.pos))?;
        }
        self.file.write_all(rows.as_bytes())?;
        self.pos += rows.len() as u64;
        Ok(())
    }

    /// Copy the blackbox to `<name>.csv` in the same directory and return
    /// the copy's path.
    pub fn snapshot(&mut self, name: &str) -> std::io::Result<PathBuf> {
        self.file.sync_data()?;
        let dest = self.path.with_file_name(format!("{name}.csv"));
        std::fs::copy(&self.path, &dest)?;
        Ok(dest)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pm_control_core::{PmF32, PmString, Recording, Registrar, pm_group};

    pm_group! {
        struct Cfg {
            gain: PmF32 = PmF32::new().range(0.0, 10.0).save(),
            serial: PmString = PmString::new().save(),
        }
    }

    fn scratch(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("pm-save-{}-{name}", std::process::id()))
    }

    #[test]
    fn boot_load_then_persist_round_trip() {
        let path = scratch("roundtrip.dat");
        let _ = std::fs::remove_file(&path);

        // First boot: nothing on disk, defaults stay, first persist writes.
        let a = Cfg::new();
        let set = SaveSet::collect(&a);
        let mut file = SaveFile::open(&path);
        assert_eq!(file.load(&set), 0);
        a.gain.set(2.5);
        a.serial.set("SN-1");
        assert!(file.persist(&set).unwrap());
        assert!(!file.persist(&set).unwrap()); // unchanged: no rewrite

        // Second boot: a fresh group hydrates from the file.
        let b = Cfg::new();
        let set_b = SaveSet::collect(&b);
        let mut file_b = SaveFile::open(&path);
        assert_eq!(file_b.load(&set_b), 2);
        assert_eq!(b.gain.val(), 2.5);
        assert_eq!(b.serial.val(), "SN-1");

        let _ = std::fs::remove_file(&path);
    }

    pm_group! {
        struct Bb {
            speed: PmF32,
        }
    }

    #[test]
    fn blackbox_file_wraps_and_snapshots() {
        let path = scratch("blackbox.csv");
        let snap = path.with_file_name("over_temp_flt.csv");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&snap);

        let app = Bb::new();
        let reg = Registrar::collect(&app);
        let mut rec = Recording::new(reg.signals.clone());
        pm_control_core::clock::set(1_000); // t=0 writes read as "never written"
        app.speed.set(0.0);
        // Rows are "1000,0.000\n" = 11 bytes; cap fits two.
        let mut file = BlackboxFile::create(&path, &rec.header, 22).unwrap();

        for t in [1000u64, 2000, 3000] {
            rec.sample(t);
            file.append(&rec.take()).unwrap();
        }
        // Third row wrapped over the first: header intact, 2000 survives.
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            text,
            "utc_time,speed\n[unit=ms],[type=f32]\n\
             3000,0.000\n\
             2000,0.000\n"
        );

        let copy = file.snapshot("over_temp_flt").unwrap();
        assert_eq!(copy, snap);
        assert_eq!(std::fs::read_to_string(&snap).unwrap(), text);

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&snap);
    }

    #[test]
    fn persist_refuses_until_loaded() {
        let path = scratch("guard.dat");
        std::fs::write(&path, "gain=9.0 \n").unwrap();

        let a = Cfg::new();
        let set = SaveSet::collect(&a);
        let mut file = SaveFile::open(&path);
        a.gain.set(1.0);
        // No load yet: a write here would wipe the 9.0 on disk.
        assert!(!file.persist(&set).unwrap());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "gain=9.0 \n");

        assert_eq!(file.load(&set), 1);
        assert_eq!(a.gain.val(), 9.0); // disk wins over the pre-boot set

        let _ = std::fs::remove_file(&path);
    }
}
