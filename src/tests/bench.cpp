// bench.cpp — pm_core stress tests & benchmarks
//
// Measures ns/op for Pool, State, Entity, and integrated workloads.
// No external dependencies — raw chrono timing + printf results.
// Always writes bench_results.csv next to the binary for regression tracking.
//
// Build: cmake --build build --target pm_bench
// Run:   ./build/pm_bench

#include "pm_core.hpp"
#include "pm_spatial_grid.hpp"
#include "pm_math.hpp"
#include "pm_util.hpp"
#include <cassert>
#include <cstdio>
#include <chrono>
#include <vector>
#include <cmath>
#include <cstring>

// =============================================================================
// Timing helpers
// =============================================================================

struct BenchResult
{
    std::string name;
    double total_ms;
    uint64_t ops;
    double ns_per_op;
};

static std::vector<BenchResult> g_results;

template <typename F>
void bench(const char *name, uint64_t ops, F &&fn)
{
    // Warmup
    fn();

    auto t0 = std::chrono::high_resolution_clock::now();
    fn();
    auto t1 = std::chrono::high_resolution_clock::now();

    double ms = std::chrono::duration<double, std::milli>(t1 - t0).count();
    double ns_op = (ms * 1e6) / static_cast<double>(ops);

    printf("  %-50s %8.2f ms  %10.1f ns/op\n", name, ms, ns_op);
    g_results.push_back({name, ms, ops, ns_op});
}

// =============================================================================
// Component types for benchmarks
// =============================================================================

struct Pos       { float x = 0, y = 0; };
struct Vel       { float dx = 0, dy = 0; };
struct Health    { int hp = 100; };
struct Damage    { int dmg = 10; };
struct Sprite    { uint32_t tex_id = 0; float u = 0, v = 0, w = 16, h = 16; };
struct Cooldown  { float remaining = 0; };
struct Team      { uint8_t team_id = 0; };
struct BigComp   { float data[64] = {}; }; // 256 bytes — cache pressure test

// Hellfire-accurate component types for game workload benchmarks
struct BMonster  { pm::Vec2 pos, vel; float shoot_timer = 0, size = 12; };
struct BBullet   { pm::Vec2 pos, vel; float lifetime = 1.5f, size = 4; bool player_owned = true; };
struct BPlayer   { pm::Vec2 pos; float hp = 100, cooldown = 0, invuln = 0; bool alive = true; };

// =============================================================================
// POOL BENCHMARKS
// =============================================================================

void bench_pool_add()
{
    printf("\n--- Pool: add ---\n");

    // 10k adds to empty pool
    bench("pool add 10k", 10000, []() {
        pm::Pm pm;
        auto *pool = pm.pool<Pos>("pos");
        for (int i = 0; i < 10000; i++)
        {
            pm::Id id = pm.spawn();
            pool->add(id, {static_cast<float>(i), static_cast<float>(i)});
        }
    });

    // 100k adds
    bench("pool add 100k", 100000, []() {
        pm::Pm pm;
        auto *pool = pm.pool<Pos>("pos");
        for (int i = 0; i < 100000; i++)
        {
            pm::Id id = pm.spawn();
            pool->add(id, {static_cast<float>(i), static_cast<float>(i)});
        }
    });

    // add overwrite (entity already in pool)
    bench("pool add overwrite 10k", 10000, []() {
        pm::Pm pm;
        auto *pool = pm.pool<Pos>("pos");
        std::vector<pm::Id> ids;
        ids.reserve(10000);
        for (int i = 0; i < 10000; i++)
        {
            pm::Id id = pm.spawn();
            pool->add(id, {0, 0});
            ids.push_back(id);
        }
        // Now overwrite all 10k
        for (int i = 0; i < 10000; i++)
            pool->add(ids[static_cast<size_t>(i)], {static_cast<float>(i), static_cast<float>(i)});
    });

    // add with big component (cache pressure)
    bench("pool add 10k (256B component)", 10000, []() {
        pm::Pm pm;
        auto *pool = pm.pool<BigComp>("big");
        for (int i = 0; i < 10000; i++)
        {
            pm::Id id = pm.spawn();
            BigComp c{};
            c.data[0] = static_cast<float>(i);
            pool->add(id, c);
        }
    });
}

void bench_pool_get()
{
    printf("\n--- Pool: get ---\n");

    // Setup: 10k entities in pool, then benchmark random access
    pm::Pm pm;
    auto *pool = pm.pool<Pos>("pos");
    std::vector<pm::Id> ids;
    ids.reserve(10000);
    for (int i = 0; i < 10000; i++)
    {
        pm::Id id = pm.spawn();
        pool->add(id, {static_cast<float>(i), 0});
        ids.push_back(id);
    }

    bench("pool get 10k (sequential)", 10000, [&]() {
        float sum = 0;
        for (auto id : ids)
        {
            auto *p = pool->get(id);
            if (p) sum += p->x;
        }
        assert(sum > 0);
    });

    // get with stale ids (should return nullptr)
    pm::Pm pm2;
    auto *pool2 = pm2.pool<Pos>("pos");
    std::vector<pm::Id> stale_ids;
    stale_ids.reserve(10000);
    for (int i = 0; i < 10000; i++)
    {
        pm::Id id = pm2.spawn();
        pool2->add(id, {static_cast<float>(i), 0});
        stale_ids.push_back(id);
    }
    // Remove all, making ids stale
    for (auto id : stale_ids)
        pm2.remove_entity(id);
    pm2.flush_removes();

    bench("pool get 10k (stale ids — all miss)", 10000, [&]() {
        int misses = 0;
        for (auto id : stale_ids)
        {
            if (!pool2->get(id)) misses++;
        }
        assert(misses == 10000);
    });

    bench("pool has 10k", 10000, [&]() {
        int hits = 0;
        for (auto id : ids)
        {
            if (pool->has(id)) hits++;
        }
        assert(hits == 10000);
    });
}

void bench_pool_remove()
{
    printf("\n--- Pool: remove ---\n");

    bench("pool remove 10k (swap-and-pop)", 10000, []() {
        pm::Pm pm;
        auto *pool = pm.pool<Pos>("pos");
        std::vector<pm::Id> ids;
        ids.reserve(10000);
        for (int i = 0; i < 10000; i++)
        {
            pm::Id id = pm.spawn();
            pool->add(id, {static_cast<float>(i), 0});
            ids.push_back(id);
        }
        for (auto id : ids)
            pool->remove(id);
        assert(pool->size() == 0);
    });

    bench("pool remove 10k via flush_removes (20 pools)", 10000, []() {
        pm::Pm pm;
        auto *pos     = pm.pool<Pos>("pos");
        auto *vel     = pm.pool<Vel>("vel");
        auto *hp      = pm.pool<Health>("hp");
        auto *dmg     = pm.pool<Damage>("dmg");
        auto *spr     = pm.pool<Sprite>("spr");
        auto *cd      = pm.pool<Cooldown>("cd");
        auto *team    = pm.pool<Team>("team");
        auto *big     = pm.pool<BigComp>("big");
        // Create 12 more pools (empty, but present in pool_by_id for broadcast)
        pm.pool<Pos>("pos2");  pm.pool<Pos>("pos3");  pm.pool<Pos>("pos4");
        pm.pool<Pos>("pos5");  pm.pool<Pos>("pos6");  pm.pool<Pos>("pos7");
        pm.pool<Pos>("pos8");  pm.pool<Pos>("pos9");  pm.pool<Pos>("pos10");
        pm.pool<Pos>("pos11"); pm.pool<Pos>("pos12"); pm.pool<Pos>("pos13");

        std::vector<pm::Id> ids;
        ids.reserve(10000);
        for (int i = 0; i < 10000; i++)
        {
            pm::Id id = pm.spawn();
            pos->add(id, {static_cast<float>(i), 0});
            vel->add(id, {1, 0});
            hp->add(id, {100});
            dmg->add(id, {10});
            spr->add(id, {0, 0, 0, 16, 16});
            cd->add(id, {0});
            team->add(id, {1});
            big->add(id);
            ids.push_back(id);
        }
        for (auto id : ids)
            pm.remove_entity(id);
        pm.flush_removes();
        assert(pos->size() == 0);
    });
}

// Helper to prevent compiler from optimizing away a value
template <typename T>
static void do_not_optimize(T const &val)
{
    asm volatile("" : : "r,m"(val) : "memory");
}

void bench_pool_each()
{
    printf("\n--- Pool: each (read-only) ---\n");

    // --- Trivial work (baseline) ---
    {
        pm::Pm pm;
        auto *pool = pm.pool<Pos>("pos");
        for (int i = 0; i < 100000; i++)
        {
            pm::Id id = pm.spawn();
            pool->add(id, {static_cast<float>(i), static_cast<float>(i)});
        }

        bench("each 100k trivial (seq)", 100000, [&]() {
            float sum = 0;
            pool->each([&](const Pos &p) { sum += p.x; }, pm::Parallel::Off);
            do_not_optimize(sum);
        });

        bench("each 100k trivial (parallel)", 100000, [&]() {
            pool->each([](const Pos &p) { do_not_optimize(p.x); }, pm::Parallel::On);
        });
    }

    // --- Medium work: trig per element (sqrtf + sinf + cosf) ---
    {
        pm::Pm pm;
        auto *pool = pm.pool<Pos>("pos");
        for (int i = 0; i < 100000; i++)
        {
            pm::Id id = pm.spawn();
            pool->add(id, {static_cast<float>(i) * 0.01f, static_cast<float>(i) * 0.02f});
        }

        bench("each 100k trig (seq)", 100000, [&]() {
            pool->each([](const Pos &p) {
                float d = sqrtf(p.x * p.x + p.y * p.y);
                float a = atan2f(p.y, p.x);
                float r = sinf(a) * cosf(d);
                do_not_optimize(r);
            }, pm::Parallel::Off);
        });

        bench("each 100k trig (parallel)", 100000, [&]() {
            pool->each([](const Pos &p) {
                float d = sqrtf(p.x * p.x + p.y * p.y);
                float a = atan2f(p.y, p.x);
                float r = sinf(a) * cosf(d);
                do_not_optimize(r);
            }, pm::Parallel::On);
        });
    }

    // --- Heavy work: 256B component, read all 64 floats per element ---
    {
        pm::Pm pm;
        auto *pool = pm.pool<BigComp>("big");
        for (int i = 0; i < 100000; i++)
        {
            pm::Id id = pm.spawn();
            BigComp c{};
            for (int j = 0; j < 64; j++)
                c.data[j] = static_cast<float>(i * 64 + j) * 0.001f;
            pool->add(id, c);
        }

        bench("each 100k 256B read-all (seq)", 100000, [&]() {
            pool->each([](const BigComp &c) {
                float sum = 0;
                for (int j = 0; j < 64; j++)
                    sum += sinf(c.data[j]);
                do_not_optimize(sum);
            }, pm::Parallel::Off);
        });

        bench("each 100k 256B read-all (parallel)", 100000, [&]() {
            pool->each([](const BigComp &c) {
                float sum = 0;
                for (int j = 0; j < 64; j++)
                    sum += sinf(c.data[j]);
                do_not_optimize(sum);
            }, pm::Parallel::On);
        });
    }

    // --- Cross-pool join: iterate pos, lookup vel + health ---
    {
        pm::Pm pm;
        auto *pos = pm.pool<Pos>("pos");
        auto *vel = pm.pool<Vel>("vel");
        auto *hp  = pm.pool<Health>("hp");
        for (int i = 0; i < 100000; i++)
        {
            pm::Id id = pm.spawn();
            pos->add(id, {static_cast<float>(i), static_cast<float>(i)});
            vel->add(id, {1.0f, 0.5f});
            if (i % 3 == 0) hp->add(id, {100 + i % 50});
        }

        bench("each 100k join 2 pools + branch (seq)", 100000, [&]() {
            pos->each([&](pm::Id id, const Pos &p) {
                auto *v = vel->get(id);
                auto *h = hp->get(id);
                float r = p.x + p.y;
                if (v) r += sqrtf(v->dx * v->dx + v->dy * v->dy);
                if (h) r *= static_cast<float>(h->hp) * 0.01f;
                do_not_optimize(r);
            }, pm::Parallel::Off);
        });

        bench("each 100k join 2 pools + branch (parallel)", 100000, [&]() {
            pos->each([&](pm::Id id, const Pos &p) {
                auto *v = vel->get(id);
                auto *h = hp->get(id);
                float r = p.x + p.y;
                if (v) r += sqrtf(v->dx * v->dx + v->dy * v->dy);
                if (h) r *= static_cast<float>(h->hp) * 0.01f;
                do_not_optimize(r);
            }, pm::Parallel::On);
        });
    }

    // --- Scale test: 500k with medium work ---
    {
        pm::Pm pm;
        auto *pool = pm.pool<Pos>("pos");
        for (int i = 0; i < 500000; i++)
        {
            pm::Id id = pm.spawn();
            pool->add(id, {static_cast<float>(i) * 0.01f, static_cast<float>(i) * 0.02f});
        }

        bench("each 500k trig (seq)", 500000, [&]() {
            pool->each([](const Pos &p) {
                float d = sqrtf(p.x * p.x + p.y * p.y);
                float r = sinf(d) * cosf(d);
                do_not_optimize(r);
            }, pm::Parallel::Off);
        });

        bench("each 500k trig (parallel)", 500000, [&]() {
            pool->each([](const Pos &p) {
                float d = sqrtf(p.x * p.x + p.y * p.y);
                float r = sinf(d) * cosf(d);
                do_not_optimize(r);
            }, pm::Parallel::On);
        });
    }
}

void bench_pool_each_mut()
{
    printf("\n--- Pool: each_mut (mutable) ---\n");

    // --- Trivial work (baseline) ---
    {
        pm::Pm pm;
        auto *pool = pm.pool<Pos>("pos");
        for (int i = 0; i < 100000; i++)
        {
            pm::Id id = pm.spawn();
            pool->add(id, {static_cast<float>(i), static_cast<float>(i)});
        }

        bench("each_mut 100k trivial (seq)", 100000, [&]() {
            pool->each_mut([](Pos &p) { p.x += 1.0f; }, pm::Parallel::Off);
        });

        bench("each_mut 100k trivial (parallel)", 100000, [&]() {
            pool->each_mut([](Pos &p) { p.x += 1.0f; }, pm::Parallel::On);
        });
    }

    // --- Medium work: physics-style update with trig ---
    {
        pm::Pm pm;
        auto *pool = pm.pool<Pos>("pos");
        for (int i = 0; i < 100000; i++)
        {
            pm::Id id = pm.spawn();
            pool->add(id, {static_cast<float>(i) * 0.1f, static_cast<float>(i) * 0.05f});
        }

        bench("each_mut 100k physics sim (seq)", 100000, [&]() {
            pool->each_mut([](Pos &p) {
                float angle = atan2f(p.y, p.x);
                float dist = sqrtf(p.x * p.x + p.y * p.y);
                // Orbit: rotate position around origin
                angle += 0.01f;
                p.x = cosf(angle) * dist;
                p.y = sinf(angle) * dist;
            }, pm::Parallel::Off);
        });

        bench("each_mut 100k physics sim (parallel)", 100000, [&]() {
            pool->each_mut([](Pos &p) {
                float angle = atan2f(p.y, p.x);
                float dist = sqrtf(p.x * p.x + p.y * p.y);
                angle += 0.01f;
                p.x = cosf(angle) * dist;
                p.y = sinf(angle) * dist;
            }, pm::Parallel::On);
        });
    }

    // --- Heavy work: 256B component, transform all 64 floats ---
    {
        pm::Pm pm;
        auto *pool = pm.pool<BigComp>("big");
        for (int i = 0; i < 100000; i++)
        {
            pm::Id id = pm.spawn();
            BigComp c{};
            for (int j = 0; j < 64; j++)
                c.data[j] = static_cast<float>(i * 64 + j) * 0.001f;
            pool->add(id, c);
        }

        bench("each_mut 100k 256B transform (seq)", 100000, [&]() {
            pool->each_mut([](BigComp &c) {
                for (int j = 0; j < 64; j++)
                    c.data[j] = sinf(c.data[j]) * 0.99f + 0.01f;
            }, pm::Parallel::Off);
        });

        bench("each_mut 100k 256B transform (parallel)", 100000, [&]() {
            pool->each_mut([](BigComp &c) {
                for (int j = 0; j < 64; j++)
                    c.data[j] = sinf(c.data[j]) * 0.99f + 0.01f;
            }, pm::Parallel::On);
        });
    }

    // --- Cross-pool mutable join: iterate pos, write with vel lookup ---
    {
        pm::Pm pm;
        auto *pos = pm.pool<Pos>("pos");
        auto *vel = pm.pool<Vel>("vel");
        for (int i = 0; i < 100000; i++)
        {
            pm::Id id = pm.spawn();
            pos->add(id, {static_cast<float>(i) * 0.1f, static_cast<float>(i) * 0.05f});
            vel->add(id, {sinf(static_cast<float>(i)), cosf(static_cast<float>(i))});
        }

        bench("each_mut 100k join + physics (seq)", 100000, [&]() {
            pos->each_mut([&](pm::Id id, Pos &p) {
                auto *v = vel->get(id);
                if (v)
                {
                    float speed = sqrtf(v->dx * v->dx + v->dy * v->dy);
                    float angle = atan2f(p.y, p.x);
                    p.x += cosf(angle) * speed * 0.016f;
                    p.y += sinf(angle) * speed * 0.016f;
                }
            }, pm::Parallel::Off);
        });

        bench("each_mut 100k join + physics (parallel)", 100000, [&]() {
            pos->each_mut([&](pm::Id id, Pos &p) {
                auto *v = vel->get(id);
                if (v)
                {
                    float speed = sqrtf(v->dx * v->dx + v->dy * v->dy);
                    float angle = atan2f(p.y, p.x);
                    p.x += cosf(angle) * speed * 0.016f;
                    p.y += sinf(angle) * speed * 0.016f;
                }
            }, pm::Parallel::On);
        });
    }

    // --- Change hook overhead ---
    {
        pm::Pm pm;
        auto *pool = pm.pool<Pos>("pos");
        for (int i = 0; i < 100000; i++)
        {
            pm::Id id = pm.spawn();
            pool->add(id, {static_cast<float>(i), static_cast<float>(i)});
        }

        int hook_count = 0;
        pool->set_change_hook([](void *ctx, pm::Id) {
            (*static_cast<int *>(ctx))++;
        }, &hook_count);

        bench("each_mut 100k with change hook", 100000, [&]() {
            hook_count = 0;
            pool->each_mut([](Pos &p) { p.x += 1.0f; });
            assert(hook_count == 100000);
        });

        pool->set_change_hook(nullptr, nullptr);
    }

    // --- Scale: 500k with physics ---
    {
        pm::Pm pm;
        auto *pool = pm.pool<Pos>("pos");
        for (int i = 0; i < 500000; i++)
        {
            pm::Id id = pm.spawn();
            pool->add(id, {static_cast<float>(i) * 0.1f, static_cast<float>(i) * 0.05f});
        }

        bench("each_mut 500k physics sim (seq)", 500000, [&]() {
            pool->each_mut([](Pos &p) {
                float angle = atan2f(p.y, p.x);
                float dist = sqrtf(p.x * p.x + p.y * p.y);
                angle += 0.01f;
                p.x = cosf(angle) * dist;
                p.y = sinf(angle) * dist;
            }, pm::Parallel::Off);
        });

        bench("each_mut 500k physics sim (parallel)", 500000, [&]() {
            pool->each_mut([](Pos &p) {
                float angle = atan2f(p.y, p.x);
                float dist = sqrtf(p.x * p.x + p.y * p.y);
                angle += 0.01f;
                p.x = cosf(angle) * dist;
                p.y = sinf(angle) * dist;
            }, pm::Parallel::On);
        });
    }
}

void bench_pool_clear()
{
    printf("\n--- Pool: clear_all ---\n");

    bench("pool clear_all 100k", 100000, []() {
        pm::Pm pm;
        auto *pool = pm.pool<Pos>("pos");
        for (int i = 0; i < 100000; i++)
        {
            pm::Id id = pm.spawn();
            pool->add(id, {static_cast<float>(i), 0});
        }
        pool->clear_all();
        assert(pool->size() == 0);
    });
}

void bench_pool_mixed()
{
    printf("\n--- Pool: mixed add/get/remove ---\n");

    bench("pool mixed ops 10k (add, get, remove interleaved)", 30000, []() {
        pm::Pm pm;
        auto *pool = pm.pool<Pos>("pos");
        std::vector<pm::Id> ids;
        ids.reserve(10000);

        // Phase 1: add 10k
        for (int i = 0; i < 10000; i++)
        {
            pm::Id id = pm.spawn();
            pool->add(id, {static_cast<float>(i), 0});
            ids.push_back(id);
        }

        // Phase 2: get all 10k
        float sum = 0;
        for (auto id : ids)
        {
            auto *p = pool->get(id);
            if (p) sum += p->x;
        }
        assert(sum > 0);

        // Phase 3: remove all 10k via deferred
        for (auto id : ids)
            pm.remove_entity(id);
        pm.flush_removes();
        assert(pool->size() == 0);
    });
}

// =============================================================================
// STATE BENCHMARKS
// =============================================================================

struct GameConfig  { int width = 800, height = 600; float fov = 90; };
struct PhysicsCfg  { float gravity = -9.8f; float friction = 0.3f; };
struct NetConfig   { uint16_t port = 9999; int max_peers = 32; };

void bench_state()
{
    printf("\n--- State ---\n");

    bench("state fetch 10k (same state, repeated)", 10000, []() {
        pm::Pm pm;
        auto *cfg = pm.state<GameConfig>("config");
        cfg->width = 1920;
        for (int i = 0; i < 10000; i++)
        {
            auto *c = pm.state<GameConfig>("config");
            assert(c->width == 1920);
        }
    });

    bench("state create 100 distinct states", 100, []() {
        pm::Pm pm;
        char buf[32];
        for (int i = 0; i < 100; i++)
        {
            snprintf(buf, sizeof(buf), "state_%d", i);
            auto *s = pm.state<GameConfig>(buf);
            s->width = i;
        }
    });
}

// =============================================================================
// ENTITY / KERNEL BENCHMARKS
// =============================================================================

void bench_spawn()
{
    printf("\n--- Entity: spawn ---\n");

    bench("spawn 10k", 10000, []() {
        pm::Pm pm;
        for (int i = 0; i < 10000; i++)
            pm.spawn();
    });

    bench("spawn 100k", 100000, []() {
        pm::Pm pm;
        for (int i = 0; i < 100000; i++)
            pm.spawn();
    });

    // Spawn with free-list reuse
    bench("spawn 10k after remove (free-list reuse)", 10000, []() {
        pm::Pm pm;
        std::vector<pm::Id> ids;
        ids.reserve(10000);
        for (int i = 0; i < 10000; i++)
            ids.push_back(pm.spawn());
        for (auto id : ids)
            pm.remove_entity(id);
        pm.flush_removes();
        // Now spawn again — should reuse free slots
        for (int i = 0; i < 10000; i++)
            pm.spawn();
    });
}

void bench_flush()
{
    printf("\n--- Entity: flush_removes ---\n");

    // flush with 1 pool
    bench("flush 10k removes (1 pool)", 10000, []() {
        pm::Pm pm;
        auto *pool = pm.pool<Pos>("pos");
        std::vector<pm::Id> ids;
        ids.reserve(10000);
        for (int i = 0; i < 10000; i++)
        {
            pm::Id id = pm.spawn();
            pool->add(id, {static_cast<float>(i), 0});
            ids.push_back(id);
        }
        for (auto id : ids)
            pm.remove_entity(id);
        pm.flush_removes();
        assert(pool->size() == 0);
    });

    // flush with 8 pools
    bench("flush 10k removes (8 pools)", 10000, []() {
        pm::Pm pm;
        auto *p1 = pm.pool<Pos>("pos");
        auto *p2 = pm.pool<Vel>("vel");
        auto *p3 = pm.pool<Health>("hp");
        auto *p4 = pm.pool<Damage>("dmg");
        auto *p5 = pm.pool<Sprite>("spr");
        auto *p6 = pm.pool<Cooldown>("cd");
        auto *p7 = pm.pool<Team>("team");
        auto *p8 = pm.pool<BigComp>("big");

        std::vector<pm::Id> ids;
        ids.reserve(10000);
        for (int i = 0; i < 10000; i++)
        {
            pm::Id id = pm.spawn();
            p1->add(id, {static_cast<float>(i), 0});
            p2->add(id, {1, 0});
            p3->add(id, {100});
            p4->add(id, {10});
            p5->add(id);
            p6->add(id);
            p7->add(id);
            p8->add(id);
            ids.push_back(id);
        }
        for (auto id : ids)
            pm.remove_entity(id);
        pm.flush_removes();
        assert(p1->size() == 0);
    });

    // flush with many pools but sparse membership
    bench("flush 1k removes (20 pools, 3 populated)", 1000, []() {
        pm::Pm pm;
        auto *pos  = pm.pool<Pos>("pos");
        auto *vel  = pm.pool<Vel>("vel");
        auto *hp   = pm.pool<Health>("hp");
        // 17 empty pools that still get the broadcast
        pm.pool<Pos>("e1");  pm.pool<Pos>("e2");  pm.pool<Pos>("e3");
        pm.pool<Pos>("e4");  pm.pool<Pos>("e5");  pm.pool<Pos>("e6");
        pm.pool<Pos>("e7");  pm.pool<Pos>("e8");  pm.pool<Pos>("e9");
        pm.pool<Pos>("e10"); pm.pool<Pos>("e11"); pm.pool<Pos>("e12");
        pm.pool<Pos>("e13"); pm.pool<Pos>("e14"); pm.pool<Pos>("e15");
        pm.pool<Pos>("e16"); pm.pool<Pos>("e17");

        std::vector<pm::Id> ids;
        ids.reserve(1000);
        for (int i = 0; i < 1000; i++)
        {
            pm::Id id = pm.spawn();
            pos->add(id, {static_cast<float>(i), 0});
            vel->add(id, {1, 0});
            hp->add(id, {100});
            ids.push_back(id);
        }
        for (auto id : ids)
            pm.remove_entity(id);
        pm.flush_removes();
        assert(pos->size() == 0);
    });
}

void bench_entity_churn()
{
    printf("\n--- Entity: churn (spawn/remove cycles) ---\n");

    bench("entity churn 10k (spawn, add 3 components, remove, repeat)", 10000, []() {
        pm::Pm pm;
        auto *pos = pm.pool<Pos>("pos");
        auto *vel = pm.pool<Vel>("vel");
        auto *hp  = pm.pool<Health>("hp");

        for (int i = 0; i < 10000; i++)
        {
            pm::Id id = pm.spawn();
            pos->add(id, {static_cast<float>(i), 0});
            vel->add(id, {1, 1});
            hp->add(id, {100});
            pm.remove_entity(id);
            // Flush every 100 to keep deferred queue bounded
            if (i % 100 == 99)
                pm.flush_removes();
        }
        pm.flush_removes();
        assert(pos->size() == 0);
    });
}

// =============================================================================
// INTEGRATED BENCHMARKS — simulated game workloads
// =============================================================================

void bench_integrated_game_tick()
{
    printf("\n--- Integrated: simulated game tick ---\n");

    // Simulate: 5000 entities with Pos+Vel, 500 with Health,
    // iterate all for physics, check collisions via get()
    bench("game tick: 5k physics + 500 health checks", 5500, []() {
        pm::Pm pm;
        auto *pos = pm.pool<Pos>("pos");
        auto *vel = pm.pool<Vel>("vel");
        auto *hp  = pm.pool<Health>("hp");

        std::vector<pm::Id> all_ids;
        all_ids.reserve(5000);
        for (int i = 0; i < 5000; i++)
        {
            pm::Id id = pm.spawn();
            pos->add(id, {static_cast<float>(i % 100), static_cast<float>(i / 100)});
            vel->add(id, {1.0f, 0.5f});
            if (i < 500) hp->add(id, {100});
            all_ids.push_back(id);
        }

        float dt = 0.016f;

        // Physics: iterate pos+vel
        pos->each_mut([&](pm::Id id, Pos &p) {
            auto *v = vel->get(id);
            if (v)
            {
                p.x += v->dx * dt;
                p.y += v->dy * dt;
            }
        }, pm::Parallel::Off);

        // Health check: iterate health, get pos for range check
        hp->each([&](pm::Id id, const Health &h) {
            (void)h;
            auto *p = pos->get(id);
            if (p && p->x > 50.0f)
            {
                // Would apply damage — just read for benchmark
            }
        }, pm::Parallel::Off);
    });
}

void bench_integrated_multi_archetype()
{
    printf("\n--- Integrated: multi-archetype world ---\n");

    // Simulate a world with different entity archetypes:
    // - Players (Pos, Vel, Health, Team): 100
    // - Bullets (Pos, Vel, Damage, Cooldown): 2000
    // - Monsters (Pos, Health, Sprite, Team): 500
    // - Pickups (Pos, Sprite): 200
    // - Walls (Pos, BigComp): 300
    bench("multi-archetype: 3100 entities, 8 pools, tick sim", 3100, []() {
        pm::Pm pm;
        auto *pos  = pm.pool<Pos>("pos");
        auto *vel  = pm.pool<Vel>("vel");
        auto *hp   = pm.pool<Health>("hp");
        auto *dmg  = pm.pool<Damage>("dmg");
        auto *spr  = pm.pool<Sprite>("spr");
        auto *cd   = pm.pool<Cooldown>("cd");
        auto *team = pm.pool<Team>("team");
        auto *big  = pm.pool<BigComp>("big");

        // Spawn players
        for (int i = 0; i < 100; i++)
        {
            pm::Id id = pm.spawn();
            pos->add(id, {static_cast<float>(i * 10), 0});
            vel->add(id, {0, 0});
            hp->add(id, {200});
            team->add(id, {static_cast<uint8_t>(i % 2)});
        }

        // Spawn bullets
        std::vector<pm::Id> bullet_ids;
        bullet_ids.reserve(2000);
        for (int i = 0; i < 2000; i++)
        {
            pm::Id id = pm.spawn();
            pos->add(id, {static_cast<float>(i), static_cast<float>(i % 50)});
            vel->add(id, {10.0f, 0});
            dmg->add(id, {25});
            cd->add(id, {1.0f});
            bullet_ids.push_back(id);
        }

        // Spawn monsters
        for (int i = 0; i < 500; i++)
        {
            pm::Id id = pm.spawn();
            pos->add(id, {static_cast<float>(i * 5), static_cast<float>(i * 2)});
            hp->add(id, {150});
            spr->add(id, {1, 0, 0, 32, 32});
            team->add(id, {2});
        }

        // Spawn pickups
        for (int i = 0; i < 200; i++)
        {
            pm::Id id = pm.spawn();
            pos->add(id, {static_cast<float>(i * 20), 100});
            spr->add(id, {2, 0, 0, 16, 16});
        }

        // Spawn walls
        for (int i = 0; i < 300; i++)
        {
            pm::Id id = pm.spawn();
            pos->add(id, {static_cast<float>(i * 3), 0});
            big->add(id);
        }

        float dt = 0.016f;

        // Tick 1: Move all entities with velocity
        pos->each_mut([&](pm::Id id, Pos &p) {
            auto *v = vel->get(id);
            if (v) { p.x += v->dx * dt; p.y += v->dy * dt; }
        }, pm::Parallel::Off);

        // Tick 2: Decay cooldowns
        cd->each_mut([&](Cooldown &c) {
            c.remaining -= dt;
            if (c.remaining < 0) c.remaining = 0;
        }, pm::Parallel::Off);

        // Tick 3: Bullet lifetime — remove expired
        for (auto bid : bullet_ids)
        {
            auto *c = cd->get(bid);
            if (c && c->remaining <= 0)
                pm.remove_entity(bid);
        }

        pm.flush_removes();
    });
}

void bench_integrated_heavy_iteration()
{
    printf("\n--- Integrated: heavy iteration (join pattern) ---\n");

    // Common pattern: iterate pool A, lookup pool B for each entity
    pm::Pm pm;
    auto *pos = pm.pool<Pos>("pos");
    auto *vel = pm.pool<Vel>("vel");
    auto *hp  = pm.pool<Health>("hp");

    // 50k entities with pos+vel, 10k also have health
    for (int i = 0; i < 50000; i++)
    {
        pm::Id id = pm.spawn();
        pos->add(id, {static_cast<float>(i), static_cast<float>(i)});
        vel->add(id, {1, 1});
        if (i < 10000) hp->add(id, {100});
    }

    bench("iterate 50k pos, lookup vel (join pattern, seq)", 50000, [&]() {
        float dt = 0.016f;
        pos->each_mut([&](pm::Id id, Pos &p) {
            auto *v = vel->get(id);
            if (v) { p.x += v->dx * dt; p.y += v->dy * dt; }
        }, pm::Parallel::Off);
    });

    bench("iterate 50k pos, lookup vel (join pattern, parallel)", 50000, [&]() {
        float dt = 0.016f;
        pos->each_mut([&](pm::Id id, Pos &p) {
            auto *v = vel->get(id);
            if (v) { p.x += v->dx * dt; p.y += v->dy * dt; }
        }, pm::Parallel::On);
    });

    bench("iterate 10k health, lookup pos (smaller iterates larger)", 10000, [&]() {
        hp->each([&](pm::Id id, const Health &h) {
            (void)h;
            auto *p = pos->get(id);
            if (p) { /* read position */ }
        }, pm::Parallel::Off);
    });
}

void bench_integrated_sustained_churn()
{
    printf("\n--- Integrated: sustained churn (30 frames) ---\n");

    // Simulate 30 frames of spawning/removing entities while iterating
    bench("30 frames: 1k spawn + 1k remove + iterate 5k", 30, []() {
        pm::Pm pm;
        auto *pos = pm.pool<Pos>("pos");
        auto *vel = pm.pool<Vel>("vel");
        auto *hp  = pm.pool<Health>("hp");

        // Seed with 5000 entities
        std::vector<pm::Id> live_ids;
        live_ids.reserve(10000);
        for (int i = 0; i < 5000; i++)
        {
            pm::Id id = pm.spawn();
            pos->add(id, {static_cast<float>(i), static_cast<float>(i)});
            vel->add(id, {1, 0});
            if (i % 5 == 0) hp->add(id, {100});
            live_ids.push_back(id);
        }

        float dt = 0.016f;

        for (int frame = 0; frame < 30; frame++)
        {
            // Iterate: physics
            pos->each_mut([&](pm::Id id, Pos &p) {
                auto *v = vel->get(id);
                if (v) { p.x += v->dx * dt; p.y += v->dy * dt; }
            }, pm::Parallel::Off);

            // Remove oldest 1000
            int to_remove = std::min(1000, static_cast<int>(live_ids.size()));
            for (int i = 0; i < to_remove; i++)
            {
                pm.remove_entity(live_ids[static_cast<size_t>(i)]);
            }
            live_ids.erase(live_ids.begin(), live_ids.begin() + to_remove);

            pm.flush_removes();

            // Spawn 1000 new
            for (int i = 0; i < 1000; i++)
            {
                pm::Id id = pm.spawn();
                pos->add(id, {static_cast<float>(frame * 1000 + i), 0});
                vel->add(id, {1, 0});
                if (i % 5 == 0) hp->add(id, {100});
                live_ids.push_back(id);
            }
        }
    });
}

// =============================================================================
// THREAD SCALING — same workload at 1, 2, 4, 8, and hardware_concurrency threads
// =============================================================================

void bench_thread_scaling()
{
    uint32_t hw = std::thread::hardware_concurrency();
    if (hw == 0) hw = 4;

    // Build thread counts: 1, 2, 4, 8, ..., up to hw (deduplicated)
    std::vector<uint32_t> counts;
    for (uint32_t n = 1; n <= hw; n *= 2)
        counts.push_back(n);
    if (counts.back() != hw)
        counts.push_back(hw);

    // Single Pm instance — all benchmarks share the same thread pool,
    // using the per-call threads parameter to limit active workers.
    pm::Pm pm;

    printf("\n--- Thread scaling: each 200k trig (sqrt+sin+cos per element) ---\n");
    printf("  (hardware_concurrency = %u)\n", hw);

    {
        auto *pool = pm.pool<Pos>("pos_scale1");
        for (int i = 0; i < 200000; i++)
        {
            pm::Id id = pm.spawn();
            pool->add(id, {static_cast<float>(i) * 0.01f, static_cast<float>(i) * 0.02f});
        }

        for (uint32_t n : counts)
        {
            char label[64];
            snprintf(label, sizeof(label), "each 200k trig — %u thread%s", n, n == 1 ? "" : "s");
            bench(label, 200000, [&, n]() {
                pool->each([](const Pos &p) {
                    float d = sqrtf(p.x * p.x + p.y * p.y);
                    float a = atan2f(p.y, p.x);
                    float r = sinf(a) * cosf(d);
                    do_not_optimize(r);
                }, n == 1 ? pm::Parallel::Off : pm::Parallel::On, n);
            });
        }
    }

    printf("\n--- Thread scaling: each_mut 200k physics (atan2+sqrt+sin+cos per element) ---\n");

    {
        auto *pool = pm.pool<Pos>("pos_scale2");
        for (int i = 0; i < 200000; i++)
        {
            pm::Id id = pm.spawn();
            pool->add(id, {static_cast<float>(i) * 0.1f, static_cast<float>(i) * 0.05f});
        }

        for (uint32_t n : counts)
        {
            char label[64];
            snprintf(label, sizeof(label), "each_mut 200k physics — %u thread%s", n, n == 1 ? "" : "s");
            bench(label, 200000, [&, n]() {
                pool->each_mut([](Pos &p) {
                    float angle = atan2f(p.y, p.x);
                    float dist = sqrtf(p.x * p.x + p.y * p.y);
                    angle += 0.01f;
                    p.x = cosf(angle) * dist;
                    p.y = sinf(angle) * dist;
                }, n == 1 ? pm::Parallel::Off : pm::Parallel::On, n);
            });
        }
    }

    printf("\n--- Thread scaling: each_mut 100k 256B transform (64 sinf per element) ---\n");

    {
        auto *pool = pm.pool<BigComp>("big_scale");
        for (int i = 0; i < 100000; i++)
        {
            pm::Id id = pm.spawn();
            BigComp c{};
            for (int j = 0; j < 64; j++)
                c.data[j] = static_cast<float>(i * 64 + j) * 0.001f;
            pool->add(id, c);
        }

        for (uint32_t n : counts)
        {
            char label[64];
            snprintf(label, sizeof(label), "each_mut 100k 256B — %u thread%s", n, n == 1 ? "" : "s");
            bench(label, 100000, [&, n]() {
                pool->each_mut([](BigComp &c) {
                    for (int j = 0; j < 64; j++)
                        c.data[j] = sinf(c.data[j]) * 0.99f + 0.01f;
                }, n == 1 ? pm::Parallel::Off : pm::Parallel::On, n);
            });
        }
    }
}

// =============================================================================
// HELLFIRE GAME WORKLOAD BENCHMARKS
// =============================================================================

void bench_spatial_grid()
{
    printf("\n--- Spatial grid (hellfire collision) ---\n");

    // Insert 400 monsters (hellfire peak)
    bench("spatial insert 400 (hellfire peak)", 400, []() {
        pm::SpatialGrid grid(900, 700, 64);
        pm::Rng rng{42};
        for (int i = 0; i < 400; i++) {
            pm::Id id = pm::Id(static_cast<uint64_t>(i) << 16);
            grid.insert(id, {rng.rfr(0, 900), rng.rfr(0, 700)});
        }
    });

    // Insert 4k (10x stress)
    bench("spatial insert 4k (10x stress)", 4000, []() {
        pm::SpatialGrid grid(900, 700, 64);
        pm::Rng rng{42};
        for (int i = 0; i < 4000; i++) {
            pm::Id id = pm::Id(static_cast<uint64_t>(i) << 16);
            grid.insert(id, {rng.rfr(0, 900), rng.rfr(0, 700)});
        }
    });

    // Clear + insert (per-frame rebuild pattern — capacity retained)
    bench("spatial clear + insert 400 (per-frame rebuild)", 400, []() {
        static pm::SpatialGrid grid(900, 700, 64);
        pm::Rng rng{42};
        grid.clear();
        for (int i = 0; i < 400; i++) {
            pm::Id id = pm::Id(static_cast<uint64_t>(i) << 16);
            grid.insert(id, {rng.rfr(0, 900), rng.rfr(0, 700)});
        }
    });

    // Query benchmarks — pre-fill grid, then measure queries
    {
        pm::SpatialGrid grid(900, 700, 64);
        pm::Rng rng{42};
        for (int i = 0; i < 400; i++) {
            pm::Id id = pm::Id(static_cast<uint64_t>(i) << 16);
            grid.insert(id, {rng.rfr(0, 900), rng.rfr(0, 700)});
        }

        bench("spatial query small radius (r=20, 1k queries)", 1000, [&]() {
            pm::Rng qrng{99};
            int hits = 0;
            for (int i = 0; i < 1000; i++) {
                pm::Vec2 c = {qrng.rfr(0, 900), qrng.rfr(0, 700)};
                grid.query(c, 20.f, [&](pm::Id, pm::Vec2) { hits++; });
            }
            do_not_optimize(hits);
        });

        bench("spatial query large radius (r=100, 1k queries)", 1000, [&]() {
            pm::Rng qrng{99};
            int hits = 0;
            for (int i = 0; i < 1000; i++) {
                pm::Vec2 c = {qrng.rfr(0, 900), qrng.rfr(0, 700)};
                grid.query(c, 100.f, [&](pm::Id, pm::Vec2) { hits++; });
            }
            do_not_optimize(hits);
        });
    }

    // Full collision frame: rebuild grid + query per bullet
    bench("spatial full frame (400 insert + 600 queries)", 1000, []() {
        pm::SpatialGrid grid(900, 700, 64);
        pm::Rng rng{42};
        // Insert 400 monsters
        for (int i = 0; i < 400; i++) {
            pm::Id id = pm::Id(static_cast<uint64_t>(i) << 16);
            grid.insert(id, {rng.rfr(0, 900), rng.rfr(0, 700)});
        }
        // Query per bullet (600 bullets)
        int hits = 0;
        for (int i = 0; i < 600; i++) {
            pm::Vec2 c = {rng.rfr(0, 900), rng.rfr(0, 700)};
            grid.query(c, 20.f, [&](pm::Id, pm::Vec2) { hits++; });
        }
        do_not_optimize(hits);
    });
}

void bench_bullet_churn()
{
    printf("\n--- Bullet churn (hellfire high-frequency spawn/expire) ---\n");

    // 30 frames, 50 spawn + 40 expire per frame (typical hellfire)
    bench("bullet churn: 30f, 50 spawn + 40 expire/f", 30 * 90, []() {
        pm::Pm pm;
        auto* bp = pm.pool<BBullet>("bullets");
        pm::Rng rng{42};
        std::vector<pm::Id> live;
        live.reserve(600);

        // Seed with 200 bullets
        for (int i = 0; i < 200; i++) {
            pm::Id id = pm.spawn();
            bp->add(id, BBullet{{rng.rfr(0, 900), rng.rfr(0, 700)},
                                {rng.rfr(-750, 750), rng.rfr(-750, 750)},
                                rng.rfr(0.5f, 1.5f), 4, true});
            live.push_back(id);
        }

        for (int f = 0; f < 30; f++) {
            // Expire oldest 40
            int to_remove = std::min(40, static_cast<int>(live.size()));
            for (int i = 0; i < to_remove; i++)
                pm.remove_entity(live[static_cast<size_t>(i)]);
            live.erase(live.begin(), live.begin() + to_remove);
            pm.flush_removes();

            // Spawn 50 new
            for (int i = 0; i < 50; i++) {
                pm::Id id = pm.spawn();
                bp->add(id, BBullet{{rng.rfr(0, 900), rng.rfr(0, 700)},
                                    {rng.rfr(-750, 750), rng.rfr(-750, 750)},
                                    rng.rfr(0.5f, 1.5f), 4, true});
                live.push_back(id);
            }
        }
    });

    // Stress: 4x hellfire rate
    bench("bullet churn: 30f, 200 spawn + 180 expire/f", 30 * 380, []() {
        pm::Pm pm;
        auto* bp = pm.pool<BBullet>("bullets");
        pm::Rng rng{42};
        std::vector<pm::Id> live;
        live.reserve(2000);

        for (int i = 0; i < 400; i++) {
            pm::Id id = pm.spawn();
            bp->add(id, BBullet{{rng.rfr(0, 900), rng.rfr(0, 700)},
                                {rng.rfr(-750, 750), rng.rfr(-750, 750)},
                                rng.rfr(0.5f, 1.5f), 4, true});
            live.push_back(id);
        }

        for (int f = 0; f < 30; f++) {
            int to_remove = std::min(180, static_cast<int>(live.size()));
            for (int i = 0; i < to_remove; i++)
                pm.remove_entity(live[static_cast<size_t>(i)]);
            live.erase(live.begin(), live.begin() + to_remove);
            pm.flush_removes();

            for (int i = 0; i < 200; i++) {
                pm::Id id = pm.spawn();
                bp->add(id, BBullet{{rng.rfr(0, 900), rng.rfr(0, 700)},
                                    {rng.rfr(-750, 750), rng.rfr(-750, 750)},
                                    rng.rfr(0.5f, 1.5f), 4, true});
                live.push_back(id);
            }
        }
    });

    // Bullet physics: each_mut 600 (pos += vel * dt)
    bench("bullet physics: each_mut 600 (pos += vel*dt)", 600, []() {
        pm::Pm pm;
        auto* bp = pm.pool<BBullet>("bullets");
        pm::Rng rng{42};
        for (int i = 0; i < 600; i++) {
            pm::Id id = pm.spawn();
            bp->add(id, BBullet{{rng.rfr(0, 900), rng.rfr(0, 700)},
                                {rng.rfr(-750, 750), rng.rfr(-750, 750)},
                                rng.rfr(0.5f, 1.5f), 4, true});
        }
        float dt = 0.016f;
        bp->each_mut([dt](BBullet& b) {
            b.pos.x += b.vel.x * dt;
            b.pos.y += b.vel.y * dt;
            b.lifetime -= dt;
        }, pm::Parallel::Off);
    });
}

void bench_monster_ai()
{
    printf("\n--- Monster AI (hellfire steering + shooting) ---\n");

    // Setup shared player data (4 players at fixed positions like hellfire)
    static const pm::Vec2 player_pos[4] = {
        {225, 280}, {675, 280}, {225, 420}, {675, 420}
    };

    // 400 monsters, find closest of 4 players + steer (sequential)
    {
        pm::Pm pm;
        auto* mp = pm.pool<BMonster>("monsters");
        pm::Rng rng{42};
        for (int i = 0; i < 400; i++) {
            pm::Id id = pm.spawn();
            BMonster m;
            m.pos = {rng.rfr(0, 900), rng.rfr(0, 700)};
            m.vel = pm::norm(pm::Vec2{450, 350} - m.pos) * 60.f;
            m.size = rng.rfr(8, 16);
            m.shoot_timer = rng.rfr(1.5f, 4.f);
            mp->add(id, m);
        }

        bench("monster AI 400 (closest of 4 + steer, seq)", 400, [&]() {
            float dt = 0.016f;
            mp->each_mut([dt](BMonster& m) {
                pm::Vec2 tgt = m.pos; float best = 1e9f;
                for (int i = 0; i < 4; i++) {
                    float d = pm::dist(m.pos, player_pos[i]);
                    if (d < best) { best = d; tgt = player_pos[i]; }
                }
                pm::Vec2 desired = pm::norm(tgt - m.pos) * pm::len(m.vel);
                m.vel.x += (desired.x - m.vel.x) * 0.5f * dt;
                m.vel.y += (desired.y - m.vel.y) * 0.5f * dt;
                m.shoot_timer -= dt;
                m.pos.x += m.vel.x * dt;
                m.pos.y += m.vel.y * dt;
            }, pm::Parallel::Off);
        });

        bench("monster AI 400 (closest of 4 + steer, parallel)", 400, [&]() {
            float dt = 0.016f;
            mp->each_mut([dt](BMonster& m) {
                pm::Vec2 tgt = m.pos; float best = 1e9f;
                for (int i = 0; i < 4; i++) {
                    float d = pm::dist(m.pos, player_pos[i]);
                    if (d < best) { best = d; tgt = player_pos[i]; }
                }
                pm::Vec2 desired = pm::norm(tgt - m.pos) * pm::len(m.vel);
                m.vel.x += (desired.x - m.vel.x) * 0.5f * dt;
                m.vel.y += (desired.y - m.vel.y) * 0.5f * dt;
                m.shoot_timer -= dt;
                m.pos.x += m.vel.x * dt;
                m.pos.y += m.vel.y * dt;
            }, pm::Parallel::On);
        });
    }

    // 2000 monsters (5x stress) — sequential only
    {
        pm::Pm pm;
        auto* mp = pm.pool<BMonster>("monsters");
        pm::Rng rng{42};
        for (int i = 0; i < 2000; i++) {
            pm::Id id = pm.spawn();
            BMonster m;
            m.pos = {rng.rfr(0, 900), rng.rfr(0, 700)};
            m.vel = pm::norm(pm::Vec2{450, 350} - m.pos) * 60.f;
            m.size = rng.rfr(8, 16);
            m.shoot_timer = rng.rfr(1.5f, 4.f);
            mp->add(id, m);
        }

        bench("monster AI 2000 (5x stress, sequential)", 2000, [&]() {
            float dt = 0.016f;
            mp->each_mut([dt](BMonster& m) {
                pm::Vec2 tgt = m.pos; float best = 1e9f;
                for (int i = 0; i < 4; i++) {
                    float d = pm::dist(m.pos, player_pos[i]);
                    if (d < best) { best = d; tgt = player_pos[i]; }
                }
                pm::Vec2 desired = pm::norm(tgt - m.pos) * pm::len(m.vel);
                m.vel.x += (desired.x - m.vel.x) * 0.5f * dt;
                m.vel.y += (desired.y - m.vel.y) * 0.5f * dt;
                m.shoot_timer -= dt;
                m.pos.x += m.vel.x * dt;
                m.pos.y += m.vel.y * dt;
            }, pm::Parallel::Off);
        });
    }
}

void bench_collision_frame()
{
    printf("\n--- Collision frame (hellfire full collision pass) ---\n");

    static const pm::Vec2 player_pos[4] = {
        {225, 280}, {675, 280}, {225, 420}, {675, 420}
    };
    static constexpr float PLAYER_R = 32.f;
    static constexpr float QUERY_R  = 20.f; // PBULLET_SIZE + MONSTER_MAX_SZ * 0.65

    // Mid-game: 400 monsters, 300 bullets
    bench("collision frame: 400 mon, 300 bul, 4 players", 1300, []() {
        pm::Pm pm;
        auto* mp = pm.pool<BMonster>("monsters");
        auto* bp = pm.pool<BBullet>("bullets");
        pm::Rng rng{42};
        pm::SpatialGrid grid(900, 700, 64);

        // Fill monsters
        for (int i = 0; i < 400; i++) {
            pm::Id id = pm.spawn();
            BMonster m;
            m.pos = {rng.rfr(0, 900), rng.rfr(0, 700)};
            m.vel = {rng.rfr(-60, 60), rng.rfr(-60, 60)};
            m.size = rng.rfr(8, 16);
            mp->add(id, m);
        }
        // Fill bullets (200 player-owned, 100 enemy)
        for (int i = 0; i < 300; i++) {
            pm::Id id = pm.spawn();
            BBullet b;
            b.pos = {rng.rfr(0, 900), rng.rfr(0, 700)};
            b.vel = {rng.rfr(-750, 750), rng.rfr(-750, 750)};
            b.lifetime = 1.5f;
            b.size = 4;
            b.player_owned = i < 200;
            bp->add(id, b);
        }

        // --- Collision pass (mirrors hellfire_server.cpp) ---
        // 1. Build monster grid
        grid.clear();
        mp->each([&](pm::Id mid, const BMonster& m) {
            grid.insert(mid, m.pos);
        }, pm::Parallel::Off);

        // 2. Player bullets vs monsters via grid
        int kills = 0;
        bp->each([&](pm::Id, const BBullet& b) {
            if (!b.player_owned) return;
            grid.query(b.pos, QUERY_R, [&](pm::Id mid, pm::Vec2) {
                const BMonster* m = mp->get(mid);
                if (m && pm::dist(b.pos, m->pos) < b.size + m->size * 0.5f)
                    kills++;
            });
        }, pm::Parallel::Off);

        // 3. Players vs enemy bullets
        int player_hits = 0;
        for (int pi = 0; pi < 4; pi++) {
            bp->each([&](const BBullet& b) {
                if (!b.player_owned && pm::dist(b.pos, player_pos[pi]) < b.size + PLAYER_R)
                    player_hits++;
            }, pm::Parallel::Off);
        }

        do_not_optimize(kills);
        do_not_optimize(player_hits);
    });

    // Peak: 400 monsters, 600 bullets
    bench("collision frame: 400 mon, 600 bul, 4 players", 1600, []() {
        pm::Pm pm;
        auto* mp = pm.pool<BMonster>("monsters");
        auto* bp = pm.pool<BBullet>("bullets");
        pm::Rng rng{42};
        pm::SpatialGrid grid(900, 700, 64);

        for (int i = 0; i < 400; i++) {
            pm::Id id = pm.spawn();
            BMonster m;
            m.pos = {rng.rfr(0, 900), rng.rfr(0, 700)};
            m.vel = {rng.rfr(-60, 60), rng.rfr(-60, 60)};
            m.size = rng.rfr(8, 16);
            mp->add(id, m);
        }
        for (int i = 0; i < 600; i++) {
            pm::Id id = pm.spawn();
            BBullet b;
            b.pos = {rng.rfr(0, 900), rng.rfr(0, 700)};
            b.vel = {rng.rfr(-750, 750), rng.rfr(-750, 750)};
            b.lifetime = 1.5f;
            b.size = 4;
            b.player_owned = i < 400;
            bp->add(id, b);
        }

        grid.clear();
        mp->each([&](pm::Id mid, const BMonster& m) {
            grid.insert(mid, m.pos);
        }, pm::Parallel::Off);

        int kills = 0;
        bp->each([&](pm::Id, const BBullet& b) {
            if (!b.player_owned) return;
            grid.query(b.pos, QUERY_R, [&](pm::Id mid, pm::Vec2) {
                const BMonster* m = mp->get(mid);
                if (m && pm::dist(b.pos, m->pos) < b.size + m->size * 0.5f)
                    kills++;
            });
        }, pm::Parallel::Off);

        int player_hits = 0;
        for (int pi = 0; pi < 4; pi++) {
            bp->each([&](const BBullet& b) {
                if (!b.player_owned && pm::dist(b.pos, player_pos[pi]) < b.size + PLAYER_R)
                    player_hits++;
            }, pm::Parallel::Off);
        }

        do_not_optimize(kills);
        do_not_optimize(player_hits);
    });

    // Brute force comparison: O(bullets * monsters) without grid
    bench("collision brute force: 400x600 (no grid)", 240000, []() {
        pm::Rng rng{42};
        std::vector<pm::Vec2> monsters(400), bullets(600);
        std::vector<float> mon_sz(400);
        for (int i = 0; i < 400; i++) {
            monsters[static_cast<size_t>(i)] = {rng.rfr(0, 900), rng.rfr(0, 700)};
            mon_sz[static_cast<size_t>(i)] = rng.rfr(8, 16);
        }
        for (int i = 0; i < 600; i++)
            bullets[static_cast<size_t>(i)] = {rng.rfr(0, 900), rng.rfr(0, 700)};

        int kills = 0;
        for (int bi = 0; bi < 600; bi++) {
            for (int mi = 0; mi < 400; mi++) {
                if (pm::dist(bullets[static_cast<size_t>(bi)], monsters[static_cast<size_t>(mi)])
                    < 4.f + mon_sz[static_cast<size_t>(mi)] * 0.5f)
                    kills++;
            }
        }
        do_not_optimize(kills);
    });
}

void bench_server_tick()
{
    printf("\n--- Server tick (hellfire full frame simulation) ---\n");

    static const pm::Vec2 player_pos[4] = {
        {225, 280}, {675, 280}, {225, 420}, {675, 420}
    };

    auto run_tick = [](int n_monsters, int n_bullets, const char* label, uint64_t ops) {
        bench(label, ops, [n_monsters, n_bullets]() {
            pm::Pm pm;
            auto* mp = pm.pool<BMonster>("monsters");
            auto* bp = pm.pool<BBullet>("bullets");
            pm::Rng rng{42};
            pm::SpatialGrid grid(900, 700, 64);

            // Setup monsters
            for (int i = 0; i < n_monsters; i++) {
                pm::Id id = pm.spawn();
                BMonster m;
                m.pos = {rng.rfr(0, 900), rng.rfr(0, 700)};
                m.vel = pm::norm(pm::Vec2{450, 350} - m.pos) * 60.f;
                m.size = rng.rfr(8, 16);
                m.shoot_timer = rng.rfr(1.5f, 4.f);
                mp->add(id, m);
            }
            // Setup bullets
            for (int i = 0; i < n_bullets; i++) {
                pm::Id id = pm.spawn();
                BBullet b;
                b.pos = {rng.rfr(0, 900), rng.rfr(0, 700)};
                b.vel = {rng.rfr(-750, 750), rng.rfr(-750, 750)};
                b.lifetime = rng.rfr(0.5f, 1.5f);
                b.size = 4;
                b.player_owned = (rng.next() % 3) != 0;
                bp->add(id, b);
            }

            float dt = 0.016f;

            // Phase 1: Monster AI
            mp->each_mut([dt](BMonster& m) {
                pm::Vec2 tgt = m.pos; float best = 1e9f;
                for (int i = 0; i < 4; i++) {
                    float d = pm::dist(m.pos, player_pos[i]);
                    if (d < best) { best = d; tgt = player_pos[i]; }
                }
                pm::Vec2 desired = pm::norm(tgt - m.pos) * pm::len(m.vel);
                m.vel.x += (desired.x - m.vel.x) * 0.5f * dt;
                m.vel.y += (desired.y - m.vel.y) * 0.5f * dt;
                m.shoot_timer -= dt;
                m.pos.x += m.vel.x * dt;
                m.pos.y += m.vel.y * dt;
            }, pm::Parallel::Off);

            // Phase 2: Bullet physics
            bp->each_mut([&](pm::Id id, BBullet& b) {
                b.pos.x += b.vel.x * dt;
                b.pos.y += b.vel.y * dt;
                b.lifetime -= dt;
                if (b.lifetime <= 0) pm.remove_entity(id);
            }, pm::Parallel::Off);

            // Phase 3: Collision
            grid.clear();
            mp->each([&](pm::Id mid, const BMonster& m) {
                grid.insert(mid, m.pos);
            }, pm::Parallel::Off);

            bp->each([&](pm::Id bid, const BBullet& b) {
                if (!b.player_owned) return;
                bool hit = false;
                grid.query(b.pos, 20.f, [&](pm::Id mid, pm::Vec2) {
                    if (hit) return;
                    const BMonster* m = mp->get(mid);
                    if (m && pm::dist(b.pos, m->pos) < b.size + m->size * 0.5f) {
                        pm.remove_entity(mid);
                        pm.remove_entity(bid);
                        hit = true;
                    }
                });
            }, pm::Parallel::Off);

            // Phase 4: Cleanup OOB
            mp->each([&](pm::Id id, const BMonster& m) {
                if (m.pos.x < -100 || m.pos.x > 1000 || m.pos.y < -100 || m.pos.y > 800)
                    pm.remove_entity(id);
            }, pm::Parallel::Off);
            bp->each([&](pm::Id id, const BBullet& b) {
                if (b.pos.x < -50 || b.pos.x > 950 || b.pos.y < -50 || b.pos.y > 750)
                    pm.remove_entity(id);
            }, pm::Parallel::Off);

            // Phase 5: Flush
            pm.flush_removes();
        });
    };

    run_tick(60, 50, "server tick: level 1 (60 mon, 50 bul)", 200);
    run_tick(400, 600, "server tick: level 5 (400 mon, 600 bul)", 1600);

    // Sustained: 30 ticks at level 5
    bench("30 server ticks: level 5 sustained", 30 * 1600, []() {
        pm::Pm pm;
        auto* mp = pm.pool<BMonster>("monsters");
        auto* bp = pm.pool<BBullet>("bullets");
        pm::Rng rng{42};
        pm::SpatialGrid grid(900, 700, 64);

        // Initial population
        for (int i = 0; i < 400; i++) {
            pm::Id id = pm.spawn();
            BMonster m;
            m.pos = {rng.rfr(0, 900), rng.rfr(0, 700)};
            m.vel = pm::norm(pm::Vec2{450, 350} - m.pos) * 60.f;
            m.size = rng.rfr(8, 16);
            m.shoot_timer = rng.rfr(1.5f, 4.f);
            mp->add(id, m);
        }
        for (int i = 0; i < 600; i++) {
            pm::Id id = pm.spawn();
            BBullet b;
            b.pos = {rng.rfr(0, 900), rng.rfr(0, 700)};
            b.vel = {rng.rfr(-750, 750), rng.rfr(-750, 750)};
            b.lifetime = rng.rfr(0.5f, 1.5f);
            b.size = 4;
            b.player_owned = (rng.next() % 3) != 0;
            bp->add(id, b);
        }

        float dt = 0.016f;

        for (int tick = 0; tick < 30; tick++) {
            // Monster AI
            mp->each_mut([dt](BMonster& m) {
                pm::Vec2 tgt = m.pos; float best = 1e9f;
                for (int i = 0; i < 4; i++) {
                    float d = pm::dist(m.pos, player_pos[i]);
                    if (d < best) { best = d; tgt = player_pos[i]; }
                }
                pm::Vec2 desired = pm::norm(tgt - m.pos) * pm::len(m.vel);
                m.vel.x += (desired.x - m.vel.x) * 0.5f * dt;
                m.vel.y += (desired.y - m.vel.y) * 0.5f * dt;
                m.shoot_timer -= dt;
                m.pos.x += m.vel.x * dt;
                m.pos.y += m.vel.y * dt;
            }, pm::Parallel::Off);

            // Bullet physics
            bp->each_mut([&](pm::Id id, BBullet& b) {
                b.pos.x += b.vel.x * dt;
                b.pos.y += b.vel.y * dt;
                b.lifetime -= dt;
                if (b.lifetime <= 0) pm.remove_entity(id);
            }, pm::Parallel::Off);

            // Collision
            grid.clear();
            mp->each([&](pm::Id mid, const BMonster& m) {
                grid.insert(mid, m.pos);
            }, pm::Parallel::Off);

            bp->each([&](pm::Id bid, const BBullet& b) {
                if (!b.player_owned) return;
                bool hit = false;
                grid.query(b.pos, 20.f, [&](pm::Id mid, pm::Vec2) {
                    if (hit) return;
                    const BMonster* m = mp->get(mid);
                    if (m && pm::dist(b.pos, m->pos) < b.size + m->size * 0.5f) {
                        pm.remove_entity(mid);
                        pm.remove_entity(bid);
                        hit = true;
                    }
                });
            }, pm::Parallel::Off);

            // Cleanup OOB
            mp->each([&](pm::Id id, const BMonster& m) {
                if (m.pos.x < -100 || m.pos.x > 1000 || m.pos.y < -100 || m.pos.y > 800)
                    pm.remove_entity(id);
            }, pm::Parallel::Off);
            bp->each([&](pm::Id id, const BBullet& b) {
                if (b.pos.x < -50 || b.pos.x > 950 || b.pos.y < -50 || b.pos.y > 750)
                    pm.remove_entity(id);
            }, pm::Parallel::Off);

            pm.flush_removes();

            // Respawn lost monsters/bullets to maintain population
            while (static_cast<int>(mp->items.size()) < 400) {
                pm::Id id = pm.spawn();
                BMonster m;
                m.pos = {rng.rfr(0, 900), rng.rfr(0, 700)};
                m.vel = pm::norm(pm::Vec2{450, 350} - m.pos) * 60.f;
                m.size = rng.rfr(8, 16);
                m.shoot_timer = rng.rfr(1.5f, 4.f);
                mp->add(id, m);
            }
            while (static_cast<int>(bp->items.size()) < 600) {
                pm::Id id = pm.spawn();
                BBullet b;
                b.pos = {rng.rfr(0, 900), rng.rfr(0, 700)};
                b.vel = {rng.rfr(-750, 750), rng.rfr(-750, 750)};
                b.lifetime = rng.rfr(0.5f, 1.5f);
                b.size = 4;
                b.player_owned = (rng.next() % 3) != 0;
                bp->add(id, b);
            }
        }
    });
}

void bench_plc_utils()
{
    printf("\n--- PLC utilities (per-entity-per-frame overhead) ---\n");

    bench("Cooldown::ready 100k", 100000, []() {
        std::vector<pm::Cooldown> cds(100000, pm::Cooldown(0.5f));
        float dt = 0.016f;
        int fires = 0;
        for (auto& cd : cds) {
            if (cd.ready(dt)) fires++;
        }
        do_not_optimize(fires);
    });

    bench("Hysteresis update+set 100k", 100000, []() {
        std::vector<pm::Hysteresis<bool>> hs(100000, pm::Hysteresis<bool>(false, 0.1f));
        float dt = 0.016f;
        for (size_t i = 0; i < hs.size(); i++) {
            hs[i].update(dt);
            hs[i].set(i % 3 == 0);
        }
        do_not_optimize(hs[0].get());
    });

    bench("RisingEdge update 100k", 100000, []() {
        std::vector<pm::RisingEdge> edges(100000);
        int fires = 0;
        for (size_t i = 0; i < edges.size(); i++) {
            if (edges[i].update(i % 7 == 0)) fires++;
        }
        do_not_optimize(fires);
    });

    bench("DelayTimer update 100k", 100000, []() {
        std::vector<pm::DelayTimer> timers(100000, pm::DelayTimer(0.5f, 0.2f));
        float dt = 0.016f;
        int active = 0;
        for (size_t i = 0; i < timers.size(); i++) {
            timers[i].update(i % 5 == 0, dt);
            if (timers[i]) active++;
        }
        do_not_optimize(active);
    });

    bench("Counter increment 100k", 100000, []() {
        std::vector<pm::Counter> ctrs(100000, pm::Counter(10));
        int done = 0;
        for (auto& c : ctrs) {
            c.increment();
            if (c.done) done++;
        }
        do_not_optimize(done);
    });
}

void bench_multi_pool_tick()
{
    printf("\n--- Multi-pool tick (hellfire pool structure) ---\n");

    static const pm::Vec2 player_pos[4] = {
        {225, 280}, {675, 280}, {225, 420}, {675, 420}
    };

    // 3 pools, hellfire sizes: iterate all + cross-lookups
    bench("multi-pool tick: 4p + 400m + 600b, iterate all", 1004, []() {
        pm::Pm pm;
        auto* pp = pm.pool<BPlayer>("players");
        auto* mp = pm.pool<BMonster>("monsters");
        auto* bp = pm.pool<BBullet>("bullets");
        pm::Rng rng{42};

        // 4 players
        for (int i = 0; i < 4; i++) {
            pm::Id id = pm.spawn();
            pp->add(id, BPlayer{player_pos[i], 100, 0, 0, true});
        }
        // 400 monsters
        for (int i = 0; i < 400; i++) {
            pm::Id id = pm.spawn();
            BMonster m;
            m.pos = {rng.rfr(0, 900), rng.rfr(0, 700)};
            m.vel = pm::norm(pm::Vec2{450, 350} - m.pos) * 60.f;
            m.size = rng.rfr(8, 16);
            mp->add(id, m);
        }
        // 600 bullets
        for (int i = 0; i < 600; i++) {
            pm::Id id = pm.spawn();
            BBullet b;
            b.pos = {rng.rfr(0, 900), rng.rfr(0, 700)};
            b.vel = {rng.rfr(-750, 750), rng.rfr(-750, 750)};
            b.lifetime = rng.rfr(0.5f, 1.5f);
            b.size = 4;
            b.player_owned = (rng.next() % 3) != 0;
            bp->add(id, b);
        }

        float dt = 0.016f;

        // Player update
        pp->each_mut([dt](BPlayer& p) {
            if (p.cooldown > 0) p.cooldown -= dt;
            if (p.invuln > 0) p.invuln -= dt;
        }, pm::Parallel::Off);

        // Monster AI (reads player pool)
        mp->each_mut([&, dt](BMonster& m) {
            pm::Vec2 tgt = m.pos; float best = 1e9f;
            pp->each([&](const BPlayer& p) {
                if (!p.alive) return;
                float d = pm::dist(m.pos, p.pos);
                if (d < best) { best = d; tgt = p.pos; }
            }, pm::Parallel::Off);
            pm::Vec2 desired = pm::norm(tgt - m.pos) * pm::len(m.vel);
            m.vel.x += (desired.x - m.vel.x) * 0.5f * dt;
            m.vel.y += (desired.y - m.vel.y) * 0.5f * dt;
            m.pos.x += m.vel.x * dt;
            m.pos.y += m.vel.y * dt;
        }, pm::Parallel::Off);

        // Bullet physics
        bp->each_mut([dt](BBullet& b) {
            b.pos.x += b.vel.x * dt;
            b.pos.y += b.vel.y * dt;
            b.lifetime -= dt;
        }, pm::Parallel::Off);
    });

    // Same + spatial grid collision
    bench("multi-pool tick + spatial grid collision", 1004, []() {
        pm::Pm pm;
        auto* pp = pm.pool<BPlayer>("players");
        auto* mp = pm.pool<BMonster>("monsters");
        auto* bp = pm.pool<BBullet>("bullets");
        pm::Rng rng{42};
        pm::SpatialGrid grid(900, 700, 64);

        for (int i = 0; i < 4; i++) {
            pm::Id id = pm.spawn();
            pp->add(id, BPlayer{player_pos[i], 100, 0, 0, true});
        }
        for (int i = 0; i < 400; i++) {
            pm::Id id = pm.spawn();
            BMonster m;
            m.pos = {rng.rfr(0, 900), rng.rfr(0, 700)};
            m.vel = pm::norm(pm::Vec2{450, 350} - m.pos) * 60.f;
            m.size = rng.rfr(8, 16);
            mp->add(id, m);
        }
        for (int i = 0; i < 600; i++) {
            pm::Id id = pm.spawn();
            BBullet b;
            b.pos = {rng.rfr(0, 900), rng.rfr(0, 700)};
            b.vel = {rng.rfr(-750, 750), rng.rfr(-750, 750)};
            b.lifetime = rng.rfr(0.5f, 1.5f);
            b.size = 4;
            b.player_owned = (rng.next() % 3) != 0;
            bp->add(id, b);
        }

        float dt = 0.016f;

        // Player update
        pp->each_mut([dt](BPlayer& p) {
            if (p.cooldown > 0) p.cooldown -= dt;
            if (p.invuln > 0) p.invuln -= dt;
        }, pm::Parallel::Off);

        // Monster AI
        mp->each_mut([&, dt](BMonster& m) {
            pm::Vec2 tgt = m.pos; float best = 1e9f;
            pp->each([&](const BPlayer& p) {
                if (!p.alive) return;
                float d = pm::dist(m.pos, p.pos);
                if (d < best) { best = d; tgt = p.pos; }
            }, pm::Parallel::Off);
            pm::Vec2 desired = pm::norm(tgt - m.pos) * pm::len(m.vel);
            m.vel.x += (desired.x - m.vel.x) * 0.5f * dt;
            m.vel.y += (desired.y - m.vel.y) * 0.5f * dt;
            m.pos.x += m.vel.x * dt;
            m.pos.y += m.vel.y * dt;
        }, pm::Parallel::Off);

        // Bullet physics
        bp->each_mut([dt](BBullet& b) {
            b.pos.x += b.vel.x * dt;
            b.pos.y += b.vel.y * dt;
            b.lifetime -= dt;
        }, pm::Parallel::Off);

        // Collision via grid
        grid.clear();
        mp->each([&](pm::Id mid, const BMonster& m) {
            grid.insert(mid, m.pos);
        }, pm::Parallel::Off);

        int kills = 0;
        bp->each([&](pm::Id, const BBullet& b) {
            if (!b.player_owned) return;
            grid.query(b.pos, 20.f, [&](pm::Id mid, pm::Vec2) {
                const BMonster* m = mp->get(mid);
                if (m && pm::dist(b.pos, m->pos) < b.size + m->size * 0.5f)
                    kills++;
            });
        }, pm::Parallel::Off);

        do_not_optimize(kills);
    });
}

// =============================================================================
// MAIN
// =============================================================================

// Write CSV to benchmarks/latest.csv (git-tracked for regression comparison).
// Assumes bench is run from the repo root (./build/pm_bench).
static void write_csv()
{
    const char* path = "benchmarks/latest.csv";
    FILE* f = fopen(path, "w");
    if (!f) {
        fprintf(stderr, "warning: could not write %s (run from repo root?)\n", path);
        return;
    }
    fprintf(f, "benchmark,total_ms,ops,ns_per_op\n");
    for (auto& r : g_results)
        fprintf(f, "\"%s\",%.4f,%llu,%.2f\n",
                r.name.c_str(), r.total_ms,
                (unsigned long long)r.ops, r.ns_per_op);
    fclose(f);
    printf("\nCSV written to %s\n", path);
}

int main()
{
    printf("=== pm_core benchmarks ===\n");
    printf("(warmup run + timed run per bench, reporting timed run only)\n");

    // Pool benchmarks
    bench_pool_add();
    bench_pool_get();
    bench_pool_remove();
    bench_pool_each();
    bench_pool_each_mut();
    bench_pool_clear();
    bench_pool_mixed();

    // State benchmarks
    bench_state();

    // Entity / kernel benchmarks
    bench_spawn();
    bench_flush();
    bench_entity_churn();

    // Integrated benchmarks
    bench_integrated_game_tick();
    bench_integrated_multi_archetype();
    bench_integrated_heavy_iteration();
    bench_integrated_sustained_churn();

    // Thread scaling
    bench_thread_scaling();

    // Hellfire game workload benchmarks
    bench_spatial_grid();
    bench_bullet_churn();
    bench_monster_ai();
    bench_collision_frame();
    bench_server_tick();
    bench_plc_utils();
    bench_multi_pool_tick();

    // Summary
    printf("\n=== Summary ===\n");
    printf("  %-50s %10s %12s\n", "Benchmark", "Total ms", "ns/op");
    printf("  %-50s %10s %12s\n",
           "--------------------------------------------------",
           "--------", "----------");
    for (auto &r : g_results)
        printf("  %-50s %8.2f ms %10.1f\n", r.name.c_str(), r.total_ms, r.ns_per_op);
    printf("\n=== All benchmarks complete ===\n");

    // Write CSV for regression tracking
    write_csv();

    return 0;
}
