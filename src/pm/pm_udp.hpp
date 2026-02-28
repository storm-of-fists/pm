// pm_udp.hpp — UDP networking + sync transport
//
// UdpSocket: Cross-platform non-blocking UDP (POSIX / Winsock2)
//
// NetSys is a plain state struct managing:
//   - Peer management (connect/disconnect, peer slots, peer ranges)
//   - Heartbeat + timeout (auto-disconnect dead peers)
//   - Per-pool sync tracking: change-tracked (sparse synced[] + pending list)
//   - Per-peer outbox with pre-serialized entries (no scanning in flush)
//   - MTU-aware packet splitting
//   - Reliable removals with send-count expiry
//   - Reliable message channel (resend until acked, dedup on recv)
//   - Ordered custom packet helpers (per-peer sequence tracking)
//   - Clock sync (server_time in PktSync, client estimates offset)
//   - Pool bind() / bind_send() / bind_recv() for serialization registration
//   - Interest management with hysteresis
//   - Send rate throttling
//   - Generic recv dispatch by packet type
//
// Usage:
//   auto* net = pm.state_get<NetSys>("net");
//   net->port = 7777;
//   net_init(pm, net);
//
// Depends: pm_core.hpp, platform sockets

#pragma once

// --- Platform sockets ---
#ifdef _WIN32
	#include <winsock2.h>
	#include <ws2tcpip.h>
	#pragma comment(lib, "ws2_32.lib")
	using socket_t = SOCKET;
#else
	#include <sys/socket.h>
	#include <netinet/in.h>
	#include <arpa/inet.h>
	#include <unistd.h>
	#include <fcntl.h>
	#include <errno.h>
	using socket_t = int;
	#ifndef INVALID_SOCKET
		#define INVALID_SOCKET (-1)
	#endif
	#ifndef SOCKET_ERROR
		#define SOCKET_ERROR (-1)
	#endif
#endif

#include "pm_core.hpp"
#include <cstring>
#include <functional>
#include <algorithm>
#include <unordered_map>
#include <vector>
#include <type_traits>

namespace pm {

// =============================================================================
// PeerRange — zero-alloc iterator over set bits in a bitmask
// =============================================================================
struct PeerRange
{
	uint64_t bits;

	struct Iterator
	{
		uint64_t remaining;
		uint8_t operator*() const { return (uint8_t)pm_ctz64(remaining); }
		Iterator &operator++()
		{
			remaining &= remaining - 1;
			return *this;
		}
		bool operator!=(const Iterator &o) const { return remaining != o.remaining; }
	};

	Iterator begin() const { return {bits}; }
	Iterator end() const { return {0}; }
	uint8_t count() const { return (uint8_t)pm_popcnt64(bits); }
	bool empty() const { return bits == 0; }
	bool has(uint8_t id) const { return id < 64 && (bits & (1ULL << id)); }
};

// =============================================================================
// UdpSocket — non-blocking UDP wrapper
// =============================================================================
struct UdpSocket {
	socket_t sock = INVALID_SOCKET;

	void init(int port) {
#ifdef _WIN32
		static bool wsa_init = false;
		if (!wsa_init) {
			WSADATA wd;
			WSAStartup(MAKEWORD(2, 2), &wd);
			wsa_init = true;
		}
#endif
		sock = socket(AF_INET, SOCK_DGRAM, IPPROTO_UDP);
		if (sock == INVALID_SOCKET) return;

#ifdef _WIN32
		u_long mode = 1;
		ioctlsocket(sock, FIONBIO, &mode);
#else
		int flags = fcntl(sock, F_GETFL, 0);
		fcntl(sock, F_SETFL, flags | O_NONBLOCK);
#endif

		int opt = 1;
		setsockopt(sock, SOL_SOCKET, SO_REUSEADDR, (const char*)&opt, sizeof(opt));

		int rcvbuf = 1024 * 1024;
		setsockopt(sock, SOL_SOCKET, SO_RCVBUF, (const char*)&rcvbuf, sizeof(rcvbuf));

		struct sockaddr_in addr{};
		addr.sin_family = AF_INET;
		addr.sin_addr.s_addr = INADDR_ANY;
		addr.sin_port = htons((uint16_t)port);

		if (::bind(sock, (struct sockaddr*)&addr, sizeof(addr)) == SOCKET_ERROR)
			close_sock();
	}

	void send(const void* data, int len, struct sockaddr_in& dest) {
		if (sock == INVALID_SOCKET) return;
		sendto(sock, (const char*)data, len, 0, (struct sockaddr*)&dest, sizeof(dest));
	}

	int recv(uint8_t* buf, int buf_size, struct sockaddr_in& src) {
		if (sock == INVALID_SOCKET) return -1;
		socklen_t slen = sizeof(src);
		int n = recvfrom(sock, (char*)buf, buf_size, 0, (struct sockaddr*)&src, &slen);
		if (n == SOCKET_ERROR) return -1;
		return n;
	}

	void close_sock() {
		if (sock == INVALID_SOCKET) return;
#ifdef _WIN32
		closesocket(sock);
#else
		close(sock);
#endif
		sock = INVALID_SOCKET;
	}
};

// =============================================================================
// Wire format — generic sync protocol
// =============================================================================
#pragma pack(push, 1)
static constexpr uint8_t PKT_SYNC_TYPE    = 0xFE;
static constexpr uint8_t PKT_STATE_SYNC   = 0xF9;
static constexpr uint8_t PKT_CONNECT_REQ  = 0xF8;
static constexpr uint8_t PKT_CONNECT_ACK  = 0xF7;
static constexpr uint8_t PKT_CONNECT_DENY = 0xF6;
static constexpr uint8_t PKT_RELIABLE     = 0xFC;
static constexpr uint8_t PKT_RELIABLE_ACK = 0xFB;
static constexpr uint8_t PKT_HEARTBEAT    = 0xFA;

// Deny reasons
static constexpr uint8_t DENY_VERSION_MISMATCH = 1;
static constexpr uint8_t DENY_SERVER_FULL      = 2;
static constexpr uint8_t DENY_REJECTED         = 3;

// Internal reliable inner types (used by framework, not game code)
static constexpr uint8_t RELIABLE_INNER_REMOVAL = 0xFE;

static constexpr uint16_t MAX_CONNECT_PAYLOAD = 240;

struct PktSync {
	uint8_t type = PKT_SYNC_TYPE;
	uint16_t seq;
	uint32_t frame;
	float server_time;
	uint16_t section_count;
};

struct SectionHeader {
	uint32_t pool_id;
	uint16_t sync_count, entry_size;
};

struct PktStateSync {
	uint8_t type = PKT_STATE_SYNC;
	uint32_t state_id;
	uint16_t size;
	// payload follows
};

struct PktConnectReq {
	uint8_t type = PKT_CONNECT_REQ;
	uint16_t version;
	// payload follows
};

struct PktConnectAck {
	uint8_t type = PKT_CONNECT_ACK;
	uint8_t peer_id;
	// payload follows
};

struct PktConnectDeny {
	uint8_t type = PKT_CONNECT_DENY;
	uint8_t reason;
};

struct PktReliable {
	uint8_t type = PKT_RELIABLE;
	uint16_t msg_id;
	uint8_t inner_type;
	// payload follows
};

struct PktReliableAck {
	uint8_t type = PKT_RELIABLE_ACK;
	uint16_t msg_id;
};

struct PktHeartbeat {
	uint8_t type = PKT_HEARTBEAT;
};
#pragma pack(pop)

static inline bool seq_after(uint16_t a, uint16_t b) { return (int16_t)(a - b) > 0; }

// =============================================================================
// PoolSyncState — Per-pool sync tracking
//
// Change-tracked: sparse synced[] bitmask + pending list.
// Only entities that called notify_change() (or were just added/removed) are
// transmitted. O(dirty) per flush.
// Installed automatically by bind_send().
// =============================================================================
struct PoolSyncState
{
	uint32_t pool_id = 0;

	// --- Change-tracked state ---
	std::vector<uint64_t> synced;
	std::vector<Id> pending;
	std::vector<uint8_t> pending_flag;

	// === Change-tracked mode API ===

	void ensure_size(uint32_t idx)
	{
		if (idx >= synced.size())
			synced.resize(idx + 1, 0);
		if (idx >= pending_flag.size())
			pending_flag.resize(idx + 1, 0);
	}

	void mark_changed(Id id)
	{
		uint32_t idx = id.index();
		ensure_size(idx);
		synced[idx] = 0;
		pending_add(idx, id);
	}

	void mark_synced(uint8_t peer, Id id)
	{
		uint32_t idx = id.index();
		ensure_size(idx);
		synced[idx] |= (1ULL << peer);
	}

	void mark_unsynced_for(uint8_t peer, Id id)
	{
		uint32_t idx = id.index();
		ensure_size(idx);
		synced[idx] &= ~(1ULL << peer);
		pending_add(idx, id);
	}

	bool is_synced_to(uint8_t peer, Id id) const
	{
		uint32_t idx = id.index();
		if (idx >= synced.size()) return false;
		return (synced[idx] & (1ULL << peer)) != 0;
	}

	template <typename T, typename F>
	void each_unsynced(Pool<T>* pool, uint8_t peer, uint64_t remote_mask, F&& fn)
	{
		uint64_t bit = 1ULL << peer;
		size_t w = 0;
		for (size_t r = 0; r < pending.size(); r++)
		{
			Id id = pending[r];
			uint32_t idx = id.index();

			if (!pool->has(id))
			{
				if (idx < pending_flag.size()) pending_flag[idx] = 0;
				continue;
			}

			uint64_t s = (idx < synced.size()) ? synced[idx] : 0;

			if (remote_mask && (s & remote_mask) == remote_mask)
			{
				if (idx < pending_flag.size()) pending_flag[idx] = 0;
				continue;
			}

			pending[w++] = id;
			if (!(s & bit))
			{
				size_t di = pool->sparse_indices[idx];
				fn(id, pool->items[di], di);
			}
		}
		pending.resize(w);
	}

	size_t pending_count() const { return pending.size(); }

	template <typename T>
	void repend_all(Pool<T>* pool)
	{
		pending.clear();
		std::fill(pending_flag.begin(), pending_flag.end(), 0);
		for (size_t i = 0; i < pool->items.size(); i++)
		{
			Id id = pool->id_at(i);
			uint32_t idx = id.index();
			pending_add(idx, id);
		}
	}

	void clear_pending()
	{
		pending.clear();
		std::fill(pending_flag.begin(), pending_flag.end(), 0);
	}

private:
	void pending_add(uint32_t idx, Id id)
	{
		if (idx >= pending_flag.size())
			pending_flag.resize(idx + 1, 0);
		if (!pending_flag[idx])
		{
			pending_flag[idx] = 1;
			pending.push_back(id);
		}
	}
};

// =============================================================================
// NetSys — UDP network sync transport + peer management
//
// Plain state struct. Created via pm.state_get<NetSys>("net").
// Call net_init(pm, net) after configuration to register tasks.
// =============================================================================
struct NetSys {
	NetSys() {
		peers_arr[0].id = 0;
		peers_arr[0].connected = true;
	}

	// --- Configuration (set before net_init) ---
	int port = 0;
	float send_rate = 1.f/30.f;
	int mtu = 1200;
	const char* connect_ip = nullptr;
	float peer_timeout = 5.0f;
	float heartbeat_interval = 1.0f;
	uint16_t protocol_version = 0;

	// --- Timekeeping ---
	float local_time = 0.f;

	// --- Client-side clock sync (derived from PktSync::server_time) ---
	float clock_offset = 0.f;
	uint32_t server_frame = 0;
	float server_time_estimate() const { return local_time + clock_offset; }

	// --- Connection handshake ---
	struct ConnectResult {
		bool accepted = true;
		uint8_t deny_reason = 0;
		uint8_t response[MAX_CONNECT_PAYLOAD]{};
		uint16_t response_size = 0;

		static ConnectResult accept(const void* data = nullptr, uint16_t size = 0) {
			ConnectResult r; r.accepted = true;
			if (data && size > 0) {
				r.response_size = std::min(size, MAX_CONNECT_PAYLOAD);
				memcpy(r.response, data, r.response_size);
			}
			return r;
		}
		static ConnectResult deny(uint8_t reason = DENY_REJECTED) {
			ConnectResult r; r.accepted = false; r.deny_reason = reason;
			return r;
		}
	};

	// Server-side: validator called on incoming connect request.
	// Receives (peer_id that will be assigned, source addr, payload, size).
	// Return ConnectResult::accept() or ConnectResult::deny().
	// If no validator set, all connections are accepted.
	using ConnectValidator = std::function<ConnectResult(uint8_t peer_id, struct sockaddr_in& src,
														 const uint8_t* payload, uint16_t size)>;
	ConnectValidator connect_validator;

	// Client-side: connection state machine
	enum class ConnState : uint8_t { DISCONNECTED, CONNECTING, CONNECTED };
	ConnState conn_state = ConnState::DISCONNECTED;
	float connect_retry_interval = 0.5f;
	float connect_timeout = 10.0f;
	float connect_timer = 0.f;
	float connect_elapsed = 0.f;
	uint8_t connect_payload_buf[MAX_CONNECT_PAYLOAD]{};
	uint16_t connect_payload_size = 0;

	using ConnectedCallback = std::function<void(NetSys&, uint8_t peer_id, const uint8_t* payload, uint16_t size)>;
	using ConnectDeniedCallback = std::function<void(NetSys&, uint8_t reason)>;
	ConnectedCallback connected_callback;
	ConnectDeniedCallback connect_denied_callback;

	void on_connected(ConnectedCallback fn) { connected_callback = std::move(fn); }
	void on_connect_denied(ConnectDeniedCallback fn) { connect_denied_callback = std::move(fn); }

	void request_connect(const char* ip, int target_port, const void* payload = nullptr, uint16_t size = 0) {
		set_client();
		if (sock.sock == INVALID_SOCKET) sock.init(0);
		peer_addrs[0].sin_family = AF_INET;
		peer_addrs[0].sin_addr.s_addr = inet_addr(ip);
		peer_addrs[0].sin_port = htons(target_port);
		has_addr[0] = true;
		connect_payload_size = std::min(size, MAX_CONNECT_PAYLOAD);
		if (payload && size > 0) memcpy(connect_payload_buf, payload, connect_payload_size);
		conn_state = ConnState::CONNECTING;
		connect_timer = 0.f;
		connect_elapsed = 0.f;
		send_connect_req();
	}

	void send_connect_req() {
		uint8_t buf[sizeof(PktConnectReq) + MAX_CONNECT_PAYLOAD];
		PktConnectReq hdr{}; hdr.version = protocol_version;
		memcpy(buf, &hdr, sizeof(hdr));
		if (connect_payload_size > 0) memcpy(buf + sizeof(hdr), connect_payload_buf, connect_payload_size);
		sock.send(buf, sizeof(hdr) + connect_payload_size, peer_addrs[0]);
	}

	// --- Transport ---
	UdpSocket sock;
	struct sockaddr_in peer_addrs[64]{};
	bool has_addr[64]{};

	void send_to(uint8_t p, const void* d, int len) {
		if (has_addr[p]) sock.send(d, len, peer_addrs[p]);
	}
	void broadcast(PeerRange remotes, const void* d, int len) {
		for (uint8_t i : remotes)
			if (has_addr[i]) send_to(i, d, len);
	}

	// --- Peer management ---
	uint8_t self_id = 0;
	bool host_mode = true;
	uint64_t peer_bits = 1;

	struct Peer {
		uint8_t id = 255;
		bool connected = false;
		void *user_data = nullptr;
		// Cached ACK for idempotent re-send on duplicate connect requests
		uint8_t ack_buf[sizeof(PktConnectAck) + MAX_CONNECT_PAYLOAD]{};
		uint16_t ack_size = 0;

		void reset() {
			id = 255;
			connected = false;
			user_data = nullptr;
			ack_size = 0;
		}
	};
	Peer peers_arr[64]{};

	using PeerCallback = std::function<void(NetSys&, uint8_t)>;
	std::vector<PeerCallback> connect_callbacks;
	std::vector<PeerCallback> disconnect_callbacks;

	void on_connect(PeerCallback fn) { connect_callbacks.push_back(std::move(fn)); }
	void on_disconnect(PeerCallback fn) { disconnect_callbacks.push_back(std::move(fn)); }

	uint8_t peer_id() const { return self_id; }
	bool is_host() const { return host_mode; }
	uint64_t peer_slots() const { return peer_bits; }
	uint8_t peer_count() const { return (uint8_t)pm_popcnt64(peer_bits); }

	Peer& peer(uint8_t id) { return peers_arr[id]; }
	const Peer& peer(uint8_t id) const { return peers_arr[id]; }

	PeerRange peers() const { return {peer_bits}; }
	PeerRange remote_peers() const {
		uint64_t self_mask = (self_id < 64) ? (1ULL << self_id) : 0;
		return {peer_bits & ~self_mask};
	}

	// Find the first free peer slot without allocating it.
	uint8_t alloc_peer_slot() const {
		for (uint8_t i = 0; i < 64; i++) {
			if (i == self_id) continue;
			if (!(peer_bits & (1ULL << i))) return i;
		}
		return 255;
	}

	// Activate a peer slot — sets bits, resets state, fires callbacks.
	void activate_peer(uint8_t id) {
		peer_bits |= (1ULL << id);
		peers_arr[id].reset();
		peers_arr[id].id = id;
		peers_arr[id].connected = true;
		for (auto& [pid, rep] : sync_registry)
			rep.repend_fn();
		for (auto& cb : connect_callbacks)
			cb(*this, id);
	}

	// Allocate + activate in one call (convenience, backward compat).
	uint8_t connect() {
		uint8_t id = alloc_peer_slot();
		if (id == 255) return 255;
		activate_peer(id);
		return id;
	}

	void set_peer_id(uint8_t id) {
		if (self_id < 64) {
			peer_bits &= ~(1ULL << self_id);
			peers_arr[self_id].connected = false;
		}
		self_id = id;
		if (id < 64) {
			peer_bits |= (1ULL << id);
			peers_arr[id].id = id;
			peers_arr[id].connected = true;
		}
	}

	void set_dedicated() {
		host_mode = true;
		if (self_id < 64) {
			peer_bits &= ~(1ULL << self_id);
			peers_arr[self_id].connected = false;
		}
		self_id = 255;
		peer_bits = 0;
	}

	void set_client() {
		host_mode = false;
		if (self_id < 64) {
			peer_bits &= ~(1ULL << self_id);
			peers_arr[self_id].connected = false;
		}
		self_id = 255;
		peer_bits = 0;
	}

	void disconnect(uint8_t id) {
		if (id == self_id || id >= 64) return;
		if (!(peer_bits & (1ULL << id))) return;
		for (auto& cb : disconnect_callbacks)
			cb(*this, id);
		peer_bits &= ~(1ULL << id);
		peers_arr[id].reset();
	}

	// --- Send rate ---
	uint32_t net_frame = 0;
	bool should_send = false;
	float send_timer = 0;

	// --- Custom packet dispatch ---
	using PacketHandler = std::function<void(Pm&, const uint8_t*, int, struct sockaddr_in&)>;
	PacketHandler packet_handlers[256]{};

	void on_recv(uint8_t type, PacketHandler fn) {
		packet_handlers[type] = std::move(fn);
	}

	// --- Pool registration ---
	using RecvHandler = std::function<void(Pm&, const uint8_t*, uint16_t)>;
	struct PoolHandler {
		RecvHandler on_sync;
		RecvHandler on_removal;
	};
	std::unordered_map<uint32_t, PoolHandler> handlers;

	void register_pool(uint32_t pool_id, RecvHandler sync_fn, RecvHandler rm_fn) {
		handlers[pool_id] = {std::move(sync_fn), std::move(rm_fn)};
	}

	// --- Sync state registry ---
	struct SyncRegistryEntry {
		PoolSyncState* state;
		std::function<void()> repend_fn;
	};
	std::unordered_map<uint32_t, SyncRegistryEntry> sync_registry;

	PoolSyncState* get_sync_state(uint32_t pool_id) {
		auto it = sync_states.find(pool_id);
		if (it != sync_states.end()) return it->second.get();
		auto state = std::make_unique<PoolSyncState>();
		state->pool_id = pool_id;
		auto* ptr = state.get();
		sync_states[pool_id] = std::move(state);
		return ptr;
	}

	// --- Per-peer network state ---
	static constexpr uint16_t MAX_ENTRY_SIZE = 48;
	static constexpr uint16_t MAX_STATE_SIZE = 240;

	struct PeerNet {
		struct SyncEntry { uint32_t pool_id; uint8_t data[MAX_ENTRY_SIZE]; uint16_t size; };
		std::vector<SyncEntry> sync_outbox;
		uint16_t next_seq = 1;
		uint16_t last_recv_seq = 0;
		float snapshot_age = 0.f;
		uint32_t packets_sent = 0;

		// --- Heartbeat / timeout ---
		float last_recv_time = 0.f;
		float last_send_time = 0.f;

		// --- Ordered custom packets ---
		uint16_t custom_send_seq = 0;
		uint16_t custom_recv_seq = 0;

		// --- Buffered removals (flushed as reliable batches) ---
		struct PendingRemoval { uint32_t pool_id; Id id; };
		std::vector<PendingRemoval> pending_removals;

		// --- State sync outbox ---
		struct StateEntry { uint32_t state_id; uint8_t data[MAX_STATE_SIZE]; uint16_t size; };
		std::vector<StateEntry> state_outbox;

		// --- Reliable message channel ---
		static constexpr uint16_t MAX_RELIABLE_PAYLOAD = 240;
		static constexpr uint8_t  RELIABLE_SEND_COUNT = 60;
		static constexpr int      RELIABLE_DEDUP_SIZE = 64;

		struct ReliableEntry {
			uint16_t msg_id;
			uint8_t envelope[4 + MAX_RELIABLE_PAYLOAD];
			uint16_t envelope_size;
			uint8_t sends_remaining;
		};
		std::vector<ReliableEntry> reliable_outbox;
		uint16_t next_reliable_id = 1;

		uint16_t reliable_seen[RELIABLE_DEDUP_SIZE]{};
		uint8_t reliable_seen_count = 0;

		bool has_seen_reliable(uint16_t msg_id) const {
			int n = std::min((int)reliable_seen_count, RELIABLE_DEDUP_SIZE);
			for (int i = 0; i < n; i++)
				if (reliable_seen[i] == msg_id) return true;
			return false;
		}
		void mark_seen_reliable(uint16_t msg_id) {
			int idx = reliable_seen_count % RELIABLE_DEDUP_SIZE;
			reliable_seen[idx] = msg_id;
			reliable_seen_count++;
		}

		PeerNet() { sync_outbox.reserve(512); }
		void clear_frame() { sync_outbox.clear(); state_outbox.clear(); }
	};
	PeerNet peer_net[64];

	void push(uint8_t peer, uint32_t pool_id, const void* data, uint16_t data_size) {
		assert(data_size > 0 && "entry_size must be > 0 (would cause infinite loop in flush)");
		assert(data_size <= MAX_ENTRY_SIZE && "entry exceeds max sync entry size");
		auto& entry = peer_net[peer].sync_outbox.emplace_back();
		entry.pool_id = pool_id;
		entry.size = data_size;
		memcpy(entry.data, data, data_size);
	}

	// --- Removal tracking (buffered, flushed as reliable batches) ---
	void track_removal(uint32_t pool_id, Id id) {
		for (uint8_t p : remote_peers())
			peer_net[p].pending_removals.push_back({pool_id, id});
	}

	void tracked_remove(Pm& pm, PoolBase* pool, Id id) {
		track_removal(pool->pool_id, id);
		pm.id_remove(id);
	}

	template<typename T>
	void clear_pool(Pm& pm, Pool<T>* pool) {
		pool->each([&](Id id, const T&) {
			track_removal(pool->pool_id, id);
			pm.id_remove(id);
		}, Parallel::Off);
	}

	// Batch pending removals into reliable messages, grouped by pool_id.
	// Payload format: {pool_id(4), count(2), ids(8*count)}
	void flush_removals(uint8_t peer) {
		auto& pn = peer_net[peer];
		if (pn.pending_removals.empty()) return;

		// Sort by pool_id for batching
		std::sort(pn.pending_removals.begin(), pn.pending_removals.end(),
			[](auto& a, auto& b) { return a.pool_id < b.pool_id; });

		constexpr uint16_t hdr_size = 4 + 2; // pool_id + count
		constexpr uint16_t ids_per_batch =
			(PeerNet::MAX_RELIABLE_PAYLOAD - hdr_size) / sizeof(Id);

		size_t i = 0;
		while (i < pn.pending_removals.size()) {
			uint32_t pid = pn.pending_removals[i].pool_id;
			size_t run_start = i;
			while (i < pn.pending_removals.size() && pn.pending_removals[i].pool_id == pid) i++;
			size_t run_len = i - run_start;

			size_t offset = 0;
			while (offset < run_len) {
				uint16_t count = (uint16_t)std::min((size_t)ids_per_batch, run_len - offset);
				uint8_t buf[PeerNet::MAX_RELIABLE_PAYLOAD];
				uint8_t* w = buf;
				memcpy(w, &pid, 4); w += 4;
				memcpy(w, &count, 2); w += 2;
				for (uint16_t j = 0; j < count; j++) {
					memcpy(w, &pn.pending_removals[run_start + offset + j].id, sizeof(Id));
					w += sizeof(Id);
				}
				send_reliable(peer, RELIABLE_INNER_REMOVAL, buf, (uint16_t)(w - buf));
				offset += count;
			}
		}
		pn.pending_removals.clear();
	}

	// --- State sync push (unreliable broadcast every tick) ---
	void push_state(uint8_t peer, uint32_t state_id, const void* data, uint16_t size) {
		assert(size <= MAX_STATE_SIZE && "state data exceeds max size");
		auto& entry = peer_net[peer].state_outbox.emplace_back();
		entry.state_id = state_id;
		entry.size = size;
		memcpy(entry.data, data, size);
	}

	void push_state_all(uint32_t state_id, const void* data, uint16_t size) {
		for (uint8_t p : remote_peers())
			push_state(p, state_id, data, size);
	}

	void flush_state(uint8_t peer) {
		auto& pn = peer_net[peer];
		for (auto& e : pn.state_outbox) {
			uint8_t buf[sizeof(PktStateSync) + MAX_STATE_SIZE];
			PktStateSync hdr{};
			hdr.state_id = e.state_id;
			hdr.size = e.size;
			memcpy(buf, &hdr, sizeof(hdr));
			memcpy(buf + sizeof(hdr), e.data, e.size);
			sock.send(buf, sizeof(hdr) + e.size, peer_addrs[peer]);
		}
		pn.state_outbox.clear();
	}

	// --- State sync registration ---
	using StateRecvHandler = std::function<void(Pm&, const uint8_t*, uint16_t)>;
	std::unordered_map<uint32_t, StateRecvHandler> state_recv_handlers;

	void on_state_recv(uint32_t state_id, StateRecvHandler fn) {
		state_recv_handlers[state_id] = std::move(fn);
	}

	// bind_state_send: register a task that serializes state and pushes to all peers every tick.
	// WriteFn signature: uint16_t(Pm& pm, uint8_t* out_buf) — returns bytes written (0 = skip).
	template<typename WriteFn>
	void bind_state_send(Pm& pm, uint32_t state_id, const char* task_name, float priority, WriteFn write_fn) {
		pm.task_add(task_name, priority, [this, state_id, write_fn](Pm& pm) {
			if (!should_send) return;
			uint8_t buf[MAX_STATE_SIZE];
			uint16_t sz = write_fn(pm, buf);
			if (sz > 0) push_state_all(state_id, buf, sz);
		});
	}

	// --- Ordered custom packet helpers (opt-in per-peer sequencing) ---
	// Game packets can include a seq field and use these to reject stale packets.
	uint16_t next_send_seq(uint8_t peer) {
		return ++peer_net[peer].custom_send_seq;
	}

	bool accept_seq(uint8_t peer, uint16_t seq) {
		auto& s = peer_net[peer].custom_recv_seq;
		if (s != 0 && !seq_after(seq, s)) return false;
		s = seq;
		return true;
	}

	// --- Reliable message channel ---
	// Wraps payload in a reliable envelope (PKT_RELIABLE + msg_id + inner_type).
	// Resends each flush cycle until acked or sends exhausted.
	// Receiver auto-acks and deduplicates. Inner type dispatches to packet_handlers[].
	void send_reliable(uint8_t peer, uint8_t inner_type, const void* data, uint16_t size) {
		assert(size <= PeerNet::MAX_RELIABLE_PAYLOAD && "reliable payload too large");
		auto& pn = peer_net[peer];
		PeerNet::ReliableEntry entry{};
		entry.msg_id = pn.next_reliable_id++;
		entry.sends_remaining = PeerNet::RELIABLE_SEND_COUNT;

		PktReliable hdr{};
		hdr.msg_id = entry.msg_id;
		hdr.inner_type = inner_type;
		memcpy(entry.envelope, &hdr, sizeof(hdr));
		if (size > 0) memcpy(entry.envelope + sizeof(hdr), data, size);
		entry.envelope_size = sizeof(hdr) + size;

		pn.reliable_outbox.push_back(entry);
	}

	void send_reliable_all(uint8_t inner_type, const void* data, uint16_t size) {
		for (uint8_t p : remote_peers())
			send_reliable(p, inner_type, data, size);
	}

	void ack_reliable(uint8_t peer, uint16_t msg_id) {
		auto& pn = peer_net[peer];
		pn.reliable_outbox.erase(
			std::remove_if(pn.reliable_outbox.begin(), pn.reliable_outbox.end(),
				[msg_id](auto& r) { return r.msg_id == msg_id; }),
			pn.reliable_outbox.end());
	}

	void flush_reliable(uint8_t peer) {
		auto& pn = peer_net[peer];
		if (!has_addr[peer]) return;
		for (auto& r : pn.reliable_outbox) {
			sock.send(r.envelope, r.envelope_size, peer_addrs[peer]);
			if (r.sends_remaining > 0) r.sends_remaining--;
		}
		pn.reliable_outbox.erase(
			std::remove_if(pn.reliable_outbox.begin(), pn.reliable_outbox.end(),
				[](auto& r) { return r.sends_remaining == 0; }),
			pn.reliable_outbox.end());
	}

	// --- Find peer by source address ---
	uint8_t find_peer_by_addr(const struct sockaddr_in& addr) const {
		for (uint8_t p : remote_peers()) {
			if (!has_addr[p]) continue;
			if (peer_addrs[p].sin_addr.s_addr == addr.sin_addr.s_addr &&
				peer_addrs[p].sin_port == addr.sin_port)
				return p;
		}
		return 255;
	}

	// --- bind_recv(): register receive-side pool sync (client use) ---
	template <typename T, typename ReadFn>
	void bind_recv(Pool<T>* pool, ReadFn read_fn) {
		register_pool(pool->pool_id,
			[pool, read_fn](Pm& pm, const uint8_t* data, uint16_t count) {
				read_fn(pm, pool, data, count);
			},
			[](Pm& pm, const uint8_t* data, uint16_t count) {
				for (uint16_t i = 0; i < count; i++) {
					Id id; memcpy(&id, data + i * sizeof(Id), sizeof(Id));
					pm.id_sync(id);
					pm.id_remove(id);
				}
			});
	}

	// --- bind_send(): change-tracked send (sparse synced[], pending list) ---
	// Entities are only transmitted when they call notify_change() or are newly
	// added/removed. Install a change hook via Pool::set_change_hook before use,
	// or call notify_change() manually each frame for always-moving entities.
	template <typename T, typename WriteFn, typename InterestFn = std::nullptr_t>
	void bind_send(Pm& pm, Pool<T>* pool, const char* task_name, float priority,
				   WriteFn write_fn, InterestFn interest_fn = nullptr, float hysteresis = 0.f) {

		auto* ss = get_sync_state(pool->pool_id);

		sync_registry[pool->pool_id] = {
			ss,
			[ss, pool]() { ss->repend_all(pool); }
		};

		pool->set_change_hook([](void* ctx, Id id) {
			static_cast<PoolSyncState*>(ctx)->mark_changed(id);
		}, ss);

		ss->repend_all(pool);

		pm.task_add(task_name, priority, [this, pool, ss, write_fn, interest_fn, hysteresis](Pm& pm) {
			(void)pm;
			if (!should_send) return;
			constexpr bool has_interest = !std::is_same_v<InterestFn, std::nullptr_t>;

			uint64_t remote_mask = remote_peers().bits;

			for (uint8_t p : remote_peers()) {
				if constexpr (has_interest) {
					for (size_t di = 0; di < pool->items.size(); di++) {
						Id id = pool->id_at(di);
						if (!ss->is_synced_to(p, id)) continue;
						if (!interest_fn(pm, p, id, pool->items[di], hysteresis)) {
							ss->mark_unsynced_for(p, id);
							peer_net[p].pending_removals.push_back({pool->pool_id, id});
						}
					}
				}

				ss->each_unsynced(pool, p, remote_mask, [&](Id id, T& val, size_t) {
					if constexpr (has_interest) {
						if (!interest_fn(pm, p, id, val, 0.f)) return;
					}
					uint8_t buf[MAX_ENTRY_SIZE];
					uint16_t sz = write_fn(id, val, buf);
					push(p, pool->pool_id, buf, sz);
					ss->mark_synced(p, id);
				});
			}

			if (remote_mask == 0)
				ss->clear_pending();
		});
	}

	// --- Flush: MTU-aware packet building for one peer (sync data only) ---
	void flush_peer(uint8_t peer) {
		auto& pn = peer_net[peer];
		if (pn.sync_outbox.empty() && pn.state_outbox.empty() && pn.pending_removals.empty())
			return;

		// Group sync entries by pool_id
		flush_pools.clear();
		for (auto& e : pn.sync_outbox) {
			auto& pv = flush_pools[e.pool_id];
			pv.pool_id = e.pool_id;
			pv.entry_size = e.size;
			pv.ptrs.push_back(e.data);
		}

		if (!flush_pools.empty()) {
			if (flush_buf.size() < (size_t)(mtu + 256))
				flush_buf.resize(mtu + 256);
			uint8_t* w = flush_buf.data() + sizeof(PktSync);
			uint16_t sections = 0;

			auto send_packet = [&]() {
				if (sections == 0) return;
				uint16_t pkt_seq = pn.next_seq++;
				PktSync hdr{};
				hdr.seq = pkt_seq;
				hdr.frame = net_frame;
				hdr.server_time = local_time;
				hdr.section_count = sections;
				memcpy(flush_buf.data(), &hdr, sizeof(hdr));
				send_to(peer, flush_buf.data(), (int)(w - flush_buf.data()));
				pn.packets_sent++;
				w = flush_buf.data() + sizeof(PktSync);
				sections = 0;
			};

			auto space = [&]() -> int { return mtu - (int)(w - flush_buf.data()); };

			for (auto& [pid, pv] : flush_pools) {
				const uint8_t** data = pv.ptrs.data();
				size_t total = pv.ptrs.size();
				uint16_t esz = pv.entry_size;
				size_t si = 0;

				while (si < total) {
					int min_needed = (int)sizeof(SectionHeader) + (int)esz;
					if (space() < min_needed) send_packet();

					int avail = space() - (int)sizeof(SectionHeader);
					uint16_t fit = (uint16_t)std::min((size_t)(avail / esz), total - si);
					if (fit == 0) { send_packet(); continue; }

					SectionHeader sh{pid, fit, esz};
					memcpy(w, &sh, sizeof(sh)); w += sizeof(sh);
					for (uint16_t i = 0; i < fit; i++) {
						memcpy(w, data[si + i], esz);
						w += esz;
					}
					si += fit;
					sections++;
				}
			}
			send_packet();
		}

		// Flush state sync (individual small packets)
		flush_state(peer);

		// Flush removals as reliable batches
		flush_removals(peer);

		pn.clear_frame();
	}

	// --- Socket init helpers ---
	void start() {
		sock.init(port);
		if (self_id < 64) {
			peers_arr[self_id].id = self_id;
			peers_arr[self_id].connected = true;
		}
	}
	void start_client() { sock.init(0); }

	float snapshot_age(uint8_t peer) const { return peer_net[peer].snapshot_age; }

private:
	std::unordered_map<uint32_t, std::unique_ptr<PoolSyncState>> sync_states;

	std::vector<uint8_t> flush_buf;

	struct FlushPoolView {
		uint32_t pool_id = 0;
		uint16_t entry_size = 0;
		std::vector<const uint8_t*> ptrs;
	};
	std::unordered_map<uint32_t, FlushPoolView> flush_pools;
};

// =============================================================================
// net_init() — Register NetSys tasks with the scheduler
//
// Call after configuring net->port, net->send_rate, etc.
// Registers tasks:
//   net/recv  — recv + dispatch: sync, state, handshake, reliable, custom
//   net/tick  — timekeeping, send rate, heartbeat, timeout, connect retry
//   net/flush — MTU sync packets, state packets, reliable removal batches, resend
// =============================================================================
inline void net_init(Pm& pm, NetSys* net, float recv_phase, float send_phase)
{
	pm.task_add("net/recv", recv_phase, [net](Pm& pm) {
		if (net->sock.sock == INVALID_SOCKET) return;
		struct sockaddr_in src{}; int n; uint8_t buf[16384];

		while ((n = net->sock.recv(buf, sizeof(buf), src)) > 0) {
			uint8_t type = buf[0];

			// Update last_recv_time for the peer this came from
			uint8_t src_peer = net->find_peer_by_addr(src);
			if (src_peer < 64)
				net->peer_net[src_peer].last_recv_time = net->local_time;

			// --- Sync packets (server→client) ---
			if (type == PKT_SYNC_TYPE && n >= (int)sizeof(PktSync)) {
				PktSync hdr; memcpy(&hdr, buf, sizeof(hdr));

				if (!net->host_mode) {
					auto& pn = net->peer_net[0];
					if (pn.last_recv_seq != 0 && !seq_after(hdr.seq, pn.last_recv_seq))
						continue;
					pn.last_recv_seq = hdr.seq;
					pn.snapshot_age = 0.f;
					pn.last_recv_time = net->local_time;

					// Clock sync: estimate server time offset
					net->server_frame = hdr.frame;
					net->clock_offset = hdr.server_time - net->local_time;
				}

				const uint8_t* ptr = buf + sizeof(PktSync);
				const uint8_t* end = buf + n;

				for (uint16_t s = 0; s < hdr.section_count && ptr + sizeof(SectionHeader) <= end; s++) {
					SectionHeader sh; memcpy(&sh, ptr, sizeof(sh)); ptr += sizeof(sh);
					uint32_t sync_bytes = (uint32_t)sh.sync_count * sh.entry_size;
					if (ptr + sync_bytes > end) break;

					auto it = net->handlers.find(sh.pool_id);
					if (it != net->handlers.end() && sh.sync_count > 0 && it->second.on_sync)
						it->second.on_sync(pm, ptr, sh.sync_count);
					ptr += sync_bytes;
				}
			}
			// --- State sync (server→client, unreliable per-tick) ---
			else if (type == PKT_STATE_SYNC && n >= (int)sizeof(PktStateSync)) {
				PktStateSync hdr; memcpy(&hdr, buf, sizeof(hdr));
				if ((int)sizeof(PktStateSync) + hdr.size <= n) {
					auto it = net->state_recv_handlers.find(hdr.state_id);
					if (it != net->state_recv_handlers.end())
						it->second(pm, buf + sizeof(PktStateSync), hdr.size);
				}
			}
			// --- Reliable message: dedup, ack, dispatch inner type ---
			else if (type == PKT_RELIABLE && n >= (int)sizeof(PktReliable)) {
				PktReliable hdr; memcpy(&hdr, buf, sizeof(hdr));

				PktReliableAck ack{};
				ack.msg_id = hdr.msg_id;
				net->sock.send(&ack, sizeof(ack), src);

				uint8_t rel_peer = src_peer;
				if (rel_peer == 255 && !net->host_mode) rel_peer = 0;
				if (rel_peer < 64) {
					auto& pn = net->peer_net[rel_peer];
					if (pn.has_seen_reliable(hdr.msg_id)) continue;
					pn.mark_seen_reliable(hdr.msg_id);
				}

				// --- Reliable removal batches (framework internal) ---
				if (hdr.inner_type == RELIABLE_INNER_REMOVAL) {
					const uint8_t* payload = buf + sizeof(PktReliable);
					int payload_size = n - (int)sizeof(PktReliable);
					if (payload_size >= 6) {
						uint32_t pool_id; memcpy(&pool_id, payload, 4);
						uint16_t count; memcpy(&count, payload + 4, 2);
						if (payload_size >= (int)(6 + count * sizeof(Id))) {
							auto it = net->handlers.find(pool_id);
							if (it != net->handlers.end() && it->second.on_removal)
								it->second.on_removal(pm, payload + 6, count);
						}
					}
				}
				// --- User reliable messages ---
				else if (net->packet_handlers[hdr.inner_type]) {
					uint8_t inner_buf[16384];
					int inner_size = n - (int)sizeof(PktReliable);
					if (inner_size > 0) {
						inner_buf[0] = hdr.inner_type;
						memcpy(inner_buf + 1, buf + sizeof(PktReliable), inner_size);
						net->packet_handlers[hdr.inner_type](pm, inner_buf, inner_size + 1, src);
					}
				}
			}
			// --- Reliable ack: remove from outbox ---
			else if (type == PKT_RELIABLE_ACK && n >= (int)sizeof(PktReliableAck)) {
				PktReliableAck ack; memcpy(&ack, buf, sizeof(ack));
				if (src_peer < 64)
					net->ack_reliable(src_peer, ack.msg_id);
				else if (!net->host_mode)
					net->ack_reliable(0, ack.msg_id);
			}
			// --- Heartbeat ---
			else if (type == PKT_HEARTBEAT) {
				// recv time already updated above
			}
			// --- Connect request (server receives from client) ---
			else if (type == PKT_CONNECT_REQ && n >= (int)sizeof(PktConnectReq) && net->host_mode) {
				PktConnectReq hdr; memcpy(&hdr, buf, sizeof(hdr));

				// Version check
				if (hdr.version != net->protocol_version) {
					PktConnectDeny deny{}; deny.reason = DENY_VERSION_MISMATCH;
					net->sock.send(&deny, sizeof(deny), src);
					continue;
				}

				// Already connected from this addr? Re-send cached ACK.
				uint8_t existing = net->find_peer_by_addr(src);
				if (existing < 64 && net->peers_arr[existing].ack_size > 0) {
					net->sock.send(net->peers_arr[existing].ack_buf,
								   net->peers_arr[existing].ack_size, src);
					continue;
				}

				// Find a free slot (don't activate yet — validator may deny)
				uint8_t slot = net->alloc_peer_slot();
				if (slot == 255) {
					PktConnectDeny deny{}; deny.reason = DENY_SERVER_FULL;
					net->sock.send(&deny, sizeof(deny), src);
					continue;
				}

				// Call validator if set
				const uint8_t* payload = buf + sizeof(PktConnectReq);
				uint16_t payload_size = (uint16_t)(n - sizeof(PktConnectReq));
				NetSys::ConnectResult result = NetSys::ConnectResult::accept();
				if (net->connect_validator) {
					result = net->connect_validator(slot, src, payload, payload_size);
				}

				if (!result.accepted) {
					PktConnectDeny deny{}; deny.reason = result.deny_reason;
					net->sock.send(&deny, sizeof(deny), src);
					continue;
				}

				// Accepted — activate peer, store addr
				net->activate_peer(slot);
				net->peer_addrs[slot] = src;
				net->has_addr[slot] = true;
				net->peer_net[slot].last_recv_time = net->local_time;

				// Build + cache ACK for idempotent re-sends
				uint8_t ack_buf[sizeof(PktConnectAck) + MAX_CONNECT_PAYLOAD];
				PktConnectAck ack_hdr{}; ack_hdr.peer_id = slot;
				memcpy(ack_buf, &ack_hdr, sizeof(ack_hdr));
				if (result.response_size > 0)
					memcpy(ack_buf + sizeof(ack_hdr), result.response, result.response_size);
				uint16_t ack_total = sizeof(ack_hdr) + result.response_size;
				memcpy(net->peers_arr[slot].ack_buf, ack_buf, ack_total);
				net->peers_arr[slot].ack_size = ack_total;

				// Send ACK
				net->sock.send(ack_buf, ack_total, src);
			}
			// --- Connect ack (client receives from server) ---
			else if (type == PKT_CONNECT_ACK && n >= (int)sizeof(PktConnectAck) && !net->host_mode) {
				if (net->conn_state != NetSys::ConnState::CONNECTING) continue;
				PktConnectAck hdr; memcpy(&hdr, buf, sizeof(hdr));
				net->set_peer_id(hdr.peer_id);
				net->conn_state = NetSys::ConnState::CONNECTED;
				const uint8_t* payload = buf + sizeof(PktConnectAck);
				uint16_t payload_size = (uint16_t)(n - sizeof(PktConnectAck));
				if (net->connected_callback)
					net->connected_callback(*net, hdr.peer_id, payload, payload_size);
			}
			// --- Connect deny (client receives from server) ---
			else if (type == PKT_CONNECT_DENY && n >= (int)sizeof(PktConnectDeny) && !net->host_mode) {
				PktConnectDeny hdr; memcpy(&hdr, buf, sizeof(hdr));
				net->conn_state = NetSys::ConnState::DISCONNECTED;
				if (net->connect_denied_callback)
					net->connect_denied_callback(*net, hdr.reason);
			}
			// --- Custom packet handlers ---
			else if (net->packet_handlers[type]) {
				net->packet_handlers[type](pm, buf, n, src);
			}
		}
	});

	pm.task_add("net/tick", send_phase - 10.f, [net](Pm& pm) {
		net->local_time += pm.loop_dt();
		net->should_send = false;

		// --- Client connect retry/timeout ---
		if (net->conn_state == NetSys::ConnState::CONNECTING) {
			net->connect_elapsed += pm.loop_dt();
			if (net->connect_elapsed >= net->connect_timeout) {
				net->conn_state = NetSys::ConnState::DISCONNECTED;
				if (net->connect_denied_callback)
					net->connect_denied_callback(*net, 0); // reason 0 = timeout
			} else {
				net->connect_timer += pm.loop_dt();
				if (net->connect_timer >= net->connect_retry_interval) {
					net->connect_timer = 0.f;
					net->send_connect_req();
				}
			}
		}

		for (uint8_t p : net->remote_peers()) {
			net->peer_net[p].snapshot_age = std::min(net->peer_net[p].snapshot_age + pm.loop_dt(), 0.2f);

			// Timeout: disconnect peers we haven't heard from
			if (net->peer_net[p].last_recv_time > 0.f &&
				net->local_time - net->peer_net[p].last_recv_time > net->peer_timeout) {
				net->disconnect(p);
			}
		}

		if (net->sock.sock != INVALID_SOCKET) {
			net->send_timer += pm.loop_dt();
			if (net->send_timer >= net->send_rate) {
				net->send_timer -= net->send_rate;
				net->should_send = true;
				net->net_frame++;
			}

			// Heartbeat: send to peers we haven't sent to recently
			for (uint8_t p : net->remote_peers()) {
				if (!net->has_addr[p]) continue;
				if (net->local_time - net->peer_net[p].last_send_time >= net->heartbeat_interval) {
					PktHeartbeat hb{};
					net->send_to(p, &hb, sizeof(hb));
					net->peer_net[p].last_send_time = net->local_time;
				}
			}
		}
	});

	pm.task_add("net/flush", send_phase + 5.f, [net](Pm&) {
		if (!net->should_send) {
			for (uint8_t p : net->remote_peers()) net->peer_net[p].clear_frame();
			return;
		}
		for (uint8_t p : net->remote_peers()) {
			if (!net->has_addr[p]) { net->peer_net[p].clear_frame(); continue; }
			net->flush_peer(p);
			net->flush_reliable(p);
			net->peer_net[p].last_send_time = net->local_time;
		}
	});
}

} // namespace pm