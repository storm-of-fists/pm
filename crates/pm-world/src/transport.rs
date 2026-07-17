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
// (pm/2: snapshots carry a per-send seq, acks echo (tick, seq).)
const ALPN: &[u8] = b"pm/2";
const FRAME_HELLO: u16 = 0;
/// User event types must be >= this; lower values are protocol-reserved.
pub const EVENT_USER_BASE: u16 = 16;
const DGRAM_SNAPSHOT: u8 = 0;
const DGRAM_ACK: u8 = 1;
const DGRAM_INPUT: u8 = 2;
/// How many recent inputs ride along in every input datagram. Up to 7
/// consecutive lost packets cost nothing; beyond that the gap is skipped
/// (input is ephemeral — newest wins).
const INPUT_REDUNDANCY: usize = 8;

// --- small helpers --------------------------------------------------------

fn schema_encode(schema: &[(u8, String, usize)]) -> Vec<u8> {
    // Sort by name so the handshake compare is registration-order
    // independent — both ends agree as long as the *set* of (kind, name,
    // size) entries matches, regardless of registration order. The kind
    // byte keeps a pool and an event channel that share a name from
    // silently passing as each other.
    let mut schema = schema.to_vec();
    schema.sort_by(|a, b| a.1.cmp(&b.1));
    let mut out = Vec::new();
    out.extend_from_slice(&(schema.len() as u16).to_le_bytes());
    for (kind, name, size) in &schema {
        out.push(*kind);
        out.extend_from_slice(&(name.len() as u16).to_le_bytes());
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(&(*size as u32).to_le_bytes());
    }
    out
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
    // PM_CC=cubic-big swaps in Cubic with a 16 MiB initial window — an
    // experiment knob, not a mode: quinn's pacer meters packets out at
    // cwnd/sRTT, and our once-per-tick pump turns every pacer hold into
    // a full 16.7 ms stall (see the lag-sim FIXME below). A huge window
    // makes the pacer's budget effectively infinite, which isolates
    // whether pacer-vs-pump is the latency amplifier.
    match std::env::var("PM_CC").as_deref() {
        Ok("cubic-big") => {
            let mut cc = quinn_proto::congestion::CubicConfig::default();
            cc.initial_window(16 * 1024 * 1024);
            tc.congestion_controller_factory(Arc::new(cc));
            eprintln!("[pm net] PM_CC=cubic-big: Cubic, 16 MiB initial window (pacer defused)");
        }
        _ => {
            tc.congestion_controller_factory(Arc::new(
                quinn_proto::congestion::BbrConfig::default(),
            ));
        }
    }
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
    peer: u8,
    connected: bool,
    gone: bool,
    stream: Option<StreamId>,
    stream_in: Vec<u8>,
    stream_out: Vec<u8>,
    last_input_seq: u32,
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
    joined: Vec<u8>,
    left: Vec<u8>,
    acks: Vec<(u8, u32, u32)>,
    inputs: Vec<(u8, u32, Vec<u8>)>,
    events: Vec<(u8, u16, Vec<u8>)>,
    /// Snapshots dropped for exceeding the datagram size (see README).
    pub oversize_drops: u32,
}

impl QuicServer {
    pub fn bind(addr: &str, schema: &[(u8, String, usize)]) -> io::Result<Self> {
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
            joined: Vec::new(),
            left: Vec::new(),
            acks: Vec::new(),
            inputs: Vec::new(),
            events: Vec::new(),
            oversize_drops: 0,
        })
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
                            let peer = self.next_peer;
                            // Peer ids are not reused; u8 wrap aborts
                            // accepting rather than colliding.
                            self.next_peer = self.next_peer.saturating_add(1);
                            self.conns.insert(
                                ch,
                                ConnState {
                                    conn,
                                    peer,
                                    connected: false,
                                    gone: false,
                                    stream: None,
                                    stream_in: Vec::new(),
                                    stream_out: Vec::new(),
                                    last_input_seq: 0,
                                },
                            );
                            self.peer_conns.insert(peer, ch);
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
                            let mut hello = vec![st.peer];
                            hello.extend_from_slice(&self.schema);
                            frame_write(&mut st.stream_out, FRAME_HELLO, &hello);
                            self.joined.push(st.peer);
                        }
                    }
                    Event::ConnectionLost { .. } => {
                        st.gone = true;
                        if st.connected {
                            self.left.push(st.peer);
                        }
                    }
                    Event::Stream(StreamEvent::Readable { id }) => {
                        stream_read(&mut st.conn, id, &mut st.stream_in);
                    }
                    _ => {}
                }
            }
            while let Some(d) = st.conn.datagrams().recv() {
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
                    _ => {}
                }
            }
            for (ty, payload) in frames_parse(&mut st.stream_in) {
                if ty >= EVENT_USER_BASE {
                    self.events.push((st.peer, ty, payload));
                }
            }
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
                self.peer_conns.remove(&st.peer);
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
    peer: Option<u8>,
    snapshots: Vec<Vec<u8>>,
    error: Option<String>,
    input_seq: u32,
    input_buf: VecDeque<(u32, Vec<u8>)>,
}

impl QuicClient {
    pub fn connect(addr: &str, schema: &[(u8, String, usize)]) -> io::Result<Self> {
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
            peer: None,
            snapshots: Vec::new(),
            error: None,
            input_seq: 0,
            input_buf: VecDeque::new(),
        })
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
        // The reliable stream only ever carries the hello downstream:
        // events are one-way client→server; server→client facts are state.
        for (ty, payload) in frames_parse(&mut self.stream_in) {
            if ty == FRAME_HELLO {
                if payload.len() < 1 + self.schema.len() || payload[1..] != self.schema[..] {
                    self.error = Some("schema mismatch with server".into());
                    self.conn
                        .close(now, 1u32.into(), b"schema mismatch"[..].into());
                    continue;
                }
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
    const SCHEMA: (u8, &str, usize) = (0, "pos", 8);

    fn schema() -> Vec<(u8, String, usize)> {
        vec![(SCHEMA.0, SCHEMA.1.into(), SCHEMA.2)]
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

    /// Legs 2 + 3 in one test (they must not run concurrently: the CC
    /// selection rides the process-global `PM_CC`).
    #[test]
    #[ignore]
    fn lag_ab_quic() {
        quic_leg("quic bbr");
        // SAFETY: single-threaded at this point in the test; the var is
        // read once per endpoint construction inside quic_leg.
        unsafe { std::env::set_var("PM_CC", "cubic-big") };
        quic_leg("quic cubic");
        unsafe { std::env::remove_var("PM_CC") };
    }
}
