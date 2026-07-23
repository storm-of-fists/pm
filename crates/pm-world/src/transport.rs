//! QUIC transport: quinn-proto driven synchronously from pm tasks.
//!
//! No async runtime and no net thread — quinn-proto is a sans-IO state
//! machine, so a pm task pumps a non-blocking UDP socket through it every
//! tick. Channel assignment per the README networking notes:
//!
//! - unreliable datagrams: snapshots (server -> client), acks and later
//!   input (client -> server)
//! - one bidirectional reliable stream per connection, framed
//!   `[type u16][len u32][bytes]`: the handshake (peer id + schema table)
//!   goes down it, typed events come up it — events are client→server
//!   only (server→client facts are state), so the transport has no
//!   server-side event send at all
//!
//! Certificates are self-signed and the client skips verification — fine
//! for development; real deployments pin or verify.
//!
//! Why QUIC underneath: datagrams for state (unreliable, unordered —
//! newest wins), one reliable stream for the handshake and events, TLS
//! for free, and one port. The alternative was measured (`lag_ab`
//! below): raw UDP through the same simulated conditions performed
//! identically — the transport was never the bottleneck, the config
//! was. Verdict final: stay on QUIC, hybrid off the table.

use std::collections::{HashMap, VecDeque};
use std::io;
use std::net::{SocketAddr, UdpSocket};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::BytesMut;
use quinn_proto::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use quinn_proto::{
    ClientConfig, Connection, ConnectionHandle, DatagramEvent, Dir, Endpoint, EndpointConfig,
    Event, IdleTimeout, ServerConfig, StreamEvent, StreamId, TransportConfig,
};

// Bump on any dgram/header format change: cross-version connects then
// fail the TLS handshake loudly instead of misparsing each other.
// (pm/2: snapshots carry a per-send seq, acks echo (tick, seq).
//  pm/3: the hello carries the schema only; FRAME_AUTH leads with a
//  16-byte session token; FRAME_WELCOME assigns the peer id at
//  admission — the reconnect handshake.
//  pm/4: schema rows carry the pod's SCHEMA_HASH (v2 item 1 stage 2 —
//  same-size-different-meaning drift now fails the connect, and the
//  client reports a per-channel diff); DGRAM_VIEW carries the client's
//  view pose for interest scoring.)
const ALPN: &[u8] = b"pm/4";
/// Server → client at QUIC connect: the wire schema (a contract, not a
/// secret — sent before auth so the client knows the stream to answer
/// on). The peer id is NOT here anymore: identity is decided at
/// admission, where a reconnecting session can reclaim its old one.
const FRAME_HELLO: u16 = 0;
/// Client → server, the client's first frame after a schema-ok hello:
/// `[session token; 16][password bytes]` (password empty when the
/// client has none). The token is the RECONNECT identity — random per
/// game session, resent on every redial, so a dropped player's new
/// connection can claim the peer id (and therefore the parked avatar)
/// the old one held. A server WITHOUT a password ignores the password
/// part; a server WITH one defers ADMISSION — no `joined`, no
/// snapshots, no inputs/events accepted — until the bytes match, and
/// closes the connection on mismatch or timeout. A bouncer, not
/// cryptography: QUIC/TLS already encrypts the wire; this only decides
/// who gets past the door (and which door was theirs).
const FRAME_AUTH: u16 = 1;
/// Server → client at ADMISSION: `[peer id u8]`. Completes the
/// handshake (the client's `handshake_done`). Separate from the hello
/// because the id depends on the token the client hasn't sent yet at
/// hello time.
const FRAME_WELCOME: u16 = 2;
/// How long an unauthenticated connection may sit before the server
/// closes it (covers clients that never send FRAME_AUTH).
const AUTH_TIMEOUT: Duration = Duration::from_secs(5);
/// How long after a disconnect a session token may still reclaim its
/// peer id (override with [`QuicServer::reconnect_grace_set`]). Games
/// that park avatars for rejoin should use the same window.
const RECONNECT_GRACE: Duration = Duration::from_secs(20);
/// User event types must be >= this; lower values are protocol-reserved.
pub const EVENT_USER_BASE: u16 = 16;
const DGRAM_SNAPSHOT: u8 = 0;
const DGRAM_ACK: u8 = 1;
const DGRAM_INPUT: u8 = 2;
/// Client → server, unreliable, newest-wins: the client's VIEW POSE
/// (`[eye f32x3][forward f32x3]`, 24 bytes) — the on-screen-ness
/// ingredient for interest scoring (v2 item 4 stage 2). Pure
/// presentation metadata: never touches the sim, never acked; a lost
/// one is superseded by the next. Absent entirely for clients that
/// never call `view_send` (bots) — the server just sees `None`.
const DGRAM_VIEW: u8 = 3;
/// How many recent inputs ride along in every input datagram. Up to 7
/// consecutive lost packets cost nothing; beyond that the gap is skipped
/// (input is ephemeral — newest wins).
const INPUT_REDUNDANCY: usize = 8;

// --- small helpers --------------------------------------------------------

pub(crate) fn schema_encode(schema: &[(u8, String, usize, u64)]) -> Vec<u8> {
    // Sort by name so the handshake compare is registration-order
    // independent — both ends agree as long as the *set* of (kind, name,
    // size, hash) entries matches, regardless of registration order. The
    // kind byte keeps a pool and an event channel that share a name from
    // silently passing as each other; the hash (from `pm::PodSchema`,
    // usually `#[pm::pod]`-generated) catches same-size field drift.
    let mut schema = schema.to_vec();
    schema.sort_by(|a, b| a.1.cmp(&b.1));
    let mut out = Vec::new();
    out.extend_from_slice(&(schema.len() as u16).to_le_bytes());
    for (kind, name, size, hash) in &schema {
        out.push(*kind);
        out.extend_from_slice(&(name.len() as u16).to_le_bytes());
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(&(*size as u32).to_le_bytes());
        out.extend_from_slice(&hash.to_le_bytes());
    }
    out
}

/// Decode a `schema_encode` payload back into rows (`None` on a
/// malformed buffer) — only used to explain a mismatch: the CONNECT
/// decision is the byte compare, this is the diagnosis.
pub(crate) fn schema_decode(mut b: &[u8]) -> Option<Vec<(u8, String, usize, u64)>> {
    let take = |b: &mut &[u8], n: usize| -> Option<Vec<u8>> {
        (b.len() >= n).then(|| {
            let (head, rest) = b.split_at(n);
            *b = rest;
            head.to_vec()
        })
    };
    let count = u16::from_le_bytes(take(&mut b, 2)?.try_into().ok()?);
    let mut rows = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let kind = take(&mut b, 1)?[0];
        let nlen = u16::from_le_bytes(take(&mut b, 2)?.try_into().ok()?) as usize;
        let name = String::from_utf8(take(&mut b, nlen)?).ok()?;
        let size = u32::from_le_bytes(take(&mut b, 4)?.try_into().ok()?) as usize;
        let hash = u64::from_le_bytes(take(&mut b, 8)?.try_into().ok()?);
        rows.push((kind, name, size, hash));
    }
    b.is_empty().then_some(rows)
}

/// Human-readable per-channel diff for a failed schema compare: which
/// channels only one end has, and which agree on the name but not on
/// kind, size, or schema hash (hash-only = same layout size, different
/// field meaning — the drift name+size alone can't see).
pub(crate) fn schema_diff(
    server: &[(u8, String, usize, u64)],
    client: &[(u8, String, usize, u64)],
) -> String {
    let mut parts = Vec::new();
    for (kind, name, size, hash) in client {
        match server.iter().find(|(_, n, ..)| n == name) {
            None => parts.push(format!("'{name}' only on this end")),
            Some((sk, _, ss, sh)) => {
                if sk != kind || ss != size {
                    parts.push(format!("'{name}': {size} B here vs {ss} B on server"));
                } else if sh != hash {
                    parts.push(format!("'{name}': schema hash differs (field-level drift)"));
                }
            }
        }
    }
    for (_, name, ..) in server {
        if !client.iter().any(|(_, n, ..)| n == name) {
            parts.push(format!("'{name}' only on server"));
        }
    }
    if parts.is_empty() {
        // Bytes differed but rows compare equal — version skew in the
        // encoding itself (should be caught by ALPN first).
        parts.push("encoding mismatch".into());
    }
    parts.join("; ")
}

/// A random session token — unpredictability from the std hasher's
/// per-instance seed. A bouncer credential like the password, not
/// cryptography (QUIC/TLS encrypts the wire it rides). Exported as
/// `pm::session_token_random`: generate ONE per game session, keep it
/// across redials, pass it to every [`Pm::client`](crate::Pm::client)
/// via [`PmClient::session_token`](crate::PmClient::session_token).
pub fn token_random() -> [u8; 16] {
    use std::hash::{BuildHasher, Hasher};
    let mut token = [0u8; 16];
    for chunk in token.chunks_mut(8) {
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u128(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos()),
        );
        chunk.copy_from_slice(&h.finish().to_le_bytes());
    }
    token
}

fn frame_write(out: &mut Vec<u8>, ty: u16, payload: &[u8]) {
    out.extend_from_slice(&ty.to_le_bytes());
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(payload);
}

/// Parse an input datagram body: `[count u8]` then per entry
/// `[seq u32][len u16][payload]`, oldest first. Entries at or below the
/// peer's last seen sequence are redundant copies — skipped — so the
/// output is in-order and gap-tolerant without retransmits.
fn inputs_parse(data: &[u8], peer: u8, last: &mut u32, out: &mut Vec<(u8, u32, Vec<u8>)>) {
    let Some((&count, mut rest)) = data.split_first() else {
        return;
    };
    for _ in 0..count {
        if rest.len() < 6 {
            return;
        }
        let seq = u32::from_le_bytes(rest[..4].try_into().unwrap());
        let len = u16::from_le_bytes(rest[4..6].try_into().unwrap()) as usize;
        if rest.len() < 6 + len {
            return;
        }
        if seq > *last {
            *last = seq;
            out.push((peer, seq, rest[6..6 + len].to_vec()));
        }
        rest = &rest[6 + len..];
    }
}

/// Pop complete `[type u16][len u32][payload]` frames off the front.
fn frames_parse(buf: &mut Vec<u8>) -> Vec<(u16, Vec<u8>)> {
    let mut frames = Vec::new();
    let mut off = 0;
    while buf.len() - off >= 6 {
        let ty = u16::from_le_bytes(buf[off..off + 2].try_into().unwrap());
        let len = u32::from_le_bytes(buf[off + 2..off + 6].try_into().unwrap()) as usize;
        if buf.len() - off - 6 < len {
            break;
        }
        frames.push((ty, buf[off + 6..off + 6 + len].to_vec()));
        off += 6 + len;
    }
    buf.drain(..off);
    frames
}

fn stream_read(conn: &mut Connection, id: StreamId, into: &mut Vec<u8>) {
    let mut recv = conn.recv_stream(id);
    let Ok(mut chunks) = recv.read(true) else {
        return;
    };
    while let Ok(Some(chunk)) = chunks.next(64 * 1024) {
        into.extend_from_slice(&chunk.bytes);
    }
    let _ = chunks.finalize();
}

fn stream_flush(conn: &mut Connection, id: StreamId, out: &mut Vec<u8>) {
    if out.is_empty() {
        return;
    }
    if let Ok(n) = conn.send_stream(id).write(out) {
        out.drain(..n);
    }
}

fn transmits_flush(conn: &mut Connection, socket: &mut LagSocket, now: Instant) {
    let mut buf = Vec::new();
    while let Some(t) = conn.poll_transmit(now, 1, &mut buf) {
        socket.send_to(now, &buf[..t.size], t.destination);
        buf.clear();
    }
}

/// Advance a connection's timers: quinn asks to be woken at
/// `poll_timeout`; a past-due deadline is delivered here.
fn conn_tick(conn: &mut Connection, now: Instant) {
    if let Some(deadline) = conn.poll_timeout()
        && now >= deadline
    {
        conn.handle_timeout(now);
    }
}

/// The shared tail of a connection's pump: flush the reliable stream,
/// run the connection↔endpoint event roundtrip, and put what the
/// connection wants to send on the wire.
fn conn_flush(
    conn: &mut Connection,
    endpoint: &mut Endpoint,
    ch: ConnectionHandle,
    stream: Option<StreamId>,
    stream_out: &mut Vec<u8>,
    socket: &mut LagSocket,
    now: Instant,
) {
    if let Some(id) = stream {
        stream_flush(conn, id, stream_out);
    }
    while let Some(ev) = conn.poll_endpoint_events() {
        if let Some(cev) = endpoint.handle_event(ch, ev) {
            conn.handle_event(cev);
        }
    }
    transmits_flush(conn, socket, now);
}

/// Drain the UDP socket through the endpoint until it would block. Each
/// datagram either produces a [`DatagramEvent`] — handed to `on_event`
/// along with the scratch buffer quinn may have written a response into —
/// or was consumed internally.
fn socket_ingest(
    socket: &mut LagSocket,
    endpoint: &mut Endpoint,
    now: Instant,
    mut on_event: impl FnMut(&mut LagSocket, &mut Endpoint, DatagramEvent, &mut Vec<u8>),
) {
    let mut buf = [0u8; 4096];
    let mut out = Vec::new();
    while let Ok((len, from)) = socket.recv_from(now, &mut buf) {
        out.clear();
        if let Some(ev) =
            endpoint.handle(now, from, None, None, BytesMut::from(&buf[..len]), &mut out)
        {
            on_event(socket, endpoint, ev, &mut out);
        }
    }
}

fn transport_config() -> Arc<TransportConfig> {
    let mut tc = TransportConfig::default();
    // Live connections ping every 2s; anything silent for 5s is dead and
    // gets reaped (ConnectionLost -> left_drain), so a killed client's
    // entities don't linger server-side.
    tc.keep_alive_interval(Some(Duration::from_secs(2)));
    tc.max_idle_timeout(Some(IdleTimeout::try_from(Duration::from_secs(5)).unwrap()));
    // Snapshots are STATE, not a stream: a newer one supersedes anything
    // still queued, so the datagram send buffer holds a handful of
    // packets (send(.., drop=true) evicts the oldest), never seconds of
    // backlog. quinn's 1 MiB default held ~14 s of snapshots — a
    // congestion-throttled link served clients ever-staler state and
    // lag-comp rewinds starved (the acked-starvation bug, 2026-07-15).
    tc.datagram_send_buffer_size(16 * 1024);
    // BBR instead of the default Cubic: loss-based congestion control
    // reads random link loss as congestion and collapses the window,
    // throttling 60 Hz snapshots to ~half rate at PM_LOSS=0.02. BBR
    // models bandwidth/RTT instead and shrugs off random loss.
    //
    // (The PM_CC=cubic-big experiment knob was DELETED 2026-07-23 with
    // the rest of the runtime env vars — the pacer-vs-pump question it
    // existed to answer was settled by lag_ab: QUIC == raw UDP, verdict
    // final.)
    tc.congestion_controller_factory(Arc::new(quinn_proto::congestion::BbrConfig::default()));
    Arc::new(tc)
}

/// UDP socket with an optional simulated link: one-way delay and packet
/// loss applied in both directions. QUIC sees the conditions as real —
/// RTT estimates rise, retransmits and redundancy actually earn their keep.
//
// HISTORY(lag-sim): the 2026-07-15 acked-starvation bug (acked_tick at
// half rate, gap growing without bound) was the 1 MiB datagram buffer +
// Cubic-vs-loss — fixed above (16 KiB drop-oldest + BBR). The leftover
// mystery — rtt_ms ~217 ms on what was believed an ~85 ms link — was no
// mystery: PM_LAG_MS was applied by BOTH roles' sockets in-process, so
// the link was really 4x the knob (160 ms) plus pump overhead. The env
// now lags the client socket only (netmod::serve). Residual overhead on
// a true link is ~2-4 pump quanta (sim queues release + ACK timers fire
// only on the per-tick pump); the lag_ab test below measures it.
struct LagSocket {
    socket: UdpSocket,
    delay: Duration,
    loss: f32,
    rng: u32,
    out_q: VecDeque<(Instant, SocketAddr, Vec<u8>)>,
    in_q: VecDeque<(Instant, SocketAddr, Vec<u8>)>,
}

impl LagSocket {
    fn new(socket: UdpSocket) -> Self {
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(7)
            | 1;
        Self {
            socket,
            delay: Duration::ZERO,
            loss: 0.0,
            rng: seed,
            out_q: VecDeque::new(),
            in_q: VecDeque::new(),
        }
    }

    fn plain(&self) -> bool {
        self.delay.is_zero() && self.loss <= 0.0
    }

    fn drop_roll(&mut self) -> bool {
        if self.loss <= 0.0 {
            return false;
        }
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 17;
        self.rng ^= self.rng << 5;
        (self.rng as f32 / u32::MAX as f32) < self.loss
    }

    fn send_to(&mut self, now: Instant, buf: &[u8], dest: SocketAddr) {
        if self.plain() {
            let _ = self.socket.send_to(buf, dest);
            return;
        }
        if self.drop_roll() {
            return;
        }
        self.out_q.push_back((now + self.delay, dest, buf.to_vec()));
    }

    /// Release queued outgoing packets that have served their delay.
    fn flush(&mut self, now: Instant) {
        while self.out_q.front().is_some_and(|(due, ..)| *due <= now) {
            let (_, dest, data) = self.out_q.pop_front().unwrap();
            let _ = self.socket.send_to(&data, dest);
        }
    }

    fn recv_from(&mut self, now: Instant, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        if self.plain() {
            return self.socket.recv_from(buf);
        }
        let mut tmp = [0u8; 4096];
        loop {
            match self.socket.recv_from(&mut tmp) {
                Ok((len, from)) => {
                    if !self.drop_roll() {
                        self.in_q
                            .push_back((now + self.delay, from, tmp[..len].to_vec()));
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => return Err(e),
            }
        }
        if self.in_q.front().is_some_and(|(due, ..)| *due <= now) {
            let (_, from, data) = self.in_q.pop_front().unwrap();
            let n = data.len().min(buf.len());
            buf[..n].copy_from_slice(&data[..n]);
            return Ok((n, from));
        }
        Err(io::ErrorKind::WouldBlock.into())
    }

    #[cfg(test)] // test seam
    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.socket.local_addr()
    }
}

// --- server -----------------------------------------------------------------

struct ConnState {
    conn: Connection,
    /// Peer id, ASSIGNED AT ADMISSION (0 = not yet admitted — real ids
    /// start at 1). Fresh sessions draw from `next_peer`; a reconnecting
    /// session token reclaims the id it held.
    peer: u8,
    connected: bool,
    gone: bool,
    stream: Option<StreamId>,
    stream_in: Vec<u8>,
    stream_out: Vec<u8>,
    last_input_seq: u32,
    /// ADMITTED: past the session/password door. Until this is true the
    /// peer is never `joined`, and its inputs/events/acks are dropped
    /// unread.
    authed: bool,
    /// When the connection was accepted — the auth-timeout clock.
    since: Instant,
    /// The session token FRAME_AUTH presented (zeros until then; an
    /// all-zero token never parks or reclaims).
    token: [u8; 16],
    /// Latest view pose this peer reported (see [`DGRAM_VIEW`]).
    view: Option<[f32; 6]>,
}

/// Server endpoint. Pump from a net task each tick; drain joins/leaves/acks
/// into a `NetServer`, send its snapshots back out.
pub struct QuicServer {
    socket: LagSocket,
    endpoint: Endpoint,
    conns: HashMap<ConnectionHandle, ConnState>,
    peer_conns: HashMap<u8, ConnectionHandle>,
    next_peer: u8,
    schema: Vec<u8>,
    /// Session password (see [`FRAME_AUTH`]); `None` = open server.
    password: Option<String>,
    joined: Vec<u8>,
    left: Vec<u8>,
    acks: Vec<(u8, u32, u32)>,
    inputs: Vec<(u8, u32, Vec<u8>)>,
    events: Vec<(u8, u16, Vec<u8>)>,
    /// Snapshots dropped for exceeding the datagram size (see README).
    pub oversize_drops: u32,
    /// Recently-departed sessions: token → (peer id, when they left).
    /// A reconnect presenting a parked token inside `grace` gets the
    /// same peer id back — with fresh net state (all delta cursors at
    /// zero), because full reconvergence IS the baseline mechanism.
    parked: HashMap<[u8; 16], (u8, Instant)>,
    /// How long a parked session may reclaim its id.
    grace: Duration,
    /// Admissions resolved after the pump's event loop (identity needs
    /// `&mut` access across connections for the supersede scan).
    pending_admit: Vec<ConnectionHandle>,
}

impl QuicServer {
    pub fn bind(addr: &str, schema: &[(u8, String, usize, u64)]) -> io::Result<Self> {
        let socket = UdpSocket::bind(addr)?;
        socket.set_nonblocking(true)?;

        let cert =
            rcgen::generate_simple_self_signed(vec!["pm".into()]).map_err(io::Error::other)?;
        let cert_der = cert.cert.der().clone();
        let key = rustls::pki_types::PrivatePkcs8KeyDer::from(cert.signing_key.serialize_der());
        let mut crypto = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![cert_der], key.into())
            .map_err(io::Error::other)?;
        crypto.alpn_protocols = vec![ALPN.to_vec()];
        let quic_crypto = QuicServerConfig::try_from(crypto).map_err(io::Error::other)?;
        let mut server_config = ServerConfig::with_crypto(Arc::new(quic_crypto));
        server_config.transport_config(transport_config());

        let endpoint = Endpoint::new(
            Arc::new(EndpointConfig::default()),
            Some(Arc::new(server_config)),
            true,
            None,
        );
        Ok(Self {
            socket: LagSocket::new(socket),
            endpoint,
            conns: HashMap::new(),
            peer_conns: HashMap::new(),
            next_peer: 1,
            schema: schema_encode(schema),
            password: None,
            joined: Vec::new(),
            left: Vec::new(),
            acks: Vec::new(),
            inputs: Vec::new(),
            events: Vec::new(),
            oversize_drops: 0,
            parked: HashMap::new(),
            grace: RECONNECT_GRACE,
            pending_admit: Vec::new(),
        })
    }

    /// Require this session password from every client (before any
    /// client connects — set it right after `bind`).
    pub fn password_set(&mut self, pw: &str) {
        self.password = Some(pw.to_string());
    }

    /// How long after a disconnect the session's token may reclaim its
    /// peer id (default [`RECONNECT_GRACE`]). Keep the game's avatar
    /// parking window in step with this.
    pub fn reconnect_grace_set(&mut self, grace: Duration) {
        self.grace = grace;
    }

    #[cfg(test)] // test seam
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.socket.local_addr()
    }

    // No server-side link_lag_set: the CLIENT lags the link (one lagged
    // socket per path — see netmod::serve for the stacking footgun).

    /// Drive the endpoint: ingest UDP, advance handshakes/timers, collect
    /// acks and events, flush outgoing packets. Call once per tick.
    pub fn pump(&mut self) {
        let now = Instant::now();
        self.socket.flush(now);

        socket_ingest(
            &mut self.socket,
            &mut self.endpoint,
            now,
            |socket, endpoint, ev, out| match ev {
                DatagramEvent::NewConnection(incoming) => {
                    out.clear();
                    match endpoint.accept(incoming, now, out, None) {
                        Ok((ch, conn)) => {
                            // No peer id yet: identity is decided at
                            // ADMISSION, where a reconnect token can
                            // reclaim the id it held (and a bounced
                            // dial burns nothing).
                            self.conns.insert(
                                ch,
                                ConnState {
                                    conn,
                                    peer: 0,
                                    connected: false,
                                    gone: false,
                                    stream: None,
                                    stream_in: Vec::new(),
                                    stream_out: Vec::new(),
                                    last_input_seq: 0,
                                    authed: false,
                                    since: now,
                                    token: [0; 16],
                                    view: None,
                                },
                            );
                        }
                        Err(err) => {
                            if let Some(t) = err.response {
                                socket.send_to(now, &out[..t.size], t.destination);
                            }
                        }
                    }
                }
                DatagramEvent::ConnectionEvent(ch, ev) => {
                    if let Some(st) = self.conns.get_mut(&ch) {
                        st.conn.handle_event(ev);
                    }
                }
                DatagramEvent::Response(response) => {
                    socket.send_to(now, &out[..response.size], response.destination);
                }
            },
        );

        let mut drained = Vec::new();
        for (&ch, st) in self.conns.iter_mut() {
            conn_tick(&mut st.conn, now);
            while let Some(ev) = st.conn.poll() {
                match ev {
                    Event::Connected => {
                        st.connected = true;
                        if let Some(id) = st.conn.streams().open(Dir::Bi) {
                            st.stream = Some(id);
                            // The hello (schema only) goes out before
                            // auth — the schema is a contract, not a
                            // secret, and the client needs it to know
                            // the stream to answer on. Identity waits
                            // for FRAME_AUTH's session token; ADMISSION
                            // (and the peer id) arrive in FRAME_WELCOME.
                            frame_write(&mut st.stream_out, FRAME_HELLO, &self.schema);
                        }
                    }
                    Event::ConnectionLost { .. } => {
                        st.gone = true;
                        // Only ADMITTED peers ever reached NetServer, so
                        // only they get a leave — a bounced password
                        // guess never touches the roster. A real session
                        // parks: its token may reclaim this peer id
                        // inside the grace window.
                        if st.authed {
                            self.left.push(st.peer);
                            if st.token != [0; 16] {
                                self.parked.insert(st.token, (st.peer, now));
                            }
                        }
                    }
                    Event::Stream(StreamEvent::Readable { id }) => {
                        stream_read(&mut st.conn, id, &mut st.stream_in);
                    }
                    _ => {}
                }
            }
            while let Some(d) = st.conn.datagrams().recv() {
                if !st.authed {
                    continue; // drained but dropped: no play before the door
                }
                match d.first() {
                    Some(&DGRAM_ACK) if d.len() >= 9 => {
                        self.acks.push((
                            st.peer,
                            u32::from_le_bytes(d[1..5].try_into().unwrap()),
                            u32::from_le_bytes(d[5..9].try_into().unwrap()),
                        ));
                    }
                    Some(&DGRAM_INPUT) => {
                        inputs_parse(&d[1..], st.peer, &mut st.last_input_seq, &mut self.inputs);
                    }
                    Some(&DGRAM_VIEW) if d.len() >= 25 => {
                        let mut pose = [0f32; 6];
                        for (k, v) in pose.iter_mut().enumerate() {
                            *v = f32::from_le_bytes(d[1 + k * 4..5 + k * 4].try_into().unwrap());
                        }
                        st.view = Some(pose);
                    }
                    _ => {}
                }
            }
            for (ty, payload) in frames_parse(&mut st.stream_in) {
                if ty == FRAME_AUTH && !st.authed && !st.gone {
                    // The one frame an unadmitted peer may speak:
                    // `[token; 16][password]`. Password mismatch →
                    // close now (the client sees the reason string);
                    // otherwise identity resolves in the admission
                    // phase below (it needs to look across
                    // connections). Constant-time compare is overkill
                    // for a lobby bouncer; QUIC already encrypted it.
                    if payload.len() < 16 {
                        st.conn.close(now, 2u32.into(), b"bad session frame"[..].into());
                    } else if self
                        .password
                        .as_ref()
                        .is_some_and(|pw| pw.as_bytes() != &payload[16..])
                    {
                        st.conn.close(now, 2u32.into(), b"bad password"[..].into());
                    } else {
                        st.token = payload[..16].try_into().unwrap();
                        self.pending_admit.push(ch);
                    }
                } else if ty >= EVENT_USER_BASE && st.authed {
                    self.events.push((st.peer, ty, payload));
                }
            }
            // Never answered the door: drop the connection so half-open
            // dials can't accumulate.
            if !st.authed && !st.gone && now.duration_since(st.since) > AUTH_TIMEOUT {
                st.conn.close(now, 2u32.into(), b"auth timeout"[..].into());
            }
        }

        // ADMISSION: decide who this connection IS, with full access
        // across connections. Priority: a live connection already
        // holding the token (the same player redialing before the drop
        // was noticed — supersede it QUIETLY, no `left`, ownership
        // stands), then a parked token inside the grace window (normal
        // reconnect — same peer id back, fresh delta cursors, and full
        // reconvergence IS the baseline mechanism), else a fresh id.
        let pending = std::mem::take(&mut self.pending_admit);
        for ch in pending {
            let Some(st) = self.conns.get(&ch) else {
                continue;
            };
            if st.gone || st.authed {
                continue;
            }
            let token = st.token;
            self.parked
                .retain(|_, &mut (_, since)| now.duration_since(since) <= self.grace);
            let live = (token != [0; 16])
                .then(|| {
                    self.conns.iter().find_map(|(&c, s)| {
                        (c != ch && s.authed && !s.gone && s.token == token)
                            .then_some((c, s.peer))
                    })
                })
                .flatten();
            let peer = if let Some((old_ch, old_peer)) = live {
                let old = self.conns.get_mut(&old_ch).unwrap();
                old.authed = false; // quiet: no left, the id lives on
                old.conn
                    .close(now, 3u32.into(), b"superseded by reconnect"[..].into());
                self.peer_conns.remove(&old_peer);
                old_peer
            } else if let Some((peer, _)) =
                (token != [0; 16]).then(|| self.parked.remove(&token)).flatten()
            {
                peer
            } else {
                // Fresh ids are never reused; u8 saturation refuses new
                // peers rather than colliding — and 255 is RESERVED for
                // the server's recorder (a virtual peer, never on the
                // wire; see `NetServer::record_to`).
                if self.next_peer >= u8::MAX {
                    let st = self.conns.get_mut(&ch).unwrap();
                    st.conn.close(now, 2u32.into(), b"server full"[..].into());
                    continue;
                }
                let peer = self.next_peer;
                self.next_peer = self.next_peer.saturating_add(1);
                peer
            };
            let st = self.conns.get_mut(&ch).unwrap();
            st.peer = peer;
            st.authed = true;
            frame_write(&mut st.stream_out, FRAME_WELCOME, &[peer]);
            self.peer_conns.insert(peer, ch);
            self.joined.push(peer);
        }

        for (&ch, st) in self.conns.iter_mut() {
            conn_flush(
                &mut st.conn,
                &mut self.endpoint,
                ch,
                st.stream,
                &mut st.stream_out,
                &mut self.socket,
                now,
            );
            if st.gone && st.conn.is_drained() {
                drained.push(ch);
            }
        }
        for ch in drained {
            if let Some(st) = self.conns.remove(&ch) {
                // Only clear the peer mapping if it still points HERE —
                // a superseded connection's peer id now belongs to the
                // reconnected one.
                if self.peer_conns.get(&st.peer) == Some(&ch) {
                    self.peer_conns.remove(&st.peer);
                }
            }
        }
    }

    /// Peers whose QUIC handshake completed since the last drain — feed to
    /// `NetServer::peer_add`.
    pub fn joined_drain(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.joined)
    }

    /// Disconnected peers — feed to `NetServer::peer_remove` so a dead
    /// peer's stale ack can't block id recycling.
    pub fn left_drain(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.left)
    }

    /// Acks received since the last drain: (peer, tick, send seq) — feed
    /// each to `NetServer::ack`.
    pub fn acks_drain(&mut self) -> Vec<(u8, u32, u32)> {
        std::mem::take(&mut self.acks)
    }

    /// Inputs received since the last drain: (peer, sequence, payload),
    /// in-order per peer, gap-tolerant. Pass each consumed sequence to
    /// `NetServer::input_processed` so snapshots echo it back.
    pub fn inputs_drain(&mut self) -> Vec<(u8, u32, Vec<u8>)> {
        std::mem::take(&mut self.inputs)
    }

    /// Reliable client→server events received since the last drain:
    /// (peer, type, payload). The net task feeds these to the typed
    /// `EventRx` channels.
    pub fn events_drain(&mut self) -> Vec<(u8, u16, Vec<u8>)> {
        std::mem::take(&mut self.events)
    }

    /// `peer`'s latest reported view pose (`[eye xyz, forward xyz]`),
    /// `None` for peers that never report one (bots, HUD-less tools).
    pub fn view(&self, peer: u8) -> Option<[f32; 6]> {
        self.peer_conns
            .get(&peer)
            .and_then(|ch| self.conns.get(ch))
            .and_then(|st| st.view)
    }

    /// Round-trip time to `peer` as quinn currently estimates it
    /// (`Duration::ZERO` for an unknown peer). The server-side sibling of
    /// `QuicClient::rtt`.
    pub fn rtt(&self, peer: u8) -> Duration {
        self.peer_conns
            .get(&peer)
            .and_then(|ch| self.conns.get(ch))
            .map_or(Duration::ZERO, |st| st.conn.rtt())
    }

    /// How many snapshot bytes fit in one datagram to `peer` right now
    /// (~1200 until MTU discovery raises it). Feed this to
    /// `NetServer::snapshot_budgeted` so snapshots never oversize.
    pub fn snapshot_budget(&mut self, peer: u8) -> usize {
        self.peer_conns
            .get(&peer)
            .and_then(|ch| self.conns.get_mut(ch))
            .and_then(|st| st.conn.datagrams().max_size())
            .map(|m| m.saturating_sub(1)) // DGRAM_SNAPSHOT type byte
            .unwrap_or(1100)
    }

    /// Send a snapshot as an unreliable datagram. Oversize snapshots are
    /// dropped and counted — keep synced state per snapshot under the
    /// datagram limit (~1200 bytes until MTU discovery raises it).
    pub fn snapshot_send(&mut self, peer: u8, snapshot: &[u8]) {
        let Some(st) = self
            .peer_conns
            .get(&peer)
            .and_then(|ch| self.conns.get_mut(ch))
        else {
            return;
        };
        if !st.connected || st.gone {
            return;
        }
        let max = st.conn.datagrams().max_size().unwrap_or(0);
        if snapshot.len() + 1 > max {
            self.oversize_drops += 1;
            return;
        }
        let mut d = Vec::with_capacity(snapshot.len() + 1);
        d.push(DGRAM_SNAPSHOT);
        d.extend_from_slice(snapshot);
        let _ = st.conn.datagrams().send(d.into(), true);
    }

    /// Remaining space in `peer`'s outgoing datagram buffer — the net
    /// doctor's backlog gauge (a shrinking value means snapshots are
    /// queueing behind pacing/congestion instead of hitting the wire).
    pub fn dgram_space(&mut self, peer: u8) -> usize {
        self.peer_conns
            .get(&peer)
            .and_then(|ch| self.conns.get_mut(ch))
            .map_or(0, |st| st.conn.datagrams().send_buffer_space())
    }
}

// --- client ------------------------------------------------------------------

#[derive(Debug)]
struct AcceptAnyCert(Arc<rustls::crypto::CryptoProvider>);

impl rustls::client::danger::ServerCertVerifier for AcceptAnyCert {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

/// Client endpoint. Pump from a net task each tick; apply drained
/// snapshots through a `NetClient` and send the resulting acks back.
pub struct QuicClient {
    socket: LagSocket,
    endpoint: Endpoint,
    ch: ConnectionHandle,
    conn: Connection,
    connected: bool,
    gone: bool,
    stream: Option<StreamId>,
    stream_in: Vec<u8>,
    stream_out: Vec<u8>,
    schema: Vec<u8>,
    /// Session password to present (empty = none). Rides FRAME_AUTH
    /// after a schema-ok hello — the server decides whether it matters.
    password: Option<String>,
    /// Session token FRAME_AUTH leads with — the reconnect identity.
    /// Random per client by default; a game that redials passes the
    /// SAME token again ([`session_token_set`](QuicClient::session_token_set))
    /// to reclaim its peer id and parked avatar.
    token: [u8; 16],
    peer: Option<u8>,
    snapshots: Vec<Vec<u8>>,
    error: Option<String>,
    input_seq: u32,
    input_buf: VecDeque<(u32, Vec<u8>)>,
}

impl QuicClient {
    pub fn connect(addr: &str, schema: &[(u8, String, usize, u64)]) -> io::Result<Self> {
        let server: SocketAddr = addr.parse().map_err(io::Error::other)?;
        let bind = if server.is_ipv4() {
            "0.0.0.0:0"
        } else {
            "[::]:0"
        };
        let socket = UdpSocket::bind(bind)?;
        socket.set_nonblocking(true)?;

        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let mut crypto = rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(AcceptAnyCert(provider)))
            .with_no_client_auth();
        crypto.alpn_protocols = vec![ALPN.to_vec()];
        let quic_crypto = QuicClientConfig::try_from(crypto).map_err(io::Error::other)?;
        let mut config = ClientConfig::new(Arc::new(quic_crypto));
        config.transport_config(transport_config());

        let mut endpoint = Endpoint::new(Arc::new(EndpointConfig::default()), None, true, None);
        let (ch, conn) = endpoint
            .connect(Instant::now(), config, server, "pm")
            .map_err(io::Error::other)?;
        Ok(Self {
            socket: LagSocket::new(socket),
            endpoint,
            ch,
            conn,
            connected: false,
            gone: false,
            stream: None,
            stream_in: Vec::new(),
            stream_out: Vec::new(),
            schema: schema_encode(schema),
            password: None,
            token: token_random(),
            peer: None,
            snapshots: Vec::new(),
            error: None,
            input_seq: 0,
            input_buf: VecDeque::new(),
        })
    }

    /// Present this session password when the hello arrives (set before
    /// the first `pump`).
    pub fn password_set(&mut self, pw: &str) {
        self.password = Some(pw.to_string());
    }

    /// Use this session token instead of the random default (set before
    /// the first `pump`). A redial presenting the token of a session
    /// inside the server's reconnect grace gets its old peer id back —
    /// generate one token per GAME session and reuse it across dials.
    pub fn session_token_set(&mut self, token: [u8; 16]) {
        self.token = token;
    }

    /// Drive the connection. Call once per tick.
    pub fn pump(&mut self) {
        let now = Instant::now();
        self.socket.flush(now);

        socket_ingest(
            &mut self.socket,
            &mut self.endpoint,
            now,
            |socket, _endpoint, ev, out| match ev {
                DatagramEvent::ConnectionEvent(_, ev) => self.conn.handle_event(ev),
                DatagramEvent::Response(response) => {
                    socket.send_to(now, &out[..response.size], response.destination);
                }
                _ => {}
            },
        );

        conn_tick(&mut self.conn, now);
        while let Some(ev) = self.conn.poll() {
            match ev {
                Event::Connected => self.connected = true,
                Event::ConnectionLost { reason } => {
                    self.gone = true;
                    self.error.get_or_insert(reason.to_string());
                }
                Event::Stream(StreamEvent::Opened { dir: Dir::Bi }) if self.stream.is_none() => {
                    self.stream = self.conn.streams().accept(Dir::Bi);
                    // Data that arrived together with the stream-open
                    // emits no Readable event — read it now or the
                    // hello sits buffered until more traffic arrives.
                    if let Some(id) = self.stream {
                        stream_read(&mut self.conn, id, &mut self.stream_in);
                    }
                }
                Event::Stream(StreamEvent::Readable { id }) => {
                    stream_read(&mut self.conn, id, &mut self.stream_in);
                }
                _ => {}
            }
        }
        while let Some(d) = self.conn.datagrams().recv() {
            if !d.is_empty() && d[0] == DGRAM_SNAPSHOT {
                self.snapshots.push(d[1..].to_vec());
            }
        }
        // The reliable stream only carries the handshake downstream
        // (hello, then welcome): events are one-way client→server;
        // server→client facts are state.
        for (ty, payload) in frames_parse(&mut self.stream_in) {
            if ty == FRAME_HELLO {
                if payload != self.schema[..] {
                    // Diagnose per channel — the compare stays the byte
                    // equality above, the diff only explains it.
                    let diff = match (schema_decode(&payload), schema_decode(&self.schema)) {
                        (Some(srv), Some(cli)) => schema_diff(&srv, &cli),
                        _ => "undecodable schema".into(),
                    };
                    self.error = Some(format!("schema mismatch with server: {diff}"));
                    self.conn
                        .close(now, 1u32.into(), b"schema mismatch"[..].into());
                    continue;
                }
                // Answer the door: session token + password (or an
                // empty knock) — an open server ignores the password
                // part, a locked one gates ADMISSION on it; the token
                // is who we ARE if we've been here before.
                let mut auth = self.token.to_vec();
                auth.extend_from_slice(self.password.as_deref().unwrap_or("").as_bytes());
                frame_write(&mut self.stream_out, FRAME_AUTH, &auth);
            } else if ty == FRAME_WELCOME && !payload.is_empty() {
                self.peer = Some(payload[0]);
            }
        }
        conn_flush(
            &mut self.conn,
            &mut self.endpoint,
            self.ch,
            self.stream,
            &mut self.stream_out,
            &mut self.socket,
            now,
        );
    }

    /// Some(peer id) once the hello arrived and the schema matched. Assign
    /// it to `pm.local_peer` before spawning any local entities.
    pub fn handshake_done(&self) -> Option<u8> {
        self.peer
    }

    pub fn is_gone(&self) -> bool {
        self.gone
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    pub fn rtt(&self) -> Duration {
        self.conn.rtt()
    }

    /// Simulate link conditions: one-way `delay` and packet `loss` (0..1)
    /// applied in both directions. RTT rises by ~2x delay.
    pub fn link_lag_set(&mut self, delay: Duration, loss: f32) {
        self.socket.delay = delay;
        self.socket.loss = loss.clamp(0.0, 1.0);
    }

    pub fn snapshots_drain(&mut self) -> Vec<Vec<u8>> {
        std::mem::take(&mut self.snapshots)
    }

    /// Ack a snapshot's (tick, send seq) as an unreliable datagram (loss
    /// just means the server declares that send lost off a later ack and
    /// resends — idempotent upserts make the redundancy harmless).
    pub fn ack_send(&mut self, tick: u32, seq: u32) {
        let mut d = Vec::with_capacity(9);
        d.push(DGRAM_ACK);
        d.extend_from_slice(&tick.to_le_bytes());
        d.extend_from_slice(&seq.to_le_bytes());
        let _ = self.conn.datagrams().send(d.into(), true);
    }

    /// Send a control input as an unreliable datagram. Each call advances
    /// the input sequence; the last few inputs ride along redundantly so
    /// the server sees an in-order series despite loss, no retransmits.
    /// Returns the assigned sequence (compare against
    /// `Applied::input_seq` for reconciliation).
    pub fn input_send(&mut self, payload: &[u8]) -> u32 {
        self.input_seq += 1;
        self.input_buf.push_back((self.input_seq, payload.to_vec()));
        if self.input_buf.len() > INPUT_REDUNDANCY {
            self.input_buf.pop_front();
        }
        let mut d = vec![DGRAM_INPUT, self.input_buf.len() as u8];
        for (seq, bytes) in &self.input_buf {
            d.extend_from_slice(&seq.to_le_bytes());
            d.extend_from_slice(&(bytes.len() as u16).to_le_bytes());
            d.extend_from_slice(bytes);
        }
        let _ = self.conn.datagrams().send(d.into(), true);
        self.input_seq
    }

    /// Report the view pose (`[eye xyz, forward xyz]`) as an unreliable
    /// newest-wins datagram — see [`DGRAM_VIEW`]. Call at the input
    /// cadence; loss costs nothing.
    pub fn view_send(&mut self, pose: [f32; 6]) {
        let mut d = Vec::with_capacity(25);
        d.push(DGRAM_VIEW);
        for v in pose {
            d.extend_from_slice(&v.to_le_bytes());
        }
        let _ = self.conn.datagrams().send(d.into(), true);
    }

    /// Send a typed event on the reliable ordered stream (`ty` must be
    /// >= EVENT_USER_BASE).
    pub fn event_send(&mut self, ty: u16, payload: &[u8]) {
        debug_assert!(ty >= EVENT_USER_BASE);
        frame_write(&mut self.stream_out, ty, payload);
    }
}

// --- lag-sim A/B -------------------------------------------------------------

/// The transport-vs-sim discriminator, run explicitly with
///
/// ```text
/// cargo test -p pm --release -- --ignored lag_ab --nocapture
/// ```
///
/// Three legs over the same simulated link (one lagged socket: 80 ms
/// one-way, 3% loss — what `PM_LAG_MS=80 PM_LOSS=0.03` now means) at the
/// same once-per-tick 60 Hz pump cadence the engine uses:
///
/// 1. raw UDP echo straight through a `LagSocket` — the floor the sim +
///    pump cadence impose, no QUIC anywhere;
/// 2. the full QUIC transport as shipped (BBR);
/// 3. the full QUIC transport with `PM_CC=cubic-big` (pacer defused).
///
/// If (2) sits near (1) + a few pump quanta, QUIC is innocent and a raw
/// side-transport would buy nothing. If (2) is far above (1) and (3)
/// collapses back, the amplifier is quinn's pacer against the per-tick
/// pump. Run alone: leg 3 flips the process-global `PM_CC`.
#[cfg(test)]
mod lag_ab {
    use super::*;

    const DELAY_MS: u64 = 80;
    const LOSS: f32 = 0.03;
    const TICK: Duration = Duration::from_micros(16_667);
    const SCHEMA: (u8, &str, usize, u64) = (0, "pos", 8, 0);

    fn schema() -> Vec<(u8, String, usize, u64)> {
        vec![(SCHEMA.0, SCHEMA.1.into(), SCHEMA.2, SCHEMA.3)]
    }

    fn stats(label: &str, mut ms: Vec<f32>) {
        assert!(!ms.is_empty(), "{label}: no RTT samples survived");
        ms.sort_by(f32::total_cmp);
        let pick = |q: f32| ms[((ms.len() - 1) as f32 * q) as usize];
        eprintln!(
            "[lag_ab] {label:>10}: p50 {:6.1} ms  p95 {:6.1} ms  max {:6.1} ms  (n={})",
            pick(0.5),
            pick(0.95),
            ms[ms.len() - 1],
            ms.len(),
        );
    }

    /// Sleep to the loop's next 60 Hz edge and return a fresh `now`.
    fn tick_edge(t0: Instant, n: u32) -> Instant {
        let target = t0 + TICK * n;
        let now = Instant::now();
        if target > now {
            std::thread::sleep(target - now);
        }
        Instant::now()
    }

    /// Leg 1: stamped pings from a lagged socket to a plain echo peer,
    /// both pumped at tick cadence — measures sim + cadence alone.
    #[test]
    #[ignore]
    fn lag_ab_raw_udp() {
        let bind = || {
            let s = UdpSocket::bind("127.0.0.1:0").unwrap();
            s.set_nonblocking(true).unwrap();
            s
        };
        let mut client = LagSocket::new(bind());
        client.delay = Duration::from_millis(DELAY_MS);
        client.loss = LOSS;
        let mut server = LagSocket::new(bind()); // stays plain()
        let server_addr = server.local_addr().unwrap();

        let t0 = Instant::now();
        let mut rtts = Vec::new();
        let mut buf = [0u8; 2048];
        for n in 1..=(6 * 60) {
            let now = tick_edge(t0, n);
            // client: release due sends, harvest echoes, fire this tick's pings
            client.flush(now);
            while let Ok((len, _)) = client.recv_from(now, &mut buf) {
                if len >= 8 {
                    let sent = u64::from_le_bytes(buf[..8].try_into().unwrap());
                    rtts.push(((now - t0).as_micros() as u64 - sent) as f32 / 1000.0);
                }
            }
            let mut ping = [0u8; 1200];
            ping[..8].copy_from_slice(&((now - t0).as_micros() as u64).to_le_bytes());
            client.send_to(now, &ping, server_addr);
            client.send_to(now, &ping, server_addr);
            // server: echo the stamp back, plain passthrough
            server.flush(now);
            while let Ok((len, from)) = server.recv_from(now, &mut buf) {
                if len >= 8 {
                    server.send_to(now, &buf[..8], from);
                }
            }
        }
        stats("raw udp", rtts);
    }

    /// One QUIC leg: handshake, then game-shaped traffic (snapshot down,
    /// input + ack up, every tick) with the client socket lagged; sample
    /// quinn's smoothed RTT each tick after warmup.
    fn quic_leg(label: &str) {
        let mut server = QuicServer::bind("127.0.0.1:0", &schema()).unwrap();
        let addr = server.local_addr().unwrap().to_string();
        let mut client = QuicClient::connect(&addr, &schema()).unwrap();
        client.link_lag_set(Duration::from_millis(DELAY_MS), LOSS);

        let t0 = Instant::now();
        let mut rtts = Vec::new();
        let mut peers: Vec<u8> = Vec::new();
        let snapshot = [7u8; 600];
        for n in 1..=(8 * 60) {
            let _now = tick_edge(t0, n);
            server.pump();
            peers.extend(server.joined_drain());
            server.acks_drain();
            server.inputs_drain();
            for &p in &peers {
                server.snapshot_send(p, &snapshot);
            }
            client.pump();
            assert!(client.error().is_none(), "client error: {:?}", client.error());
            if client.handshake_done().is_some() {
                client.input_send(&[3u8; 16]);
                let got = client.snapshots_drain().len() as u32;
                if got > 0 {
                    client.ack_send(n, n);
                }
                // 2 s warmup: skip handshake transients and sRTT settling.
                if t0.elapsed() > Duration::from_secs(2) {
                    rtts.push(client.rtt().as_secs_f32() * 1000.0);
                }
            }
        }
        assert!(!peers.is_empty(), "{label}: handshake never completed");
        stats(label, rtts);
    }

    #[test]
    #[ignore]
    fn lag_ab_quic() {
        quic_leg("quic bbr");
    }
}
