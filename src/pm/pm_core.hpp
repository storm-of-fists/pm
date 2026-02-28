// pm_core.hpp — Process Manager kernel (pure ECS)
//
// THE RULE OF 3 VERBS:
// 1. Data: pm.pool_get<T>(), pm.state_get<T>()
// 2. Ids: pm.id_add(), pm.id_remove(id)
// 3. Scheduling: pm.task_add(name, priority, fn)
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
#include <mutex>
#include <condition_variable>
#include <atomic>
#include <cstdint>
#include <type_traits>
#include <typeinfo>
#include <stdexcept>
#include <memory>

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

struct TaskFault : std::exception
{
	std::string msg;
	explicit TaskFault(std::string m) : msg(std::move(m)) {}
	const char *what() const noexcept override { return msg.c_str(); }
};

// =============================================================================
// Parallel — execution mode for Pool<T>::each()
// =============================================================================
enum class Parallel { Auto, Off, On };

// =============================================================================
// ThreadPool — barrier-based chunk dispatch for parallel iteration
// =============================================================================
struct ThreadPool
{
	std::vector<std::thread> workers;
	std::mutex mtx;
	std::condition_variable cv;
	std::condition_variable done_cv;

	std::function<void(size_t, size_t)> work_fn;
	size_t work_total = 0;
	uint32_t work_gen = 0;
	uint32_t work_active = 0; // how many workers should do actual work this dispatch

	std::atomic<uint32_t> remaining{0};
	std::exception_ptr captured_exception;
	bool shutdown = false;
	uint32_t num_threads = 0;

	void start(uint32_t n)
	{
		num_threads = n;
		for (uint32_t i = 0; i < n; i++)
			workers.emplace_back([this, i]() { run(i); });
	}

	~ThreadPool()
	{
		{ std::lock_guard<std::mutex> lk(mtx); shutdown = true; }
		cv.notify_all();
		for (auto &w : workers) w.join();
	}

	// use_threads: how many workers to use. 0 = all.
	void dispatch(size_t count, std::function<void(size_t, size_t)> fn, uint32_t use_threads = 0)
	{
		uint32_t active = (use_threads == 0 || use_threads >= num_threads)
			? num_threads : use_threads;
		captured_exception = nullptr;
		{
			std::lock_guard<std::mutex> lk(mtx);
			work_fn = std::move(fn);
			work_total = count;
			work_active = active;
			remaining.store(num_threads, std::memory_order_relaxed);
			work_gen++;
		}
		cv.notify_all();

		std::unique_lock<std::mutex> lk(mtx);
		done_cv.wait(lk, [&] { return remaining.load(std::memory_order_acquire) == 0; });

		if (captured_exception)
		{
			auto e = captured_exception;
			captured_exception = nullptr;
			std::rethrow_exception(e);
		}
	}

private:
	void run(uint32_t id)
	{
		uint32_t local_gen = 0;
		while (true)
		{
			std::unique_lock<std::mutex> lk(mtx);
			cv.wait(lk, [&] { return shutdown || work_gen != local_gen; });
			if (shutdown) return;
			local_gen = work_gen;

			size_t total = work_total;
			uint32_t active = work_active;
			auto fn = work_fn;
			lk.unlock();

			if (id < active)
			{
				size_t chunk = (total + active - 1) / active;
				size_t begin = std::min(static_cast<size_t>(id) * chunk, total);
				size_t end = std::min(begin + chunk, total);

				try
				{
					if (begin < end) fn(begin, end);
				}
				catch (...)
				{
					std::lock_guard<std::mutex> elk(mtx);
					if (!captured_exception) captured_exception = std::current_exception();
				}
			}

			if (remaining.fetch_sub(1, std::memory_order_acq_rel) == 1)
				done_cv.notify_one();
		}
	}
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
constexpr uint32_t NULL_INDEX = 0x00FFFFFF;

// Id layout: [63..40] 24-bit index, [39..16] 24-bit generation, [15..0] 16-bit flags
inline uint32_t id_index(Id id) { return static_cast<uint32_t>((id >> 40) & 0xFFFFFF); }
inline uint32_t id_generation(Id id) { return static_cast<uint32_t>((id >> 16) & 0xFFFFFF); }
inline uint16_t id_flags(Id id) { return static_cast<uint16_t>(id & 0xFFFF); }
inline Id make_id(uint32_t idx, uint32_t gen, uint16_t flags = 0)
{
	return (static_cast<uint64_t>(idx & 0xFFFFFF) << 40)
		 | (static_cast<uint64_t>(gen & 0xFFFFFF) << 16)
		 | flags;
}

constexpr uint16_t ID_FLAG_FREE = 0x0001;
inline bool id_is_free(Id slot) { return (slot & ID_FLAG_FREE) != 0; }

template <typename T>
class Pool;

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
	const std::vector<Id> *kernel_slots = nullptr; // set by Pm::pool_get<T>()
	virtual void remove(Id id) = 0;
	virtual void clear_all() = 0;
	virtual void shrink_to_fit() = 0;
};

// =============================================================================
// Pool<T> — Contiguous DOD Sparse Set
//
// Pure data container. No lifecycle methods.
// Created via pm.pool_get<T>("name"). Pm wires up entity→pool removal.
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
	std::vector<uint32_t> dense_indices;  // slot index per dense entry
	std::vector<Id> dense_ids;            // full Id per dense entry (cached for O(1) id_at)
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

	// Full Id for a given dense position (O(1) direct lookup)
	Id id_at(size_t dense_idx) const
	{
		return dense_ids[dense_idx];
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
			dense_indices[dense_idx] = idx;
			dense_ids[dense_idx] = id;
			items[dense_idx] = std::move(val);
			notify_change(id);
			return &items[dense_idx];
		}

		uint32_t dense_idx = static_cast<uint32_t>(items.size());
		sparse_indices[idx] = dense_idx;
		dense_indices.push_back(idx);
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
		if (dense_indices[dense_idx] != idx)
			return;

		uint32_t last_dense_idx = static_cast<uint32_t>(items.size() - 1);
		uint32_t last_slot_idx = dense_indices[last_dense_idx];

		if (swap_fn) swap_fn(swap_ctx, dense_idx, last_dense_idx);

		if (dense_idx != last_dense_idx)
		{
			items[dense_idx] = std::move(items[last_dense_idx]);
			dense_indices[dense_idx] = last_slot_idx;
			dense_ids[dense_idx] = dense_ids[last_dense_idx];
			sparse_indices[last_slot_idx] = dense_idx;
		}

		sparse_indices[idx] = NULL_INDEX;
		items.pop_back();
		dense_indices.pop_back();
		dense_ids.pop_back();
	}

	void clear_all() override
	{
		for (uint32_t idx : dense_indices)
			sparse_indices[idx] = NULL_INDEX;
		items.clear();
		dense_indices.clear();
		dense_ids.clear();
	}

	void shrink_to_fit() override
	{
		items.shrink_to_fit();
		dense_indices.shrink_to_fit();
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
		if (idx >= sparse_indices.size() || sparse_indices[idx] == NULL_INDEX)
			return nullptr;
		uint32_t dense_idx = sparse_indices[idx];
		// Validate generation via kernel slots
		if (kernel_slots)
		{
			Id slot = (*kernel_slots)[idx];
			if (id_generation(slot) != id_generation(id) || id_is_free(slot))
				return nullptr;
		}
		return &items[dense_idx];
	}
	const T *get(Id id) const { return const_cast<Pool *>(this)->get(id); }

	bool has(Id id) const { return get(id) != nullptr; }
	size_t size() const { return items.size(); }

	// --- Iteration (lambda form, auto-parallelizes) ---
	// Defined out-of-line after Pm class.
	// each():     void(const T&) or void(Id, const T&) — read-only, no change hooks
	// each_mut(): void(T&) or void(Id, T&) — mutable, fires change hooks
	Pm *kernel = nullptr; // set by Pm::pool_get<T>()

	static constexpr size_t PARALLEL_THRESHOLD = 1024;

	// threads: 0 = use all workers, N = use at most N workers for this call.
	template <typename F>
	void each(F &&fn, Parallel p = Parallel::Auto, uint32_t threads = 0);

	template <typename F>
	void each_mut(F &&fn, Parallel p = Parallel::Auto, uint32_t threads = 0);
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
// Task
// =============================================================================
class Pm;
struct Task
{
	NameId name = NULL_NAME;
	float priority = 0;
	bool active = true;
	bool pauseable = false;

	std::function<void(Pm &)> fn;

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

// =============================================================================
// Pm Kernel
// =============================================================================
class Pm
{
	NameTable _name_table;

	std::vector<Id> _slots;        // One packed Id per entity slot (index, gen, flags)
	std::vector<uint32_t> _free_ids;
	std::vector<Id> _pending_removes;
	std::mutex _remove_mtx;
	std::unique_ptr<ThreadPool> _thread_pool;

	std::vector<uint16_t> _task_order_indices;
	bool _tasks_dirty = false;

	struct PoolEntry
	{
		std::unique_ptr<PoolBase> pool;
		size_t type_id = 0;
	};
	std::vector<PoolEntry> _pool_entries;
	std::vector<PoolBase *> _pool_by_id;
	uint32_t _next_pool_id = 0;

	struct StateEntry
	{
		std::unique_ptr<StateBase> state;
		size_t type_id = 0;
	};
	std::vector<StateEntry> _state_entries;

	float _raw_dt = 0.f;
	uint64_t _tick = 0;
	bool _running = true, _stepping_active = false, _step_requested = false;
	NameId _active_task = NULL_NAME;
	std::chrono::steady_clock::time_point _last_time = std::chrono::steady_clock::now();
	std::vector<std::string> _fault_list;

	uint32_t _spawns_this_frame = 0;
	uint32_t _removes_this_frame = 0;
	uint32_t _alive_count = 0;

	static constexpr uint32_t MAX_SYNC_INDEX = 1000000;

public:
	// --- Public members ---
	std::vector<Task> task_list;
	float loop_rate = 0.f;
	bool paused = false;
	uint32_t thread_count = 0; // 0 = auto (hardware_concurrency). Set before first parallel each.

	Pm() = default;
	Pm(const Pm &) = delete;
	Pm &operator=(const Pm &) = delete;
	Pm(Pm &&) = delete;
	Pm &operator=(Pm &&) = delete;

	~Pm() = default;

	// ====== THREAD POOL (lazy-init) ======

	ThreadPool *thread_pool()
	{
		if (!_thread_pool)
		{
			uint32_t n = thread_count;
			if (n == 0)
			{
				n = std::thread::hardware_concurrency();
				if (n == 0) n = 4;
			}
			else
			{
				uint32_t hw = std::thread::hardware_concurrency();
				if (hw == 0) hw = 4;
				n = std::min(n, hw);
			}
			_thread_pool = std::make_unique<ThreadPool>();
			_thread_pool->start(n);
		}
		return _thread_pool.get();
	}

	// ====== NAME INTERNING ======
	NameId name_new(const char *s) { return _name_table.intern(s); }
	const char *name_get(NameId id) const { return _name_table.str(id); }
	NameTable &name_table() { return _name_table; }

	// ====== CORE TIMELINE ======

	uint32_t loop_spawns() const { return _spawns_this_frame; }
	uint32_t loop_removes() const { return _removes_this_frame; }
	uint32_t id_count() const { return _alive_count; }
	void loop_quit() { _running = false; }
	bool loop_running() const { return _running; }
	float loop_dt() const { return _raw_dt; }
	uint64_t loop_count() const { return _tick; }

	const std::vector<std::string> &task_faults() const { return _fault_list; }
	bool loop_stepping() const { return _stepping_active; }
	void loop_step() { _step_requested = true; }
	NameId task_current() const { return _active_task; }

	// Run the main loop until loop_quit() is called.
	void loop_run()
	{
		while (_running)
			loop_once();
	}

	// Execute one frame: sleep to rate, run tasks, process removes.
	void loop_once()
	{
		auto now = std::chrono::steady_clock::now();

		if (loop_rate > 0.f)
		{
			float target_us = 1e6f / loop_rate;
			float elapsed_us = std::chrono::duration<float, std::micro>(now - _last_time).count();
			if (elapsed_us < target_us)
			{
				auto sleep_us = (int)(target_us - elapsed_us);
				if (sleep_us > 0)
					std::this_thread::sleep_for(std::chrono::microseconds(sleep_us));
			}
			now = std::chrono::steady_clock::now();
		}

		_raw_dt = std::min(std::chrono::duration<float>(now - _last_time).count(), 0.1f);
		_last_time = now;
		_tick++;
		_spawns_this_frame = 0;
		_removes_this_frame = 0;

		_stepping_active = _step_requested;
		_step_requested = false;

		if (_tasks_dirty)
		{
			_task_order_indices.clear();
			for (uint16_t i = 0; i < (uint16_t)task_list.size(); i++)
				if (task_list[i].active)
					_task_order_indices.push_back(i);
			std::stable_sort(_task_order_indices.begin(), _task_order_indices.end(),
				[this](uint16_t a, uint16_t b) { return task_list[a].priority < task_list[b].priority; });
			_tasks_dirty = false;
		}

		for (uint16_t oi = 0; oi < (uint16_t)_task_order_indices.size(); ++oi)
		{
			uint16_t ti = _task_order_indices[oi];
			// Access task_list[ti] by index throughout — never hold a reference
			// across a task call, because task_add()/push_back may reallocate.
			if (!task_list[ti].active || !task_list[ti].fn)
				continue;
			if (task_list[ti].pauseable && paused && !_stepping_active)
				continue;

			auto exec_t = [&, ti]()
			{
				auto t0 = std::chrono::steady_clock::now();
				try
				{
					task_list[ti].fn(*this);
				}
				catch (const TaskFault &e)
				{
					task_list[ti].active = false;
					_fault_list.push_back(std::string(name_get(task_list[ti].name)) + ": " + e.what());
					_tasks_dirty = true;
				}
				task_list[ti].record(std::chrono::duration_cast<std::chrono::microseconds>(std::chrono::steady_clock::now() - t0).count());
			};

			_active_task = task_list[ti].name;
			exec_t();
			_active_task = NULL_NAME;
		}

		id_process_removes();
		_stepping_active = false;
	}

	// ====== IDS ======
	Id id_add()
	{
		uint32_t idx = UINT32_MAX;
		while (!_free_ids.empty())
		{
			uint32_t candidate = _free_ids.back();
			_free_ids.pop_back();
			if (id_is_free(_slots[candidate]))
			{
				idx = candidate;
				break;
			}
		}
		if (idx == UINT32_MAX)
		{
			idx = static_cast<uint32_t>(_slots.size());
			_slots.push_back(make_id(idx, 1, ID_FLAG_FREE));
		}
		uint32_t gen = id_generation(_slots[idx]);
		_slots[idx] = make_id(idx, gen); // clear free flag
		_spawns_this_frame++;
		_alive_count++;
		return make_id(idx, gen);
	}

	void id_remove(Id id)
	{
		std::lock_guard<std::mutex> lk(_remove_mtx);
		_pending_removes.push_back(id);
	}

	uint32_t id_pending_removes() const { return static_cast<uint32_t>(_pending_removes.size()); }

	void id_process_removes()
	{
		for (Id id : _pending_removes)
		{
			uint32_t idx = id_index(id);
			if (idx >= _slots.size()) continue;
			if (id_generation(_slots[idx]) != id_generation(id)) continue;
			if (id_is_free(_slots[idx])) continue;

			uint32_t new_gen = (id_generation(id) + 1) & 0xFFFFFF;
			if (new_gen == 0) new_gen = 1;
			_slots[idx] = make_id(idx, new_gen, ID_FLAG_FREE);

			for (PoolBase *p : _pool_by_id)
			{
				if (p) p->remove(id);
			}

			_free_ids.push_back(idx);
			_removes_this_frame++;
			_alive_count--;
		}
		_pending_removes.clear();
	}

	bool id_sync(Id id)
	{
		uint32_t idx = id_index(id);
		uint32_t gen = id_generation(id);

		if (idx > MAX_SYNC_INDEX)
			return false;

		if (idx >= _slots.size())
		{
			uint32_t old_size = static_cast<uint32_t>(_slots.size());
			_slots.resize(idx + 1, make_id(0, 0, ID_FLAG_FREE));
			for (uint32_t i = old_size; i <= idx; ++i)
			{
				_slots[i] = make_id(i, 0, ID_FLAG_FREE);
				if (i < idx) _free_ids.push_back(i);
			}
		}

		uint32_t current_gen = id_generation(_slots[idx]);
		if (current_gen > gen)
			return false;

		if (id_is_free(_slots[idx])) _alive_count++;
		_slots[idx] = make_id(idx, gen); // clear free flag
		return true;
	}

	// ====== SCHEDULING ======
	template <typename F>
	void task_add(NameId name, float priority, F &&fn, bool pauseable = false)
	{
		Task t;
		t.name = name;
		t.priority = priority;
		t.pauseable = pauseable;

		t.fn = [f = std::forward<F>(fn)](Pm &pm) { f(pm); };
		task_list.push_back(std::move(t));
		_tasks_dirty = true;
	}

	template <typename F>
	void task_add(const char *name, float priority, F &&fn, bool pauseable = false)
	{
		task_add(name_new(name), priority, std::forward<F>(fn), pauseable);
	}

	const std::vector<uint16_t> &task_order() const { return _task_order_indices; }

	Task *task_get(NameId name)
	{
		for (auto &t : task_list)
			if (t.name == name)
				return &t;
		return nullptr;
	}
	Task *task_get(const char *name) { return task_get(name_new(name)); }

	void task_reset_stats()
	{
		for (auto &t : task_list)
		{
			t.max_us = 0;
			t.runs = 0;
		}
	}

	void task_stop(NameId name)
	{
		for (auto &t : task_list)
			if (t.name == name)
			{
				t.fn = {};
				t.active = false;
				_tasks_dirty = true;
			}
	}
	void task_stop(const char *name) { task_stop(name_new(name)); }

	// ====== POOLS ======
	template <typename T>
	Pool<T> *pool_get(NameId name)
	{
		if (name == NULL_NAME) return nullptr;
		if (name >= _pool_entries.size()) _pool_entries.resize(name + 1);
		auto &r = _pool_entries[name];
		if (!r.pool)
		{
			auto p = std::make_unique<Pool<T>>();
			p->pool_id = _next_pool_id++;
			p->kernel_slots = &_slots;
			p->kernel = this;
			r.type_id = get_type_id<T>();

			if (p->pool_id >= _pool_by_id.size())
				_pool_by_id.resize(p->pool_id + 1, nullptr);
			_pool_by_id[p->pool_id] = p.get();
			r.pool = std::move(p);
		}
		assert(r.type_id == get_type_id<T>() && "Pool type mismatch! Same name used with different types");
		return static_cast<Pool<T> *>(r.pool.get());
	}

	template <typename T>
	Pool<T> *pool_get(const char *name) { return pool_get<T>(name_new(name)); }

	// ====== STATES — Named singletons ======
	template <typename T>
	T *state_get(NameId name)
	{
		if (name == NULL_NAME) return nullptr;
		if (name >= _state_entries.size()) _state_entries.resize(name + 1);
		auto &r = _state_entries[name];
		if (!r.state)
		{
			r.state = std::make_unique<StateHolder<T>>();
			r.type_id = get_type_id<T>();
		}
		assert(r.type_id == get_type_id<T>() && "State type mismatch! Same name used with different types");
		return &static_cast<StateHolder<T> *>(r.state.get())->value;
	}

	template <typename T>
	T *state_get(const char *name) { return state_get<T>(name_new(name)); }

};

// =============================================================================
// Pool<T>::each — read-only iteration, no change hooks
// Signatures: void(const T&) or void(Id, const T&)
// =============================================================================
template <typename T>
template <typename F>
void Pool<T>::each(F &&fn, Parallel p, uint32_t threads)
{
	size_t n = items.size();
	if (n == 0) return;

	bool use_parallel = (p == Parallel::On)
		|| (p == Parallel::Auto && n >= PARALLEL_THRESHOLD && kernel);
	if (p == Parallel::Off) use_parallel = false;

	if (use_parallel && kernel)
	{
		auto *tp = kernel->thread_pool();
		tp->dispatch(n, [this, &fn](size_t begin, size_t end) {
			for (size_t i = begin; i < end; i++)
			{
				if constexpr (std::is_invocable_v<F, Id, const T &>)
					fn(id_at(i), static_cast<const T &>(items[i]));
				else
					fn(static_cast<const T &>(items[i]));
			}
		}, threads);
	}
	else
	{
		for (size_t i = 0; i < n; i++)
		{
			if constexpr (std::is_invocable_v<F, Id, const T &>)
				fn(id_at(i), static_cast<const T &>(items[i]));
			else
				fn(static_cast<const T &>(items[i]));
		}
	}
}

// =============================================================================
// Pool<T>::each_mut — mutable iteration, fires change hooks
// Signatures: void(T&) or void(Id, T&)
// If change_fn is installed and parallel requested, falls back to sequential.
// =============================================================================
template <typename T>
template <typename F>
void Pool<T>::each_mut(F &&fn, Parallel p, uint32_t threads)
{
	size_t n = items.size();
	if (n == 0) return;

	bool use_parallel = (p == Parallel::On)
		|| (p == Parallel::Auto && n >= PARALLEL_THRESHOLD && kernel);
	if (p == Parallel::Off) use_parallel = false;
	if (change_fn) use_parallel = false;

	if (use_parallel && kernel)
	{
		auto *tp = kernel->thread_pool();
		tp->dispatch(n, [this, &fn](size_t begin, size_t end) {
			for (size_t i = begin; i < end; i++)
			{
				if constexpr (std::is_invocable_v<F, Id, T &>)
					fn(id_at(i), items[i]);
				else
					fn(items[i]);
			}
		}, threads);
	}
	else
	{
		for (size_t i = 0; i < n; i++)
		{
			Id eid = id_at(i);
			if constexpr (std::is_invocable_v<F, Id, T &>)
				fn(eid, items[i]);
			else
				fn(items[i]);
			if (change_fn) change_fn(change_ctx, eid);
		}
	}
}

} // namespace pm
