// pm_core.hpp — Process Manager kernel (pure ECS)
//
// THE RULE OF 3 VERBS:
// 1. Data: pm.pool<T>(), pm.state<T>()
// 2. Ids: pm.spawn(), pm.find(name), pm.remove_entity(id)
// 3. Scheduling: pm.schedule(name, priority, fn)
//
// INIT PATTERN:
// Plain functions register tasks, pools, and state during setup.
// States are named singletons — no base class, no lifecycle methods.
//
// NETWORKING:
// pm_core knows nothing about peers, sync, or transport.
// See pm_udp.hpp for the network sync layer built on top of this kernel.

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
#include <typeinfo>
#include <stdexcept>

// =============================================================================
// Platform intrinsics
// =============================================================================
#if defined(_MSC_VER) && !defined(__clang__)
#include <intrin.h>
inline uint32_t pm_ctz64(uint64_t mask)
{
    unsigned long index;
    _BitScanForward64(&index, mask);
    return static_cast<uint32_t>(index);
}
inline uint32_t pm_popcnt64(uint64_t mask)
{
    return static_cast<uint32_t>(__popcnt64(mask));
}
#else
inline uint32_t pm_ctz64(uint64_t mask)
{
    return static_cast<uint32_t>(__builtin_ctzll(mask));
}
inline uint32_t pm_popcnt64(uint64_t mask)
{
    return static_cast<uint32_t>(__builtin_popcountll(mask));
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
    std::unordered_map<std::string, NameId, StringHash, std::equal_to<>> to_id;
    std::vector<std::string> to_str;

public:
    NameId intern(const char *s)
    {
        if (!s || !s[0])
            return NULL_NAME;
        auto it = to_id.find(s);
        if (it != to_id.end())
            return it->second;
        NameId id = static_cast<NameId>(to_str.size());
        to_id.emplace(s, id);
        to_str.push_back(s);
        return id;
    }

    NameId find(const char *s) const
    {
        if (!s || !s[0])
            return NULL_NAME;
        auto it = to_id.find(s);
        return it != to_id.end() ? it->second : NULL_NAME;
    }

    const char *str(NameId id) const
    {
        if (id == NULL_NAME || id >= to_str.size())
            return "";
        return to_str[id].c_str();
    }
};

struct Result
{
    const char *error = nullptr;
    static Result ok() { return {nullptr}; }
    static Result err(const char *msg) { return {msg}; }
    explicit operator bool() const { return error == nullptr; }
};

// =============================================================================
// Type IDs — cross-SO safe via typeid name hash
// =============================================================================
template <typename T>
inline size_t get_type_id()
{
    static const size_t id = std::hash<std::string_view>{}(typeid(T).name());
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
    T *operator->() const { return get(); }
    T &operator*() const { return *get(); }
    explicit operator bool() const { return get() != nullptr; }
};

// =============================================================================
// Core Interfaces
// =============================================================================
class Pm;

// PoolBase — type-erased base for Pool<T>.
// Lets Pm manage pools during entity removal without knowing T.
class PoolBase
{
public:
    virtual ~PoolBase() = default;
    uint32_t pool_id = 0;
    virtual void remove(Id id) = 0;
    virtual void remove_by_index(uint32_t idx) = 0;
    virtual Id id_at_index(uint32_t idx) const = 0;
    virtual void clear_all() = 0;
    virtual void shrink_to_fit() = 0;
};

// =============================================================================
// Pool<T> — Contiguous DOD Sparse Set
//
// Pure data container. No lifecycle methods.
// Created via pm.pool<T>("name"). Pm wires up entity→pool removal.
//
// Optional change hook: set via set_change_hook() to get notified when
// entries are added or modified. Used by the network layer (pm_udp) to
// track dirty entries without Pool knowing about sync bitmasks.
// =============================================================================

template <typename T>
class Pool : public PoolBase
{
public:
    std::vector<T> items;
    std::vector<Id> dense_ids;
    std::vector<uint32_t> sparse_indices;

    // --- Change notification hook (optional) ---
    using ChangeHook = void(*)(void*, Id);
    ChangeHook change_fn = nullptr;
    void *change_ctx = nullptr;

    // --- Swap-remove notification hook (optional) ---
    // Fires during remove() after the swap. Observer mirrors the swap
    // on its own dense-parallel arrays.
    // Args: (ctx, removed_dense_idx, last_dense_idx_before_pop)
    using SwapHook = void(*)(void*, uint32_t, uint32_t);
    SwapHook swap_fn = nullptr;
    void *swap_ctx = nullptr;

    void set_change_hook(ChangeHook fn, void *data)
    {
        change_fn = fn;
        change_ctx = data;
    }

    void set_swap_hook(SwapHook fn, void *data)
    {
        swap_fn = fn;
        swap_ctx = data;
    }

    void notify_change(Id id)
    {
        if (change_fn) change_fn(change_ctx, id);
    }

    // --- Sparse set operations ---

    T *add(Id id, T val = T{})
    {
        uint32_t idx = id_index(id);
        if (idx >= sparse_indices.size())
            sparse_indices.resize(idx + 1, NULL_INDEX);

        if (sparse_indices[idx] != NULL_INDEX)
        {
            uint32_t dense_idx = sparse_indices[idx];
            dense_ids[dense_idx] = id;
            items[dense_idx] = std::move(val);
            notify_change(id);
            return &items[dense_idx];
        }

        uint32_t dense_idx = static_cast<uint32_t>(items.size());
        sparse_indices[idx] = dense_idx;
        dense_ids.push_back(id);
        items.push_back(std::move(val));

        notify_change(id);
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

        uint32_t last_dense_idx = static_cast<uint32_t>(items.size() - 1);
        Id last_id = dense_ids[last_dense_idx];

        if (swap_fn) swap_fn(swap_ctx, dense_idx, last_dense_idx);

        if (dense_idx != last_dense_idx)
        {
            items[dense_idx] = std::move(items[last_dense_idx]);
            dense_ids[dense_idx] = last_id;
            sparse_indices[id_index(last_id)] = dense_idx;
        }

        sparse_indices[idx] = NULL_INDEX;
        items.pop_back();
        dense_ids.pop_back();
    }

    void remove_by_index(uint32_t idx) override
    {
        if (idx >= sparse_indices.size() || sparse_indices[idx] == NULL_INDEX)
            return;
        uint32_t dense_idx = sparse_indices[idx];
        remove(dense_ids[dense_idx]);
    }

    Id id_at_index(uint32_t idx) const override
    {
        if (idx >= sparse_indices.size() || sparse_indices[idx] == NULL_INDEX)
            return NULL_ID;
        return dense_ids[sparse_indices[idx]];
    }

    void clear_all() override
    {
        for (Id id : dense_ids)
            sparse_indices[id_index(id)] = NULL_INDEX;
        items.clear();
        dense_ids.clear();
    }

    void shrink_to_fit() override
    {
        items.shrink_to_fit();
        dense_ids.shrink_to_fit();
    }

    void reset()
    {
        clear_all();
        sparse_indices.clear();
        sparse_indices.shrink_to_fit();
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
    size_t size() const { return items.size(); }

    Handle<T> handle(Id id) { return {id, this}; }

    // --- Iteration ---

    struct Entry
    {
        Id id;
        T &value;
        Pool<T> *pool;
        T *modify()
        {
            pool->notify_change(id);
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
        Iterator &operator++() { ++i; return *this; }
        bool operator!=(const Iterator &o) const { return i != o.i; }
    };
    struct ConstIterator
    {
        const Pool *p;
        size_t i;
        ConstEntry operator*() const { return {p->dense_ids[i], p->items[i]}; }
        ConstIterator &operator++() { ++i; return *this; }
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
};

// =============================================================================
// StateBase — type-erased base for named singletons
// =============================================================================
class StateBase
{
public:
    virtual ~StateBase() = default;
};

template <typename T>
class StateHolder : public StateBase
{
public:
    T value{};
};

// =============================================================================
// TaskContext — Task execution context
//
// Exposes the runtime API tasks need each frame. Intentionally omits
// init-time operations (pool<T>, state<T>, schedule) to guide
// users toward capturing pointers during init functions.
//
// For admin tasks (level loading, mod init), use ctx.pm() to escape.
// =============================================================================
class Pm;
class TaskContext
{
    Pm *owner;
public:
    explicit TaskContext(Pm &pm) : owner(&pm) {}

    Pm &pm();

    float dt() const;
    uint64_t tick_count() const;

    Id spawn(NameId name = NULL_NAME);
    Id spawn(const char *name);
    Id find(NameId name) const;
    Id find(const char *name) const;
    void remove_entity(Id id);
    bool is_removing_entity(Id id) const;
    bool sync_id(Id id);

    void quit();
    bool is_running() const;
    bool is_paused() const;
    void pause();
    void resume();
    void toggle_pause();

    const std::vector<std::string> &faults() const;
    bool stepping() const;
};

// =============================================================================
// Task
// =============================================================================
struct Task
{
    NameId name = NULL_NAME;
    float priority = 0, hz = 0, accum = 0;
    bool active = true;
    bool pauseable = false;

    std::function<Result(TaskContext &)> fn;

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
// Phase — Suggested priority constants for common task ordering
// =============================================================================
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

// =============================================================================
// Pm Kernel
// =============================================================================
class Pm
{
    NameTable name_table;

    std::vector<uint32_t> id_gen;
    std::vector<uint32_t> free_ids;
    std::vector<uint8_t> slot_free;
    std::vector<NameId> id_to_name;
    std::vector<Id> name_to_id;

    std::vector<uint8_t> slot_removing;
    std::vector<Id> deferred_removes;
    size_t remove_cursor = 0;
    float remove_budget = 0.f;

    std::vector<Task> task_list;
    std::vector<uint16_t> task_order_indices;
    bool tasks_dirty = false;

    struct PoolEntry
    {
        PoolBase *pool = nullptr;
        size_t type_id = 0;
    };
    std::vector<PoolEntry> pool_entries;
    std::vector<PoolBase *> pool_by_id;
    uint32_t next_pool_id = 0;

    struct StateEntry
    {
        StateBase *state = nullptr;
        size_t type_id = 0;
    };
    std::vector<StateEntry> state_entries;

    float loop_rate = 0.f, raw_dt = 0.f;
    uint64_t tick = 0;
    bool running = true, stepping_active = false, paused = false, step_requested = false;
    NameId active_task = NULL_NAME;
    std::chrono::steady_clock::time_point last_time = std::chrono::steady_clock::now();
    std::vector<std::string> fault_list;

    uint32_t spawns_this_frame = 0;
    uint32_t removes_this_frame = 0;
    uint32_t alive_count = 0;

    static constexpr uint32_t MAX_SYNC_INDEX = 1000000;

public:
    Pm() = default;
    Pm(const Pm &) = delete;
    Pm &operator=(const Pm &) = delete;
    Pm(Pm &&) = delete;
    Pm &operator=(Pm &&) = delete;

    ~Pm()
    {
        for (auto &r : state_entries)
            delete r.state;
        for (auto &r : pool_entries)
            delete r.pool;
    }

    // ====== NAME INTERNING ======
    NameId intern(const char *s) { return name_table.intern(s); }
    const char *name_str(NameId id) const { return name_table.str(id); }
    NameTable &names() { return name_table; }

    // ====== CORE TIMELINE ======
    void set_loop_rate(float hz) { loop_rate = hz; }
    void set_remove_budget_us(float us) { remove_budget = us; }
    float remove_budget_us() const { return remove_budget; }
    size_t remove_pending() const { return deferred_removes.size() - remove_cursor; }

    uint32_t frame_spawns() const { return spawns_this_frame; }
    uint32_t frame_removes() const { return removes_this_frame; }
    uint32_t entity_count() const { return alive_count; }
    void quit() { running = false; }
    bool is_running() const { return running; }
    float dt() const { return raw_dt; }
    uint64_t tick_count() const { return tick; }
    uint64_t *tick_ptr() { return &tick; }

    const std::vector<std::string> &faults() const { return fault_list; }
    bool stepping() const { return stepping_active; }
    bool is_paused() const { return paused; }
    void pause() { paused = true; }
    void resume() { paused = false; }
    void toggle_pause() { paused = !paused; }
    void request_step() { step_requested = true; }
    NameId current_task() const { return active_task; }

    // Run the main loop until quit() is called.
    void run()
    {
        while (running)
            tick_once();
    }

    // Execute one frame: sleep to rate, run tasks, process removes.
    void tick_once()
    {
        auto now = std::chrono::steady_clock::now();

        if (loop_rate > 0.f)
        {
            float target_us = 1e6f / loop_rate;
            float elapsed_us = std::chrono::duration<float, std::micro>(now - last_time).count();
            if (elapsed_us < target_us)
            {
                auto sleep_us = (int)(target_us - elapsed_us);
                if (sleep_us > 0)
                    std::this_thread::sleep_for(std::chrono::microseconds(sleep_us));
            }
            now = std::chrono::steady_clock::now();
        }

        raw_dt = std::min(std::chrono::duration<float>(now - last_time).count(), 0.1f);
        last_time = now;
        tick++;
        spawns_this_frame = 0;
        removes_this_frame = 0;

        stepping_active = step_requested;
        step_requested = false;

        if (tasks_dirty)
        {
            task_order_indices.clear();
            for (uint16_t i = 0; i < (uint16_t)task_list.size(); i++)
                if (task_list[i].active)
                    task_order_indices.push_back(i);
            std::sort(task_order_indices.begin(), task_order_indices.end(),
                [this](uint16_t a, uint16_t b) { return task_list[a].priority < task_list[b].priority; });
            tasks_dirty = false;
        }

        for (uint16_t oi = 0; oi < (uint16_t)task_order_indices.size(); ++oi)
        {
            uint16_t ti = task_order_indices[oi];
            // Access task_list[ti] by index throughout — never hold a reference
            // across a task call, because schedule()/push_back may reallocate.
            if (!task_list[ti].active || !task_list[ti].fn)
                continue;
            if (task_list[ti].pauseable && paused && !stepping_active)
                continue;

            auto exec_t = [&, ti]()
            {
                auto t0 = std::chrono::steady_clock::now();
                TaskContext ctx(*this);
                Result r = task_list[ti].fn(ctx);
                task_list[ti].record(std::chrono::duration_cast<std::chrono::microseconds>(std::chrono::steady_clock::now() - t0).count());
                if (!r)
                {
                    task_list[ti].active = false;
                    fault_list.push_back(std::string(name_str(task_list[ti].name)) + ": " + r.error);
                    tasks_dirty = true;
                }
            };

            if (task_list[ti].hz > 0)
            {
                task_list[ti].accum += raw_dt;
                float interval = 1.0f / task_list[ti].hz;
                if (task_list[ti].accum > interval * 5.0f)
                    task_list[ti].accum = interval * 5.0f;
                while (task_list[ti].accum >= interval && task_list[ti].active)
                {
                    task_list[ti].accum -= interval;
                    active_task = task_list[ti].name;
                    exec_t();
                    active_task = NULL_NAME;
                }
            }
            else
            {
                active_task = task_list[ti].name;
                exec_t();
                active_task = NULL_NAME;
            }
        }

        // Process deferred entity removes (time-budgeted, cursor-based)
        {
            auto remove_start = std::chrono::steady_clock::now();
            size_t total = deferred_removes.size();

            while (remove_cursor < total)
            {
                Id id = deferred_removes[remove_cursor];
                uint32_t idx = id_index(id);

                if (idx < id_gen.size() && id_gen[idx] == id_generation(id))
                {
                    if (idx < id_to_name.size() && id_to_name[idx] != NULL_NAME)
                    {
                        NameId nid = id_to_name[idx];
                        if (nid < name_to_id.size() && name_to_id[nid] == id)
                            name_to_id[nid] = NULL_ID;
                        id_to_name[idx] = NULL_NAME;
                    }

                    id_gen[idx]++;
                    if (id_gen[idx] == 0)
                        id_gen[idx] = 1;

                    slot_free[idx] = 1;
                    free_ids.push_back(idx);
                    slot_removing[idx] = 0;

                    uint32_t current_gen = id_gen[idx];
                    for (PoolBase *p : pool_by_id)
                    {
                        if (!p) continue;
                        p->remove(id);
                        Id pid = p->id_at_index(idx);
                        if (pid != NULL_ID && id_generation(pid) < current_gen)
                            p->remove_by_index(idx);
                    }
                    removes_this_frame++;
                    alive_count--;
                }
                else if (idx < id_gen.size() && id_gen[idx] > id_generation(id))
                {
                    uint32_t current_gen = id_gen[idx];
                    for (PoolBase *p : pool_by_id)
                    {
                        if (!p) continue;
                        Id pid = p->id_at_index(idx);
                        if (pid != NULL_ID && id_generation(pid) < current_gen)
                            p->remove_by_index(idx);
                    }
                    slot_removing[idx] = 0;
                }

                ++remove_cursor;

                if (remove_budget > 0.f && (remove_cursor & 15) == 0)
                {
                    float elapsed = std::chrono::duration<float, std::micro>(
                                        std::chrono::steady_clock::now() - remove_start)
                                        .count();
                    if (elapsed >= remove_budget)
                        break;
                }
            }

            if (remove_cursor == total)
            {
                deferred_removes.clear();
                remove_cursor = 0;
            }
        }

        stepping_active = false;
    }

    // ====== IDS ======
    Id spawn(NameId name = NULL_NAME)
    {
        uint32_t idx = UINT32_MAX;
        while (!free_ids.empty())
        {
            uint32_t candidate = free_ids.back();
            free_ids.pop_back();
            if (slot_free[candidate])
            {
                slot_free[candidate] = 0;
                idx = candidate;
                break;
            }
        }
        if (idx == UINT32_MAX)
        {
            idx = static_cast<uint32_t>(id_gen.size());
            id_gen.push_back(1);
            slot_free.push_back(0);
            slot_removing.push_back(0);
            id_to_name.push_back(NULL_NAME);
        }
        Id id = make_id(idx, id_gen[idx]);
        spawns_this_frame++;
        alive_count++;
        if (name != NULL_NAME)
        {
            if (name >= name_to_id.size())
                name_to_id.resize(name + 1, NULL_ID);
            name_to_id[name] = id;
            id_to_name[idx] = name;
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
        if (name == NULL_NAME || name >= name_to_id.size())
            return NULL_ID;
        return name_to_id[name];
    }

    Id find(const char *name) const
    {
        NameId nid = name_table.find(name);
        if (nid == NULL_NAME)
            return NULL_ID;
        return find(nid);
    }

    void remove_entity(Id id)
    {
        uint32_t idx = id_index(id);
        if (idx >= id_gen.size() || id_gen[idx] != id_generation(id))
            return;
        if (slot_removing[idx] == 0)
        {
            slot_removing[idx] = 1;
            deferred_removes.push_back(id);
        }
    }

    bool is_removing_entity(Id id) const
    {
        uint32_t idx = id_index(id);
        if (idx >= id_gen.size() || id_gen[idx] != id_generation(id))
            return true;
        if (idx < slot_removing.size())
            return slot_removing[idx] == 1;
        return false;
    }

    bool sync_id(Id id)
    {
        uint32_t idx = id_index(id);
        uint32_t gen = id_generation(id);

        if (idx > MAX_SYNC_INDEX)
            return false;

        if (idx >= id_gen.size())
        {
            uint32_t old_size = static_cast<uint32_t>(id_gen.size());
            id_gen.resize(idx + 1, 0);
            slot_free.resize(idx + 1, 1);
            slot_removing.resize(idx + 1, 0);
            id_to_name.resize(idx + 1, NULL_NAME);

            for (uint32_t i = old_size; i < idx; ++i)
                free_ids.push_back(i);
        }

        if (id_gen[idx] > gen)
            return false;

        if (id_gen[idx] == gen && idx < slot_removing.size() && slot_removing[idx])
            return false;

        id_gen[idx] = gen;
        slot_removing[idx] = 0;
        if (slot_free[idx]) alive_count++;
        slot_free[idx] = 0;
        return true;
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
    void schedule(NameId name, float priority, F &&fn, float hz = 0.f, bool pauseable = false)
    {
        Task t;
        t.name = name;
        t.priority = priority;
        t.hz = hz;
        t.pauseable = pauseable;

        t.fn = [f = std::forward<F>(fn)](TaskContext &ctx) -> Result
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
        task_list.push_back(std::move(t));
        tasks_dirty = true;
    }

    template <typename F>
    void schedule(const char *name, float priority, F &&fn, float hz = 0.f, bool pauseable = false)
    {
        schedule(intern(name), priority, std::forward<F>(fn), hz, pauseable);
    }

    const std::vector<Task> &tasks() const { return task_list; }
    const std::vector<uint16_t> &task_order() const { return task_order_indices; }

    Task *task(NameId name)
    {
        for (auto &t : task_list)
            if (t.name == name)
                return &t;
        return nullptr;
    }
    Task *task(const char *name) { return task(intern(name)); }

    void reset_task_stats()
    {
        for (auto &t : task_list)
        {
            t.max_us = 0;
            t.runs = 0;
        }
    }

    void stop_task(NameId name)
    {
        for (auto &t : task_list)
            if (t.name == name)
            {
                t.active = false;
                tasks_dirty = true;
            }
    }
    void stop_task(const char *name) { stop_task(intern(name)); }

    void unschedule(NameId name)
    {
        for (auto &t : task_list)
            if (t.name == name)
            {
                t.fn = {};
                t.active = false;
                tasks_dirty = true;
            }
    }
    void unschedule(const char *name) { unschedule(intern(name)); }

    // ====== POOLS ======
    template <typename T>
    Pool<T> *pool(NameId name)
    {
        if (name == NULL_NAME) return nullptr;
        if (name >= pool_entries.size()) pool_entries.resize(name + 1);
        auto &r = pool_entries[name];
        if (!r.pool)
        {
            auto *p = new Pool<T>();
            p->pool_id = next_pool_id++;
            r.pool = p;
            r.type_id = get_type_id<T>();

            if (p->pool_id >= pool_by_id.size())
                pool_by_id.resize(p->pool_id + 1, nullptr);
            pool_by_id[p->pool_id] = p;
        }
        assert(r.type_id == get_type_id<T>() && "Pool type mismatch! Same name used with different types");
        return static_cast<Pool<T> *>(r.pool);
    }

    template <typename T>
    Pool<T> *pool(const char *name) { return pool<T>(intern(name)); }

    // ====== STATES — Named singletons ======
    template <typename T>
    T *state(NameId name)
    {
        if (name == NULL_NAME) return nullptr;
        if (name >= state_entries.size()) state_entries.resize(name + 1);
        auto &r = state_entries[name];
        if (!r.state)
        {
            auto *h = new StateHolder<T>();
            r.state = h;
            r.type_id = get_type_id<T>();
        }
        assert(r.type_id == get_type_id<T>() && "State type mismatch! Same name used with different types");
        return &static_cast<StateHolder<T> *>(r.state)->value;
    }

    template <typename T>
    T *state(const char *name) { return state<T>(intern(name)); }
};

// =============================================================================
// TaskContext inline implementations
// =============================================================================
inline Pm &TaskContext::pm() { return *owner; }
inline float TaskContext::dt() const { return owner->dt(); }
inline uint64_t TaskContext::tick_count() const { return owner->tick_count(); }
inline Id TaskContext::spawn(NameId name) { return owner->spawn(name); }
inline Id TaskContext::spawn(const char *name) { return owner->spawn(name); }
inline Id TaskContext::find(NameId name) const { return owner->find(name); }
inline Id TaskContext::find(const char *name) const { return owner->find(name); }
inline void TaskContext::remove_entity(Id id) { owner->remove_entity(id); }
inline bool TaskContext::is_removing_entity(Id id) const { return owner->is_removing_entity(id); }
inline bool TaskContext::sync_id(Id id) { return owner->sync_id(id); }
inline void TaskContext::quit() { owner->quit(); }
inline bool TaskContext::is_running() const { return owner->is_running(); }
inline bool TaskContext::is_paused() const { return owner->is_paused(); }
inline void TaskContext::pause() { owner->pause(); }
inline void TaskContext::resume() { owner->resume(); }
inline void TaskContext::toggle_pause() { owner->toggle_pause(); }
inline const std::vector<std::string> &TaskContext::faults() const { return owner->faults(); }
inline bool TaskContext::stepping() const { return owner->stepping(); }

} // namespace pm