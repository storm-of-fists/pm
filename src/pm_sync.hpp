// pm_sync.hpp — Generic network sync transport
//
// NetSys handles:
//   - Per-peer outbox with pre-serialized entries (no scanning in flush)
//   - MTU-aware packet splitting
//   - Reliable removals with ack tracking
//   - Pool bind() for one-call serialization registration
//   - Interest management with hysteresis
//   - Send rate throttling
//   - Generic recv dispatch by packet type
//
// NetSys knows NOTHING about game types. Games register custom packet
// handlers via on_recv() and pool serialization via bind().
//
// Depends: pm_core.hpp, pm_net.hpp (UdpSocket)

#pragma once
#include "pm_core.hpp"
#include "pm_net.hpp"
#include <cstring>
#include <functional>
#include <algorithm>
#include <unordered_map>
#include <vector>
#include <type_traits>

namespace pm {

// =============================================================================
// Wire format — generic sync protocol
// =============================================================================
#pragma pack(push, 1)
static constexpr uint8_t PKT_SYNC_TYPE = 0xFE; // reserved type for sync packets

struct PktSync {
    uint8_t type = PKT_SYNC_TYPE;
    uint16_t seq;
    uint32_t frame;
    uint16_t section_count;
};

struct SectionHeader {
    uint32_t pool_id;
    uint16_t sync_count, entry_size, rm_count;
};
#pragma pack(pop)

static inline bool seq_after(uint16_t a, uint16_t b) { return (int16_t)(a - b) > 0; }

// =============================================================================
// NetSys — fully generic network sync transport
//
// Game code:
//   1. Create: pm.sys<NetSys>("net")
//   2. Configure: net->port = 9998; net->connect_ip = "1.2.3.4";
//   3. Register packet handlers: net->on_recv(MY_TYPE, handler);
//   4. Register pool sync: net->bind(pm, pool, "task", write, read);
//   5. pm.run_loop() — NetSys schedules recv/tick/flush internally.
//
// Game is responsible for:
//   - Connection protocol (join/welcome/auth)
//   - Client-to-host input forwarding (schedule your own send task)
//   - Calling on_ack() when receiving ack sequences from peers
// =============================================================================
struct NetSys : public System {

    // --- Configuration (set before run_loop) ---
    int port = 0;               // 0 = ephemeral (client), nonzero = bind (host)
    float send_rate = 1.f/30.f; // seconds between sync flushes
    int mtu = 1200;             // safe UDP payload before fragmentation
    const char* connect_ip = nullptr; // client: host IP to connect to

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

    // --- Send rate ---
    uint32_t net_frame = 0;
    bool should_send = false;
    float send_timer = 0;
    float snapshot_age = 0.f;

    // --- Custom packet dispatch ---
    //
    // Games register handlers for their packet types. The recv loop
    // dispatches PktSync internally; everything else goes to handlers.
    //
    //   net->on_recv(PKT_INPUT, [](Ctx& ctx, const uint8_t* buf, int len, sockaddr_in& src) {
    //       PktInput p; memcpy(&p, buf, sizeof(p));
    //       // ...
    //   });

    using PacketHandler = std::function<void(Ctx&, const uint8_t*, int, struct sockaddr_in&)>;
    PacketHandler packet_handlers[256]{};

    void on_recv(uint8_t type, PacketHandler fn) {
        packet_handlers[type] = std::move(fn);
    }

    // --- Pool registration ---
    using RecvHandler = std::function<void(Ctx&, const uint8_t*, uint16_t)>;
    struct PoolHandler {
        RecvHandler on_sync;
        RecvHandler on_removal;
    };
    std::unordered_map<uint32_t, PoolHandler> handlers;

    void register_pool(uint32_t pool_id, RecvHandler sync_fn, RecvHandler rm_fn) {
        handlers[pool_id] = {std::move(sync_fn), std::move(rm_fn)};
    }

    // --- Per-peer network state ---
    struct PendingRemoval {
        uint32_t pool_id;
        Id id;
        uint16_t first_sent_seq;
    };

    struct PeerNet {
        struct SyncEntry { uint32_t pool_id; uint8_t data[48]; uint16_t size; };
        std::vector<SyncEntry> sync_outbox;
        std::vector<PendingRemoval> pending_removals;
        uint16_t next_seq = 1;
        uint16_t last_acked = 0;
        uint32_t packets_sent = 0;

        PeerNet() { sync_outbox.reserve(512); pending_removals.reserve(128); }
        void clear_frame() { sync_outbox.clear(); }
    };
    PeerNet peer_net[64];

    // Client-side: last received sync seq (game reads this for ack piggybacking)
    uint16_t client_last_recv_seq = 0;

    // --- Push: bucketed by peer ---
    void push(uint8_t peer, uint32_t pool_id, const void* data, uint16_t data_size) {
        auto& entry = peer_net[peer].sync_outbox.emplace_back();
        entry.pool_id = pool_id;
        entry.size = std::min(data_size, (uint16_t)sizeof(entry.data));
        memcpy(entry.data, data, entry.size);
    }

    // --- Reliable removal tracking ---
    void track_removal(Ctx& ctx, uint32_t pool_id, Id id) {
        for (uint8_t peer : ctx.remote_peers())
            peer_net[peer].pending_removals.push_back({pool_id, id, 0});
    }

    // --- Ack processing: game calls this when it receives an ack from a peer ---
    void on_ack(uint8_t peer, uint16_t acked_seq) {
        auto& pn = peer_net[peer];
        if (!seq_after(acked_seq, pn.last_acked) && acked_seq != pn.last_acked)
            return;
        pn.last_acked = acked_seq;
        pn.pending_removals.erase(
            std::remove_if(pn.pending_removals.begin(), pn.pending_removals.end(),
                [acked_seq](auto& r) { return r.first_sent_seq != 0 && !seq_after(r.first_sent_seq, acked_seq); }),
            pn.pending_removals.end());
    }

    // --- bind(): one-call sync registration ---
    //
    // WriteFn:    (Id, const T&, uint8_t* out) -> uint16_t
    // ReadFn:     (Ctx&, Pool<T>*, const uint8_t* data, uint16_t count) -> void
    // InterestFn: (Ctx&, uint8_t peer, Id, const T&, float margin) -> bool
    //
    // Interest defaults to nullptr (sync everything). When provided, margin
    // enables hysteresis: enter at tight radius (margin=0), leave at wider
    // radius (margin=hysteresis). Prevents churn at boundaries.

    template <typename T, typename WriteFn, typename ReadFn,
              typename InterestFn = std::nullptr_t>
    void bind(Pm& pm, Pool<T>* pool, const char* task_name,
              WriteFn write_fn, ReadFn read_fn,
              InterestFn interest_fn = nullptr, float hysteresis = 0.f) {
        register_pool(pool->pool_id,
            [pool, read_fn](Ctx& ctx, const uint8_t* data, uint16_t count) {
                read_fn(ctx, pool, data, count);
            },
            [pool](Ctx& ctx, const uint8_t* data, uint16_t count) {
                for (uint16_t i = 0; i < count; i++) {
                    Id id; memcpy(&id, data + i * sizeof(Id), sizeof(Id));
                    if (pool->has(id)) ctx.remove(id);
                }
            });

        pm.schedule(task_name, Pm::Phase::NET_SEND, [this, pool, write_fn, interest_fn, hysteresis](Ctx& ctx) {
            if (!should_send || !ctx.is_host()) return;
            constexpr bool has_interest = !std::is_same_v<InterestFn, std::nullptr_t>;

            for (uint8_t peer : ctx.remote_peers()) {
                if constexpr (has_interest) {
                    for (size_t di = 0; di < pool->items.size(); di++) {
                        if (!pool->is_synced_to(peer, di)) continue;
                        if (!interest_fn(ctx, peer, pool->dense_ids[di], pool->items[di], hysteresis)) {
                            pool->unsync_for(peer, di);
                            peer_net[peer].pending_removals.push_back({pool->pool_id, pool->dense_ids[di], 0});
                        }
                    }
                }

                pool->each_unsynced(peer, [&](Id id, T& val, size_t) {
                    if constexpr (has_interest) {
                        if (!interest_fn(ctx, peer, id, val, 0.f)) return;
                    }
                    uint8_t buf[48];
                    uint16_t sz = write_fn(id, val, buf);
                    push(peer, pool->pool_id, buf, sz);
                });
            }
        }, 0.f, RunOn::Host);
    }

    // --- Flush: MTU-aware packet building for one peer ---
    void flush_peer(uint8_t peer) {
        auto& pn = peer_net[peer];
        if (pn.sync_outbox.empty() && pn.pending_removals.empty()) return;

        uint16_t seq = pn.next_seq++;

        for (auto& r : pn.pending_removals)
            if (r.first_sent_seq == 0) r.first_sent_seq = seq;

        struct PoolView {
            uint32_t pool_id; uint16_t entry_size;
            std::vector<const uint8_t*> ptrs;
        };
        std::unordered_map<uint32_t, PoolView> pools;
        for (auto& e : pn.sync_outbox) {
            auto& pv = pools[e.pool_id];
            pv.pool_id = e.pool_id;
            pv.entry_size = e.size;
            pv.ptrs.push_back(e.data);
        }

        std::unordered_map<uint32_t, std::vector<Id>> rm_by_pool;
        for (auto& r : pn.pending_removals)
            rm_by_pool[r.pool_id].push_back(r.id);

        std::vector<uint32_t> all_pool_ids;
        for (auto& [pid, _] : pools) all_pool_ids.push_back(pid);
        for (auto& [pid, _] : rm_by_pool)
            if (pools.find(pid) == pools.end()) all_pool_ids.push_back(pid);

        // Use mtu member, not constant
        std::vector<uint8_t> buf(mtu + 256);
        uint8_t* w = buf.data() + sizeof(PktSync);
        uint16_t sections = 0;

        auto send_packet = [&]() {
            if (sections == 0) return;
            PktSync hdr{PKT_SYNC_TYPE, seq, net_frame, sections};
            memcpy(buf.data(), &hdr, sizeof(hdr));
            send_to(peer, buf.data(), (int)(w - buf.data()));
            pn.packets_sent++;
            w = buf.data() + sizeof(PktSync);
            sections = 0;
        };

        auto space = [&]() -> int { return mtu - (int)(w - buf.data()); };

        for (uint32_t pid : all_pool_ids) {
            auto pool_it = pools.find(pid);
            auto rm_it = rm_by_pool.find(pid);

            const uint8_t** sync_data = nullptr;
            size_t sync_total = 0;
            uint16_t entry_size = 0;
            if (pool_it != pools.end()) {
                sync_data = pool_it->second.ptrs.data();
                sync_total = pool_it->second.ptrs.size();
                entry_size = pool_it->second.entry_size;
            }

            const Id* rm_data = nullptr;
            size_t rm_total = 0;
            if (rm_it != rm_by_pool.end()) {
                rm_data = rm_it->second.data();
                rm_total = rm_it->second.size();
            }

            size_t si = 0, ri = 0;
            while (si < sync_total || ri < rm_total) {
                int min_needed = (int)sizeof(SectionHeader) + std::max((int)entry_size, (int)sizeof(Id));
                if (space() < min_needed) send_packet();

                int avail = space() - (int)sizeof(SectionHeader);
                uint16_t sync_fit = 0, rm_fit = 0;

                if (si < sync_total && entry_size > 0) {
                    sync_fit = (uint16_t)std::min((size_t)(avail / entry_size), sync_total - si);
                    avail -= sync_fit * entry_size;
                }
                if (ri < rm_total)
                    rm_fit = (uint16_t)std::min((size_t)(avail / (int)sizeof(Id)), rm_total - ri);

                if (sync_fit == 0 && rm_fit == 0) { send_packet(); continue; }

                SectionHeader sh{pid, sync_fit, entry_size, rm_fit};
                memcpy(w, &sh, sizeof(sh)); w += sizeof(sh);
                for (uint16_t i = 0; i < sync_fit; i++) {
                    memcpy(w, sync_data[si + i], entry_size);
                    w += entry_size;
                }
                for (uint16_t i = 0; i < rm_fit; i++) {
                    memcpy(w, &rm_data[ri + i], sizeof(Id));
                    w += sizeof(Id);
                }
                si += sync_fit;
                ri += rm_fit;
                sections++;
            }
        }

        send_packet();
        pn.clear_frame();
    }

    // --- Lifecycle ---
    // Call start() after configure to open socket. Tasks are safe before this
    // (they guard on INVALID_SOCKET). This enables deferred init for menus.
    void start() { sock.init(port); }
    void start_client() { sock.init(0); }

    void shutdown(Pm&) override { sock.close_sock(); }

    void initialize(Pm& pm) override {
        // Socket NOT opened here — game calls start()/start_client() when ready

        // recv: dispatch by packet type
        pm.schedule(tname("recv").c_str(), Pm::Phase::NET_RECV, [this](Ctx& ctx) {
            if (sock.sock == INVALID_SOCKET) return;
            struct sockaddr_in src{}; int n; uint8_t buf[16384];

            while ((n = sock.recv(buf, sizeof(buf), src)) > 0) {
                uint8_t type = buf[0];

                if (type == PKT_SYNC_TYPE && n >= (int)sizeof(PktSync)) {
                    // Built-in: sync section dispatch
                    PktSync hdr; memcpy(&hdr, buf, sizeof(hdr));
                    snapshot_age = 0.f;
                    client_last_recv_seq = hdr.seq;

                    const uint8_t* ptr = buf + sizeof(PktSync);
                    const uint8_t* end = buf + n;

                    for (uint16_t s = 0; s < hdr.section_count && ptr + sizeof(SectionHeader) <= end; s++) {
                        SectionHeader sh; memcpy(&sh, ptr, sizeof(sh)); ptr += sizeof(sh);
                        uint32_t sync_bytes = (uint32_t)sh.sync_count * sh.entry_size;
                        uint32_t rm_bytes   = (uint32_t)sh.rm_count * sizeof(Id);
                        if (ptr + sync_bytes + rm_bytes > end) break;

                        auto it = handlers.find(sh.pool_id);
                        if (it != handlers.end()) {
                            if (sh.sync_count > 0 && it->second.on_sync)
                                it->second.on_sync(ctx, ptr, sh.sync_count);
                            if (sh.rm_count > 0 && it->second.on_removal)
                                it->second.on_removal(ctx, ptr + sync_bytes, sh.rm_count);
                        }
                        ptr += sync_bytes + rm_bytes;
                    }
                }
                else if (packet_handlers[type]) {
                    packet_handlers[type](ctx, buf, n, src);
                }
            }
        });

        // tick: throttle send rate
        pm.schedule(tname("tick").c_str(), Pm::Phase::NET_SEND - 10.f, [this](Ctx& ctx) {
            should_send = false;
            snapshot_age += ctx.dt();
            if (sock.sock != INVALID_SOCKET) {
                send_timer += ctx.dt();
                if (send_timer >= send_rate) { send_timer -= send_rate; should_send = true; net_frame++; }
            }
        });

        // flush: per-peer, MTU-aware
        pm.schedule(tname("flush").c_str(), Pm::Phase::NET_SEND + 5.f, [this](Ctx& ctx) {
            if (!should_send || !ctx.is_host()) {
                for (uint8_t peer : ctx.remote_peers()) peer_net[peer].clear_frame();
                return;
            }
            for (uint8_t peer : ctx.remote_peers()) {
                if (!has_addr[peer]) { peer_net[peer].clear_frame(); continue; }
                flush_peer(peer);
            }
        });
    }
};

} // namespace pm