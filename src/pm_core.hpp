// pm.hpp — Process Manager kernel
//
// THE RULE OF 3 VERBS:
// 1. Data/Logic: pm.pool<T>(), pm.sys<T>(), pm.queue<T>()
// 2. Ids: pm.spawn(), pm.find(name), pm.remove(id)
// 3. Cleanup: pm.drop(name)
//
// SEPARATION OF CONCERNS:
// Pools and Queues are pure data containers — no lifecycle, no logic.
// Systems are the only thing with initialize() and shutdown().
// Systems call pm.schedule() to inject logic phases into the timeline.

#pragma once

#include <vector>
#include <string>
#include <string_view>
#include <unordered_map>
#include <algorithm>
#include <functional>
#include <cassert>
#include <chrono>
#include <thread>
#include <cstdint>
#include <type_traits>
#include <stdexcept>

#if defined(_MSC_VER) && !defined(__clang__)
#include <intrin.h>
inline uint32_t pm_ctz64(uint64_t mask)
{
    unsigned long index;
    _BitScanForward64(&index, mask);
    return static_cast<uint32_t>(index);
}
#else
inline uint32_t pm_ctz64(uint64_t mask)
{
    return static_cast<uint32_t>(__builtin_ctzll(mask));
}
#endif

namespace pm
{

    // =============================================================================
    // Names — Interned into a global table, passed around as 4-byte handles
    // =============================================================================
    using NameId = uint32_t;
    constexpr NameId NULL_NAME = 0xFFFFFFFF;

    class NameTable
    {
        struct StringHash
        {
            using is_transparent = void;
            size_t operator()(std::string_view sv) const { return std::hash<std::string_view>{}(sv); }
            size_t operator()(const std::string &s) const { return std::hash<std::string_view>{}(s); }
            size_t operator()(const char *s) const { return std::hash<std::string_view>{}(s); }
        };
        std::unordered_map<std::string, NameId, StringHash, std::equal_to<>> m_to_id;
        std::vector<std::string> m_to_str;

    public:
        NameId intern(const char *s)
        {
            if (!s || !s[0])
                return NULL_NAME;
            auto it = m_to_id.find(s);
            if (it != m_to_id.end())
                return it->second;
            NameId id = static_cast<NameId>(m_to_str.size());
            m_to_id.emplace(s, id);
            m_to_str.push_back(s);
            return id;
        }

        NameId find(const char *s) const
        {
            if (!s || !s[0])
                return NULL_NAME;
            auto it = m_to_id.find(s);
            return it != m_to_id.end() ? it->second : NULL_NAME;
        }

        const char *str(NameId id) const
        {
            if (id == NULL_NAME || id >= m_to_str.size())
                return "";
            return m_to_str[id].c_str();
        }
    };

    // =============================================================================
    // Run Filters
    // =============================================================================
    enum class RunOn : uint8_t
    {
        All,
        Host,
        NonHost
    };

    struct Result
    {
        const char *error = nullptr;
        static Result ok() { return {nullptr}; }
        static Result err(const char *msg) { return {msg}; }
        explicit operator bool() const { return error == nullptr; }
    };

    // =============================================================================
    // Fast RTTI-free type IDs
    // =============================================================================
    inline size_t next_type_id()
    {
        static size_t counter = 0;
        return ++counter;
    }
    template <typename T>
    inline size_t get_type_id()
    {
        static size_t id = next_type_id();
        return id;
    }

    // =============================================================================
    // Ids & Handles
    // =============================================================================
    using Id = uint64_t;
    constexpr Id NULL_ID = 0xFFFFFFFFFFFFFFFFULL;
    constexpr uint32_t NULL_INDEX = 0xFFFFFFFF;

    inline uint32_t id_index(Id id) { return static_cast<uint32_t>(id & 0xFFFFFFFFULL); }
    inline uint32_t id_generation(Id id) { return static_cast<uint32_t>(id >> 32); }
    inline Id make_id(uint32_t idx, uint32_t gen) { return (static_cast<uint64_t>(gen) << 32) | idx; }

    template <typename T>
    class Pool;

    template <typename T>
    struct Handle
    {
        Id id = NULL_ID;
        Pool<T> *pool = nullptr;

        T *get() const { return pool ? pool->get(id) : nullptr; }
        T *modify() const
        {
            T *ptr = get();
            if (ptr && pool)
                pool->unsync(id);
            return ptr;
        }
        T *operator->() const { return get(); }
        T &operator*() const { return *get(); }
        explicit operator bool() const { return get() != nullptr; }
    };

    // =============================================================================
    // PeerRange — zero-alloc iterator over set bits in a bitmask
    //
    //   for (uint8_t peer : ctx.peers()) { ... }
    //   for (uint8_t peer : ctx.remote_peers()) { ... }
    // =============================================================================
    struct PeerRange
    {
        uint64_t bits;

        struct Iterator
        {
            uint64_t remaining;
            uint8_t operator*() const
            {
                // Count trailing zeros = index of lowest set bit
                return (uint8_t)__builtin_ctzll(remaining);
            }
            Iterator &operator++()
            {
                remaining &= remaining - 1; // clear lowest set bit
                return *this;
            }
            bool operator!=(const Iterator &o) const { return remaining != o.remaining; }
        };

        Iterator begin() const { return {bits}; }
        Iterator end() const { return {0}; }
        uint8_t count() const { uint64_t v = bits; uint8_t c = 0; while (v) { c++; v &= v-1; } return c; }
        bool empty() const { return bits == 0; }
        bool has(uint8_t id) const { return id < 64 && (bits & (1ULL << id)); }
    };

    // =============================================================================
    // Peer — per-slot state managed by Pm
    //
    // PeerConnection handles sync tracking (send/ack ring buffer).
    // PeerInfo adds user-visible metadata and a user_data pointer for
    // systems to attach whatever they need (display name, latency, etc).
    // =============================================================================
    struct SentRecord
    {
        uint32_t pool_id;
        size_t dense_idx;
    };

    struct SentPacket
    {
        uint16_t sequence = 0;
        bool active = false;
        std::vector<SentRecord> records;
    };

    constexpr size_t SENT_RING_SIZE = 64;

    struct Peer
    {
        uint8_t id = 255;
        bool connected = false;
        uint64_t connected_tick = 0;
        void *user_data = nullptr;

        // Sync tracking
        uint16_t next_seq = 1;
        SentPacket sent_ring[SENT_RING_SIZE] = {};

        void reset()
        {
            id = 255;
            connected = false;
            connected_tick = 0;
            user_data = nullptr;
            next_seq = 1;
            for (auto &sp : sent_ring)
            {
                sp.sequence = 0;
                sp.active = false;
                sp.records.clear();
            }
        }
    };

    // =============================================================================
    // Core Interfaces
    // =============================================================================
    class Pm;

    // IPool — type-erased base for Pool<T>.
    // Lets Pm manage pools during entity removal without knowing T.
    class IPool
    {
    public:
        virtual ~IPool() = default;
        uint32_t pool_id = 0;
        virtual void remove(Id id) = 0;
        virtual void clear_all() = 0;
        virtual void sync(uint8_t peer, size_t dense_idx) = 0;
        virtual void repend_all() = 0;
        virtual void shrink_to_fit() = 0; // release excess memory
    };

    // System — the ONLY user-facing type with lifecycle.
    // Systems own logic. Pools and queues own data.
    class System
    {
    public:
        virtual ~System() = default;
        NameId m_name = NULL_NAME;
        std::string m_name_str;

        // Build prefixed task name: "sysname/suffix"
        // Used during initialize() to give tasks unique, traceable names.
        //   pm.schedule(tname("collision").c_str(), ...)
        //   → "game/collision" if system registered as "game"
        std::string tname(const char *suffix) const
        {
            return m_name_str + "/" + suffix;
        }

        virtual void initialize(Pm &) {}
        virtual void shutdown(Pm &) {}
    };

    // =============================================================================
    // Pool<T> — Contiguous DOD Sparse Set + Per-Entry Sync Bitmask
    //
    // Pure data container. No lifecycle methods.
    // Created via pm.pool<T>("name"). Pm wires up entity→pool bitset.
    //
    // Each entry carries a uint64_t synced mask — one bit per peer/connection.
    // Bits start all-0s (unsynced for everyone). Set to 1 via sync() when a
    // peer receives the update. New peers automatically see everything unsynced
    // because their bit was never set.
    //
    // A pending list tracks which entries have been unsynced. each_unsynced()
    // only iterates the pending list, not all entries. Pending is stored as
    // Ids (not dense indices) so swap-remove doesn't invalidate it. Stale and
    // fully-synced entries are compacted during each_unsynced().
    // =============================================================================

    template <typename T>
    class Pool : public IPool
    {
    public:
        NameId m_name = NULL_NAME;

        std::vector<T> items;
        std::vector<Id> dense_ids;
        std::vector<uint64_t> synced;     // parallel to items — per-peer sync bitmask
        std::vector<uint32_t> sparse_indices;

        // Pending sync list — Ids that may need syncing
        std::vector<Id> m_pending;
        std::vector<uint8_t> m_pending_flag; // parallel to sparse_indices — dedup
        uint64_t *m_peer_slots = nullptr;    // pointer to Pm's peer_slots
        uint8_t *m_self_id = nullptr;        // pointer to Pm's peer_id

        // --- Sync tracking ---

        // Mark entry unsynced for all peers (by Id)
        void unsync(Id id)
        {
            uint32_t idx = id_index(id);
            if (idx >= sparse_indices.size() || sparse_indices[idx] == NULL_INDEX)
                return;
            synced[sparse_indices[idx]] = 0;
            pending_add(idx, id);
        }

        // Mark entry synced for a peer (by dense index)
        void sync(uint8_t peer, size_t dense_idx) override
        {
            synced[dense_idx] |= (1ULL << peer);
        }

        // Mark all entries synced for a peer
        void sync_all(uint8_t peer)
        {
            uint64_t bit = 1ULL << peer;
            for (auto &s : synced)
                s |= bit;
        }

        // Check if entry is synced to a specific peer (by dense index)
        bool is_synced_to(uint8_t peer, size_t dense_idx) const
        {
            return (synced[dense_idx] & (1ULL << peer)) != 0;
        }

        // Clear sync for a specific peer (by dense index).
        // Entity will re-appear as unsynced for that peer.
        // Use for interest management: entity left peer's relevant area.
        void unsync_for(uint8_t peer, size_t dense_idx)
        {
            synced[dense_idx] &= ~(1ULL << peer);
            Id id = dense_ids[dense_idx];
            pending_add(id_index(id), id);
        }

        // Iterate entries not yet synced to a specific peer.
        // Only touches entries in the pending list. Compacts stale/fully-synced entries.
        template <typename F>
        void each_unsynced(uint8_t peer, F &&fn)
        {
            uint64_t bit = 1ULL << peer;
            // Compact based on remote peers (all except self)
            uint64_t self_mask = (m_self_id && *m_self_id < 64) ? (1ULL << *m_self_id) : 0;
            uint64_t remote = m_peer_slots ? (*m_peer_slots & ~self_mask) : 0;
            size_t w = 0;
            for (size_t r = 0; r < m_pending.size(); r++)
            {
                Id id = m_pending[r];
                uint32_t idx = id_index(id);

                // Stale: entity removed or generation mismatch
                if (idx >= sparse_indices.size() || sparse_indices[idx] == NULL_INDEX
                    || dense_ids[sparse_indices[idx]] != id)
                {
                    if (idx < m_pending_flag.size()) m_pending_flag[idx] = 0;
                    continue;
                }

                size_t di = sparse_indices[idx];

                // Fully synced to all remote peers — evict
                if (remote && (synced[di] & remote) == remote)
                {
                    m_pending_flag[idx] = 0;
                    continue;
                }

                m_pending[w++] = id;
                if (!(synced[di] & bit))
                    fn(id, items[di], di);
            }
            m_pending.resize(w);
        }

        size_t pending_count() const { return m_pending.size(); }

        // --- Sparse set operations ---

        T *add(Id id, T val = T{})
        {
            uint32_t idx = id_index(id);
            if (idx >= sparse_indices.size())
                sparse_indices.resize(idx + 1, NULL_INDEX);

            if (sparse_indices[idx] != NULL_INDEX)
            {
                uint32_t dense_idx = sparse_indices[idx];
                items[dense_idx] = std::move(val);
                synced[dense_idx] = 0;
                pending_add(idx, id);
                return &items[dense_idx];
            }

            uint32_t dense_idx = static_cast<uint32_t>(items.size());
            sparse_indices[idx] = dense_idx;
            dense_ids.push_back(id);
            items.push_back(std::move(val));
            synced.push_back(0);

            pending_add(idx, id);
            return &items.back();
        }

        void remove(Id id) override
        {
            uint32_t idx = id_index(id);
            if (idx >= sparse_indices.size() || sparse_indices[idx] == NULL_INDEX)
                return;

            uint32_t dense_idx = sparse_indices[idx];
            if (dense_ids[dense_idx] != id)
                return;

            // Clear pending flag — stale entry will be compacted in each_unsynced
            if (idx < m_pending_flag.size())
                m_pending_flag[idx] = 0;

            uint32_t last_dense_idx = static_cast<uint32_t>(items.size() - 1);
            Id last_id = dense_ids[last_dense_idx];

            if (dense_idx != last_dense_idx)
            {
                items[dense_idx] = std::move(items[last_dense_idx]);
                dense_ids[dense_idx] = last_id;
                synced[dense_idx] = synced[last_dense_idx];
                sparse_indices[id_index(last_id)] = dense_idx;
            }

            sparse_indices[idx] = NULL_INDEX;
            items.pop_back();
            dense_ids.pop_back();
            synced.pop_back();
        }

        void clear_all() override
        {
            for (Id id : dense_ids)
            {
                uint32_t idx = id_index(id);
                sparse_indices[idx] = NULL_INDEX;
                if (idx < m_pending_flag.size())
                    m_pending_flag[idx] = 0;
            }
            items.clear();
            dense_ids.clear();
            synced.clear();
            m_pending.clear();
        }

        // Release excess memory after bulk removal / level transition
        void shrink_to_fit() override
        {
            items.shrink_to_fit();
            dense_ids.shrink_to_fit();
            synced.shrink_to_fit();
            m_pending.shrink_to_fit();
        }

        // Full reset: clear all data AND release sparse array memory
        void reset()
        {
            clear_all();
            sparse_indices.clear();
            sparse_indices.shrink_to_fit();
            m_pending_flag.clear();
            m_pending_flag.shrink_to_fit();
            shrink_to_fit();
        }

        T *get(Id id)
        {
            uint32_t idx = id_index(id);
            if (idx < sparse_indices.size() && sparse_indices[idx] != NULL_INDEX && dense_ids[sparse_indices[idx]] == id)
                return &items[sparse_indices[idx]];
            return nullptr;
        }
        const T *get(Id id) const { return const_cast<Pool *>(this)->get(id); }

        bool has(Id id) const { return get(id) != nullptr; }

        Handle<T> handle(Id id) { return {id, this}; }

        struct Entry
        {
            Id id;
            T &value;
            Pool<T> *pool;
            T *modify()
            {
                pool->unsync(id);
                return &value;
            }
        };
        struct ConstEntry
        {
            Id id;
            const T &value;
        };

        struct Iterator
        {
            Pool *p;
            size_t i;
            Entry operator*() { return {p->dense_ids[i], p->items[i], p}; }
            Iterator &operator++()
            {
                ++i;
                return *this;
            }
            bool operator!=(const Iterator &o) const { return i != o.i; }
        };
        struct ConstIterator
        {
            const Pool *p;
            size_t i;
            ConstEntry operator*() const { return {p->dense_ids[i], p->items[i]}; }
            ConstIterator &operator++()
            {
                ++i;
                return *this;
            }
            bool operator!=(const ConstIterator &o) const { return i != o.i; }
        };

        struct Range
        {
            Pool *p;
            Iterator begin() { return {p, 0}; }
            Iterator end() { return {p, p->items.size()}; }
        };
        struct ConstRange
        {
            const Pool *p;
            ConstIterator begin() const { return {p, 0}; }
            ConstIterator end() const { return {p, p->items.size()}; }
        };

        Range each() { return {this}; }
        ConstRange each() const { return {this}; }

    private:
        void pending_add(uint32_t idx, Id id)
        {
            if (idx >= m_pending_flag.size())
                m_pending_flag.resize(idx + 1, 0);
            if (!m_pending_flag[idx])
            {
                m_pending_flag[idx] = 1;
                m_pending.push_back(id);
            }
        }

    public:
        // Re-add all entries to pending. Called by Pm when a new peer connects
        // so the new peer sees everything.
        void repend_all() override
        {
            m_pending.clear();
            std::fill(m_pending_flag.begin(), m_pending_flag.end(), 0);
            for (size_t i = 0; i < items.size(); i++)
                pending_add(id_index(dense_ids[i]), dense_ids[i]);
        }
    };

    // =============================================================================
    // Queue<T> — Vector-backed Auto-Clearing Event Channel
    //
    // Pure data container. Pm clears it each frame automatically.
    // =============================================================================
    class IQueue
    {
    public:
        virtual ~IQueue() = default;
        virtual void clear() = 0;
        virtual void shrink_to_fit() = 0;
    };

    template <typename T>
    class Queue : public IQueue
    {
    public:
        NameId m_name = NULL_NAME;
        std::vector<T> items;

        void push(T val) { items.push_back(std::move(val)); }
        void clear() override { items.clear(); }
        void shrink_to_fit() override { items.shrink_to_fit(); }
        void reset() { clear(); shrink_to_fit(); }

        auto begin() { return items.begin(); }
        auto end() { return items.end(); }
        size_t size() const { return items.size(); }
    };

    // =============================================================================
    // Ctx — Task execution context
    //
    // Exposes the runtime API tasks need each frame. Intentionally omits
    // init-time operations (sys<T>, pool<T>, schedule) to guide users toward
    // capturing pointers during initialize().
    //
    // For admin tasks (level loading, mod init), use ctx.pm() to escape.
    // =============================================================================
    class Pm;
    class Ctx
    {
        Pm *m_pm;
    public:
        explicit Ctx(Pm &pm) : m_pm(&pm) {}

        // Escape hatch — full Pm access for admin tasks
        Pm &pm();

        // Runtime API — defined after Pm
        float dt() const;
        uint64_t tick_count() const;
        bool is_host() const;
        uint8_t peer_id() const;
        uint64_t peer_slots() const;
        uint8_t peer_count() const;

        Id spawn(NameId name = NULL_NAME);
        Id spawn(const char *name);
        void remove(Id id);
        bool is_removing(Id id) const;
        void sync_id(Id id);

        void quit();
        bool is_running() const;
        bool is_paused() const;
        void pause();
        void resume();
        void toggle_pause();

        PeerRange peers() const;
        PeerRange remote_peers() const;
        Peer &peer(uint8_t id);
        const Peer &peer(uint8_t id) const;

        const std::vector<std::string> &faults() const;
        bool stepping() const;

        template <typename T> Queue<T> *queue(NameId name);
        template <typename T> Queue<T> *queue(const char *name);
    };

    // =============================================================================
    // Task
    // =============================================================================
    struct Task
    {
        NameId name = NULL_NAME;
        float priority = 0, hz = 0, accum = 0;
        RunOn run_on = RunOn::All;
        bool active = true;
        bool pauseable = false;

        std::function<Result(Ctx &)> fn;

        uint64_t runs = 0, last_us = 0, max_us = 0;

        void record(uint64_t us)
        {
            last_us = us;
            if (us > max_us)
                max_us = us;
            runs++;
        }
    };

    // =============================================================================
    // Pm Kernel
    // =============================================================================
    class Pm
    {
        NameTable m_names;

        std::vector<uint32_t> m_id_gen;
        std::vector<uint32_t> m_free_ids;
        std::vector<NameId> m_id_to_name;
        std::unordered_map<NameId, Id> m_name_to_id;

        std::vector<uint8_t> m_removing;
        std::vector<Id> m_deferred_removes;
        size_t m_remove_cursor = 0; // cursor into deferred removes (avoids O(N) erase)
        float m_remove_budget_us = 0.f; // 0 = unlimited (microseconds per frame)
        std::vector<NameId> m_deferred_task_stops;

        std::vector<Task> m_tasks;
        bool m_tasks_dirty = false;

        // Pools — pure data, no lifecycle
        struct PoolEntry
        {
            IPool *pool = nullptr;
            size_t type_id = 0;
        };
        std::vector<PoolEntry> m_pools;
        std::vector<IPool *> m_pool_by_id;
        uint32_t m_next_pool_id = 0;

        // Queues — pure data, Pm clears each frame
        struct QueueEntry
        {
            IQueue *queue = nullptr;
            size_t type_id = 0;
        };
        std::vector<QueueEntry> m_queues;

        // Systems — the only thing with lifecycle
        struct SysEntry
        {
            System *sys = nullptr;
            size_t type_id = 0;
        };
        std::vector<SysEntry> m_sys;
        std::vector<System *> m_systems; // init order, for reverse shutdown

        float m_loop_rate = 0.f, m_raw_dt = 0.f;
        uint64_t m_tick = 0;
        uint8_t m_peer_id = 0;
        uint64_t m_peer_slots = 1; // bit 0 = host (self), always active by default
        Peer m_peers[64] = {};
        bool m_first_run = true, m_running = true, m_stepping = false, m_paused = false, m_step_requested = false;
        NameId m_current_task = NULL_NAME;
        std::chrono::steady_clock::time_point m_last_time;
        std::vector<std::string> m_faults;

        // Lifecycle hooks
        using PeerCallback = std::function<void(Pm &, uint8_t)>;
        std::vector<PeerCallback> m_on_connect;
        std::vector<PeerCallback> m_on_disconnect;

    public:
        Pm()
        {
            // Self (host by default) is always connected
            m_peers[0].id = 0;
            m_peers[0].connected = true;
            m_peers[0].connected_tick = 0;
        }
        Pm(const Pm &) = delete;
        Pm &operator=(const Pm &) = delete;
        Pm(Pm &&) = delete;
        Pm &operator=(Pm &&) = delete;

        ~Pm()
        {
            // Only systems have shutdown — reverse init order
            for (auto it = m_systems.rbegin(); it != m_systems.rend(); ++it)
                (*it)->shutdown(*this);

            for (auto &r : m_sys)
                delete r.sys;
            for (auto &r : m_pools)
                delete r.pool;
            for (auto &r : m_queues)
                delete r.queue;
        }

        // ====== DEFAULT PHASES ======
        struct Phase
        {
            static constexpr float INPUT = 10.f;
            static constexpr float NET_RECV = 15.f;
            static constexpr float SIMULATE = 30.f;
            static constexpr float COLLIDE = 50.f;
            static constexpr float CLEANUP = 55.f;
            static constexpr float DRAW = 70.f;
            static constexpr float HUD = 80.f;
            static constexpr float RENDER = 90.f;
            static constexpr float NET_SEND = 95.f;
        };

        // ====== NAME INTERNING ======
        NameId intern(const char *s) { return m_names.intern(s); }
        const char *name_str(NameId id) const { return m_names.str(id); }
        NameTable &names() { return m_names; }

        // ====== CORE TIMELINE ======
        void set_loop_rate(float hz) { m_loop_rate = hz; }
        void set_remove_budget_us(float us) { m_remove_budget_us = us; } // 0 = unlimited
        float remove_budget_us() const { return m_remove_budget_us; }
        size_t remove_pending() const { return m_deferred_removes.size() - m_remove_cursor; }
        void quit() { m_running = false; }
        bool is_running() const { return m_running; }
        float dt() const { return m_raw_dt; }
        uint64_t tick_count() const { return m_tick; }
        uint64_t *tick_ptr() { return &m_tick; }
        uint8_t peer_id() const { return m_peer_id; }

        void set_peer_id(uint8_t id)
        {
            // Move self bit and peer metadata from old slot to new slot
            if (m_peer_id < 64)
            {
                m_peer_slots &= ~(1ULL << m_peer_id);
                m_peers[m_peer_id].connected = false;
            }
            m_peer_id = id;
            if (id < 64)
            {
                m_peer_slots |= (1ULL << id);
                m_peers[id].id = id;
                m_peers[id].connected = true;
                m_peers[id].connected_tick = m_tick;
            }
        }

        bool is_host() const { return m_peer_id == 0; }
        uint64_t peer_slots() const { return m_peer_slots; }

        // --- Peer ranges ---

        // All connected peers (including self)
        PeerRange peers() const { return {m_peer_slots}; }

        // All connected peers except self
        PeerRange remote_peers() const
        {
            uint64_t self_mask = (m_peer_id < 64) ? (1ULL << m_peer_id) : 0;
            return {m_peer_slots & ~self_mask};
        }

        // --- Peer info ---

        Peer &peer(uint8_t id) { return m_peers[id]; }
        const Peer &peer(uint8_t id) const { return m_peers[id]; }

        // --- Lifecycle hooks ---

        void on_connect(PeerCallback fn) { m_on_connect.push_back(std::move(fn)); }
        void on_disconnect(PeerCallback fn) { m_on_disconnect.push_back(std::move(fn)); }

        // --- Connect/disconnect ---

        uint8_t connect()
        {
            for (uint8_t i = 0; i < 64; i++)
            {
                if (i == m_peer_id) continue; // skip self
                if (!(m_peer_slots & (1ULL << i)))
                {
                    m_peer_slots |= (1ULL << i);
                    m_peers[i].reset();
                    m_peers[i].id = i;
                    m_peers[i].connected = true;
                    m_peers[i].connected_tick = m_tick;
                    // New peer needs to see everything — re-add all entries to pending
                    for (auto *p : m_pool_by_id)
                        if (p) p->repend_all();
                    for (auto &cb : m_on_connect)
                        cb(*this, i);
                    return i;
                }
            }
            return 255; // full
        }

        void disconnect(uint8_t peer)
        {
            if (peer == m_peer_id || peer >= 64)
                return; // can't disconnect self
            if (!(m_peer_slots & (1ULL << peer)))
                return; // not connected
            for (auto &cb : m_on_disconnect)
                cb(*this, peer);
            m_peer_slots &= ~(1ULL << peer);
            m_peers[peer].reset();
        }

        uint8_t peer_count() const
        {
            uint64_t v = m_peer_slots;
            uint8_t c = 0;
            while (v) { c++; v &= v - 1; }
            return c;
        }

        // ====== SEND/ACK — sequence-tracked sync ======

        // Begin building a packet for a peer. Returns the sequence number.
        uint16_t send_begin(uint8_t peer)
        {
            auto &pc = m_peers[peer];
            uint16_t seq = pc.next_seq++;
            auto &sp = pc.sent_ring[seq % SENT_RING_SIZE];
            sp.sequence = seq;
            sp.active = true;
            sp.records.clear();
            return seq;
        }

        // Track that a pool entry was included in the current packet.
        template <typename T>
        void send_track(uint8_t peer, Pool<T> *pool, size_t dense_idx)
        {
            send_track(peer, pool->pool_id, dense_idx);
        }

        // Type-erased send_track — for code that only knows pool_id
        void send_track(uint8_t peer, uint32_t pool_id, size_t dense_idx)
        {
            auto &pc = m_peers[peer];
            uint16_t seq = pc.next_seq - 1;
            auto &sp = pc.sent_ring[seq % SENT_RING_SIZE];
            sp.records.push_back({pool_id, dense_idx});
        }

        // Finalize the packet. (Currently a no-op, but keeps the API symmetric
        // and gives us a hook for future work like stats or compression.)
        void send_end(uint8_t /*peer*/) {}

        // Peer acked a sequence. Marks all tracked entries as synced.
        void recv_ack(uint8_t peer, uint16_t acked_seq)
        {
            auto &pc = m_peers[peer];
            auto &sp = pc.sent_ring[acked_seq % SENT_RING_SIZE];
            if (!sp.active || sp.sequence != acked_seq)
                return;

            for (auto &rec : sp.records)
            {
                if (rec.pool_id < m_pool_by_id.size() && m_pool_by_id[rec.pool_id])
                    m_pool_by_id[rec.pool_id]->sync(peer, rec.dense_idx);
            }
            sp.active = false;
        }

        // Ack a range of sequences (handles cumulative acks).
        void recv_ack_range(uint8_t peer, uint16_t from_seq, uint16_t to_seq)
        {
            for (uint16_t s = from_seq; s <= to_seq; s++)
                recv_ack(peer, s);
        }
        const std::vector<std::string> &faults() const { return m_faults; }

        bool stepping() const { return m_stepping; }
        bool is_paused() const { return m_paused; }
        void pause() { m_paused = true; }
        void resume() { m_paused = false; }
        void toggle_pause() { m_paused = !m_paused; }
        void request_step() { m_step_requested = true; }
        NameId current_task() const { return m_current_task; }

        void run_loop()
        {
            while (m_running)
                run();
        }

        void run()
        {
            auto now = std::chrono::steady_clock::now();
            if (m_first_run)
            {
                m_last_time = now;
                m_first_run = false;
            }

            if (m_loop_rate > 0.f)
            {
                float target_us = 1e6f / m_loop_rate;
                float elapsed_us = std::chrono::duration<float, std::micro>(now - m_last_time).count();
                if (elapsed_us < target_us)
                {
                    float sleep_us = target_us - elapsed_us;
                    if (sleep_us > 2000.f)
                        std::this_thread::sleep_for(std::chrono::microseconds((int)(sleep_us - 2000.f)));
                    while (std::chrono::duration<float, std::micro>(std::chrono::steady_clock::now() - m_last_time).count() < target_us)
                        std::this_thread::yield();
                }
                now = std::chrono::steady_clock::now();
            }

            m_raw_dt = std::min(std::chrono::duration<float>(now - m_last_time).count(), 0.1f);
            m_last_time = now;
            m_tick++;

            // Consume step request — this frame runs pauseable tasks once
            m_stepping = m_step_requested;
            m_step_requested = false;

            if (m_tasks_dirty)
            {
                m_tasks.erase(std::remove_if(m_tasks.begin(), m_tasks.end(), [](const Task &t)
                                             { return !t.active; }),
                              m_tasks.end());
                std::stable_sort(m_tasks.begin(), m_tasks.end(), [](const Task &a, const Task &b)
                                 { return a.priority < b.priority; });
                m_tasks_dirty = false;
            }

            size_t num_tasks = m_tasks.size();
            for (size_t i = 0; i < num_tasks; ++i)
            {
                if (!m_tasks[i].active || !m_tasks[i].fn)
                    continue;
                if (m_tasks[i].run_on == RunOn::Host && m_peer_id != 0)
                    continue;
                if (m_tasks[i].run_on == RunOn::NonHost && m_peer_id == 0)
                    continue;
                if (m_tasks[i].pauseable && m_paused && !m_stepping)
                    continue;

                auto exec_t = [&]()
                {
                    auto t0 = std::chrono::steady_clock::now();
                    Ctx ctx(*this);
                    Result r = m_tasks[i].fn(ctx);
                    m_tasks[i].record(std::chrono::duration_cast<std::chrono::microseconds>(std::chrono::steady_clock::now() - t0).count());
                    if (!r)
                    {
                        m_tasks[i].active = false;
                        m_faults.push_back(std::string(name_str(m_tasks[i].name)) + ": " + r.error);
                        m_tasks_dirty = true;
                    }
                };

                if (m_tasks[i].hz > 0)
                {
                    m_tasks[i].accum += m_raw_dt;
                    float interval = 1.0f / m_tasks[i].hz;
                    if (m_tasks[i].accum > interval * 5.0f)
                        m_tasks[i].accum = interval * 5.0f;
                    while (m_tasks[i].accum >= interval && m_tasks[i].active)
                    {
                        m_tasks[i].accum -= interval;
                        m_current_task = m_tasks[i].name;
                        exec_t();
                        m_current_task = NULL_NAME;
                    }
                }
                else
                {
                    m_current_task = m_tasks[i].name;
                    exec_t();
                    m_current_task = NULL_NAME;
                }
            }

            // Process deferred entity removes (time-budgeted, cursor-based)
            {
                auto remove_start = std::chrono::steady_clock::now();
                size_t total = m_deferred_removes.size();

                while (m_remove_cursor < total)
                {
                    Id id = m_deferred_removes[m_remove_cursor];
                    uint32_t idx = id_index(id);

                    if (idx < m_id_gen.size() && m_id_gen[idx] == id_generation(id))
                    {
                        if (idx < m_id_to_name.size() && m_id_to_name[idx] != NULL_NAME)
                        {
                            auto it = m_name_to_id.find(m_id_to_name[idx]);
                            if (it != m_name_to_id.end() && it->second == id)
                                m_name_to_id.erase(it);
                            m_id_to_name[idx] = NULL_NAME;
                        }

                        m_id_gen[idx]++;
                        if (m_id_gen[idx] == 0)
                            m_id_gen[idx] = 1;

                        m_free_ids.push_back(idx);
                        m_removing[idx] = 0;

                        // Linear scan — Pool::remove() fast-fails in 2 comparisons
                        for (IPool *pool : m_pool_by_id)
                            if (pool) pool->remove(id);
                    }

                    ++m_remove_cursor;

                    // Check time budget every 16 removes to avoid clock overhead
                    if (m_remove_budget_us > 0.f && (m_remove_cursor & 15) == 0)
                    {
                        float elapsed = std::chrono::duration<float, std::micro>(
                                            std::chrono::steady_clock::now() - remove_start)
                                            .count();
                        if (elapsed >= m_remove_budget_us)
                            break;
                    }
                }

                if (m_remove_cursor == total)
                {
                    m_deferred_removes.clear();
                    m_remove_cursor = 0;
                }
            }

            // Process deferred task stops
            for (NameId name : m_deferred_task_stops)
            {
                for (auto &t : m_tasks)
                    if (t.name == name)
                    {
                        t.active = false;
                        m_tasks_dirty = true;
                    }
            }
            m_deferred_task_stops.clear();

            // End-of-frame: clear queues
            for (auto &r : m_queues)
                if (r.queue)
                    r.queue->clear();

            m_stepping = false;
        }

        // ====== IDS — remove() is always deferred to end-of-frame ======
        Id spawn(NameId name = NULL_NAME)
        {
            uint32_t idx;
            if (!m_free_ids.empty())
            {
                idx = m_free_ids.back();
                m_free_ids.pop_back();
            }
            else
            {
                idx = static_cast<uint32_t>(m_id_gen.size());
                m_id_gen.push_back(1);
                m_removing.push_back(0);
                m_id_to_name.push_back(NULL_NAME);
            }
            Id id = make_id(idx, m_id_gen[idx]);
            if (name != NULL_NAME)
            {
                m_name_to_id[name] = id;
                m_id_to_name[idx] = name;
            }
            return id;
        }

        Id spawn(const char *name)
        {
            if (!name || !name[0])
                return spawn(NULL_NAME);
            return spawn(intern(name));
        }

        Id find(NameId name) const
        {
            auto it = m_name_to_id.find(name);
            return it != m_name_to_id.end() ? it->second : NULL_ID;
        }

        Id find(const char *name) const
        {
            NameId nid = m_names.find(name);
            if (nid == NULL_NAME)
                return NULL_ID;
            return find(nid);
        }

        void remove(Id id)
        {
            uint32_t idx = id_index(id);
            if (idx >= m_id_gen.size() || m_id_gen[idx] != id_generation(id))
                return;
            if (m_removing[idx] == 0)
            {
                m_removing[idx] = 1;
                m_deferred_removes.push_back(id);
            }
        }

        bool is_removing(Id id) const
        {
            uint32_t idx = id_index(id);
            if (idx >= m_id_gen.size() || m_id_gen[idx] != id_generation(id))
                return true;
            if (idx < m_removing.size())
                return m_removing[idx] == 1;
            return false;
        }

        void sync_id(Id id)
        {
            uint32_t idx = id_index(id);
            uint32_t gen = id_generation(id);

            if (idx > 1000000)
                return;

            if (idx < m_id_gen.size() && m_id_gen[idx] == gen)
                return;

            if (idx >= m_id_gen.size())
            {
                uint32_t old_size = static_cast<uint32_t>(m_id_gen.size());
                m_id_gen.resize(idx + 1, 0);
                m_removing.resize(idx + 1, 0);
                m_id_to_name.resize(idx + 1, NULL_NAME);

                for (uint32_t i = old_size; i < idx; ++i)
                    m_free_ids.push_back(i);
            }

            m_id_gen[idx] = gen;

            for (size_t i = 0; i < m_free_ids.size(); i++)
            {
                if (m_free_ids[i] == idx)
                {
                    m_free_ids[i] = m_free_ids.back();
                    m_free_ids.pop_back();
                    break;
                }
            }
        }

        template <typename T>
        Handle<T> handle(NameId pool_name, Id id)
        {
            return {id, pool<T>(pool_name)};
        }

        template <typename T>
        Handle<T> handle(const char *pool_name, Id id)
        {
            return handle<T>(intern(pool_name), id);
        }

        // ====== SCHEDULING ======
        template <typename F>
        void schedule(NameId name, float priority, F &&fn, float hz = 0.f, RunOn run_on = RunOn::All, bool pauseable = false)
        {
            Task t;
            t.name = name;
            t.priority = priority;
            t.hz = hz;
            t.run_on = run_on;
            t.pauseable = pauseable;

            t.fn = [f = std::forward<F>(fn)](Ctx &ctx) -> Result
            {
                using Ret = decltype(f(ctx));
                if constexpr (std::is_same_v<Ret, Result>)
                    return f(ctx);
                else
                {
                    f(ctx);
                    return Result::ok();
                }
            };
            m_tasks.push_back(std::move(t));
            m_tasks_dirty = true;
        }

        template <typename F>
        void schedule(const char *name, float priority, F &&fn, float hz = 0.f, RunOn run_on = RunOn::All, bool pauseable = false)
        {
            schedule(intern(name), priority, std::forward<F>(fn), hz, run_on, pauseable);
        }

        const std::vector<Task> &tasks() const { return m_tasks; }

        Task *task(NameId name)
        {
            for (auto &t : m_tasks)
                if (t.name == name)
                    return &t;
            return nullptr;
        }
        Task *task(const char *name) { return task(intern(name)); }

        void stop_task(NameId name)
        {
            for (auto &t : m_tasks)
                if (t.name == name)
                {
                    t.active = false;
                    m_tasks_dirty = true;
                }
        }
        void stop_task(const char *name) { stop_task(intern(name)); }

        void stop_task_deferred(NameId name) { m_deferred_task_stops.push_back(name); }
        void stop_task_deferred(const char *name) { stop_task_deferred(intern(name)); }

        // ====== POOLS — pure data, T is always the component type ======
        template <typename T>
        Pool<T> *pool(NameId name)
        {
            if (name == NULL_NAME) return nullptr;
            if (name >= m_pools.size()) m_pools.resize(name + 1);
            auto &r = m_pools[name];
            if (!r.pool)
            {
                auto *p = new Pool<T>();
                p->m_name = name;
                p->pool_id = m_next_pool_id++;
                p->m_peer_slots = &m_peer_slots;
                p->m_self_id = &m_peer_id;
                r.pool = p;
                r.type_id = get_type_id<T>();

                if (p->pool_id >= m_pool_by_id.size())
                    m_pool_by_id.resize(p->pool_id + 1, nullptr);
                m_pool_by_id[p->pool_id] = p;
            }
            assert(r.type_id == get_type_id<T>() && "Pool type mismatch! Same name used with different types");
            return static_cast<Pool<T> *>(r.pool);
        }

        template <typename T>
        Pool<T> *pool(const char *name) { return pool<T>(intern(name)); }

        // ====== QUEUES — pure data, Pm clears each frame ======
        template <typename T>
        Queue<T> *queue(NameId name)
        {
            if (name == NULL_NAME) return nullptr;
            if (name >= m_queues.size()) m_queues.resize(name + 1);
            auto &r = m_queues[name];
            if (!r.queue)
            {
                auto *q = new Queue<T>();
                q->m_name = name;
                r.queue = q;
                r.type_id = get_type_id<T>();
            }
            assert(r.type_id == get_type_id<T>() && "Queue type mismatch! Same name used with different types");
            return static_cast<Queue<T> *>(r.queue);
        }

        template <typename T>
        Queue<T> *queue(const char *name) { return queue<T>(intern(name)); }

        // ====== SYSTEMS — the only thing with lifecycle ======
        template <typename T>
        T *sys(NameId name)
        {
            static_assert(std::is_base_of_v<System, T>, "System must inherit from pm::System");
            if (name == NULL_NAME) return nullptr;
            if (name >= m_sys.size()) m_sys.resize(name + 1);
            auto &r = m_sys[name];
            if (!r.sys)
            {
                auto *s = new T();
                s->m_name = name;
                s->m_name_str = name_str(name);
                r.sys = s;
                r.type_id = get_type_id<T>();
                m_systems.push_back(s);
                s->initialize(*this);
            }
            assert(r.type_id == get_type_id<T>() && "System type mismatch! Same name used with different types");
            return static_cast<T *>(r.sys);
        }

        template <typename T>
        T *sys(const char *name) { return sys<T>(intern(name)); }

        // ====== TEARDOWN ======
        // drop() is entity + task only. Pools/Systems/Queues live for kernel lifetime.
        // Use pool->reset() or queue->reset() to clear data without destroying infrastructure.
        void drop(NameId name)
        {
            Id id = find(name);
            if (id != NULL_ID)
                remove(id);
            stop_task_deferred(name);
        }

        void drop(const char *name) { drop(intern(name)); }
    };

    // =============================================================================
    // Ctx inline implementations — defined after Pm so we can forward calls
    // =============================================================================
    inline Pm &Ctx::pm() { return *m_pm; }
    inline float Ctx::dt() const { return m_pm->dt(); }
    inline uint64_t Ctx::tick_count() const { return m_pm->tick_count(); }
    inline bool Ctx::is_host() const { return m_pm->is_host(); }
    inline uint8_t Ctx::peer_id() const { return m_pm->peer_id(); }
    inline uint64_t Ctx::peer_slots() const { return m_pm->peer_slots(); }
    inline uint8_t Ctx::peer_count() const { return m_pm->peer_count(); }
    inline Id Ctx::spawn(NameId name) { return m_pm->spawn(name); }
    inline Id Ctx::spawn(const char *name) { return m_pm->spawn(name); }
    inline void Ctx::remove(Id id) { m_pm->remove(id); }
    inline bool Ctx::is_removing(Id id) const { return m_pm->is_removing(id); }
    inline void Ctx::sync_id(Id id) { m_pm->sync_id(id); }
    inline void Ctx::quit() { m_pm->quit(); }
    inline bool Ctx::is_running() const { return m_pm->is_running(); }
    inline bool Ctx::is_paused() const { return m_pm->is_paused(); }
    inline void Ctx::pause() { m_pm->pause(); }
    inline void Ctx::resume() { m_pm->resume(); }
    inline void Ctx::toggle_pause() { m_pm->toggle_pause(); }
    inline const std::vector<std::string> &Ctx::faults() const { return m_pm->faults(); }
    inline bool Ctx::stepping() const { return m_pm->stepping(); }
    inline PeerRange Ctx::peers() const { return m_pm->peers(); }
    inline PeerRange Ctx::remote_peers() const { return m_pm->remote_peers(); }
    inline Peer &Ctx::peer(uint8_t id) { return m_pm->peer(id); }
    inline const Peer &Ctx::peer(uint8_t id) const { return m_pm->peer(id); }

    template <typename T>
    Queue<T> *Ctx::queue(NameId name) { return m_pm->queue<T>(name); }

    template <typename T>
    Queue<T> *Ctx::queue(const char *name) { return m_pm->queue<T>(name); }

} // namespace pm
