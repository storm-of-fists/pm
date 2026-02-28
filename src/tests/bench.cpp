// bench.cpp — pm_core benchmarks as doctest TEST_CASEs with threshold checks.
//
// Each benchmark runs median-of-5 timing and CHECKs that ns/op stays
// below 2x the historical max. Thresholds calibrated from latest.csv
// on 20-core WSL (Ubuntu 24.04).
//
// Run:   ./build/pm_tests -ts=bench
// Skip:  ./build/pm_tests -tse=bench

#include "doctest/doctest.h"
#include "test_types.hpp"
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
#include <algorithm>

// =============================================================================
// Timing helpers
// =============================================================================

static constexpr int BENCH_ITERATIONS = 5;

struct BenchResult
{
    const char* name;
    double median_ms;
    double max_ms;
    uint64_t ops;
    double ns_per_op;
};

template <typename F>
BenchResult bench(const char *name, uint64_t ops, F &&fn)
{
    // Warmup
    fn();

    double times[BENCH_ITERATIONS];
    for (int i = 0; i < BENCH_ITERATIONS; i++)
    {
        auto t0 = std::chrono::high_resolution_clock::now();
        fn();
        auto t1 = std::chrono::high_resolution_clock::now();
        times[i] = std::chrono::duration<double, std::milli>(t1 - t0).count();
    }

    std::sort(times, times + BENCH_ITERATIONS);
    double median = times[BENCH_ITERATIONS / 2];
    double max_ms = times[BENCH_ITERATIONS - 1];
    double ns_op = (median * 1e6) / static_cast<double>(ops);

    printf("  %-50s %8.2f ms  %10.1f ns/op  (max %.2f)\n",
           name, median, ns_op, max_ms);
    return {name, median, max_ms, ops, ns_op};
}

// Overload for destructive operations: setup runs un-timed before each timed run.
template <typename S, typename F>
BenchResult bench(const char *name, uint64_t ops, S &&setup, F &&fn)
{
    // Warmup
    setup();
    fn();

    double times[BENCH_ITERATIONS];
    for (int i = 0; i < BENCH_ITERATIONS; i++)
    {
        setup();
        auto t0 = std::chrono::high_resolution_clock::now();
        fn();
        auto t1 = std::chrono::high_resolution_clock::now();
        times[i] = std::chrono::duration<double, std::milli>(t1 - t0).count();
    }

    std::sort(times, times + BENCH_ITERATIONS);
    double median = times[BENCH_ITERATIONS / 2];
    double max_ms = times[BENCH_ITERATIONS - 1];
    double ns_op = (median * 1e6) / static_cast<double>(ops);

    printf("  %-50s %8.2f ms  %10.1f ns/op  (max %.2f)\n",
           name, median, ns_op, max_ms);
    return {name, median, max_ms, ops, ns_op};
}

// =============================================================================
// Thresholds — 2x historical max ns/op (from benchmarks/latest.csv)
// =============================================================================

namespace threshold {
    // Pool: add
    constexpr double POOL_ADD_10K           = 220.0;
    constexpr double POOL_ADD_100K          = 136.0;
    constexpr double POOL_ADD_OVERWRITE     = 18.0;
    constexpr double POOL_ADD_BIG           = 1118.0;
    // Pool: get
    constexpr double POOL_GET_SEQ           = 5.2;
    constexpr double POOL_GET_STALE         = 2.1;
    constexpr double POOL_HAS               = 5.1;
    // Pool: remove
    constexpr double POOL_REMOVE_SWAP       = 10.0;
    constexpr double POOL_REMOVE_20POOLS    = 147.0;
    // Pool: each (read-only)
    constexpr double EACH_TRIVIAL_SEQ       = 4.0;
    constexpr double EACH_TRIVIAL_PAR       = 10.0;
    constexpr double EACH_TRIG_SEQ          = 110.0;
    constexpr double EACH_TRIG_PAR          = 22.0;
    constexpr double EACH_256B_SEQ          = 1096.0;
    constexpr double EACH_256B_PAR          = 89.0;
    constexpr double EACH_JOIN_SEQ          = 9.5;
    constexpr double EACH_JOIN_PAR          = 6.2;
    constexpr double EACH_500K_TRIG_SEQ     = 14.0;
    constexpr double EACH_500K_TRIG_PAR     = 3.0;
    // Pool: each_mut (mutable)
    constexpr double EACH_MUT_TRIV_SEQ      = 1.4;
    constexpr double EACH_MUT_TRIV_PAR      = 11.1;
    constexpr double EACH_MUT_PHYS_SEQ      = 24.2;
    constexpr double EACH_MUT_PHYS_PAR      = 9.5;
    constexpr double EACH_MUT_256B_SEQ      = 319.0;
    constexpr double EACH_MUT_256B_PAR      = 156.0;
    constexpr double EACH_MUT_JOIN_SEQ      = 36.0;
    constexpr double EACH_MUT_JOIN_PAR      = 10.2;
    constexpr double EACH_MUT_HOOK          = 2.4;
    constexpr double EACH_MUT_500K_SEQ      = 24.0;
    constexpr double EACH_MUT_500K_PAR      = 4.8;
    // Pool: clear
    constexpr double POOL_CLEAR             = 0.6;
    // Pool: mixed
    constexpr double POOL_MIXED             = 20.4;
    // State
    constexpr double STATE_FETCH            = 21.4;
    constexpr double STATE_CREATE           = 312.0;
    // Entity / kernel
    constexpr double ID_ADD_10K             = 3.4;
    constexpr double ID_ADD_100K            = 3.2;
    constexpr double ID_ADD_REUSE           = 14.0;
    constexpr double FLUSH_1POOL            = 8.9;
    constexpr double FLUSH_8POOLS           = 76.3;
    constexpr double FLUSH_20POOLS_SPARSE   = 54.8;
    constexpr double ENTITY_CHURN           = 71.1;
    // Integrated
    constexpr double GAME_TICK              = 3.4;
    constexpr double MULTI_ARCH             = 6.0;
    constexpr double JOIN_50K_SEQ           = 4.2;
    constexpr double JOIN_50K_PAR           = 22.5;
    constexpr double JOIN_10K_SMALL         = 1.0;
    constexpr double SUSTAINED_CHURN        = 103754.0;
    // Spatial grid
    constexpr double SPATIAL_INSERT_400     = 90.5;
    constexpr double SPATIAL_INSERT_4K      = 40.0;
    constexpr double SPATIAL_REBUILD_400    = 11.5;
    constexpr double SPATIAL_QUERY_SMALL    = 62.2;
    constexpr double SPATIAL_QUERY_LARGE    = 159.0;
    constexpr double SPATIAL_FULL_FRAME     = 74.2;
    // Bullet churn
    constexpr double BULLET_CHURN_50        = 35.0;
    constexpr double BULLET_CHURN_200       = 52.0;
    constexpr double BULLET_PHYSICS         = 37.4;
    // Monster AI
    constexpr double MONSTER_AI_400_SEQ     = 21.5;
    constexpr double MONSTER_AI_400_PAR     = 1288.0;
    constexpr double MONSTER_AI_2000        = 27.5;
    // Collision
    constexpr double COLLISION_MID          = 60.0;
    constexpr double COLLISION_PEAK         = 74.2;
    constexpr double COLLISION_BRUTE        = 0.8;
    // Server tick
    constexpr double SERVER_TICK_L1         = 92.0;
    constexpr double SERVER_TICK_L5         = 137.0;
    constexpr double SERVER_TICK_30         = 39.0;
    // PLC utils
    constexpr double COOLDOWN               = 0.94;
    constexpr double HYSTERESIS             = 1.82;
    constexpr double RISING_EDGE            = 0.62;
    constexpr double DELAY_TIMER            = 2.4;
    constexpr double COUNTER                = 2.0;
    // Multi-pool tick
    constexpr double MULTI_POOL_TICK        = 53.6;
    constexpr double MULTI_POOL_TICK_GRID   = 125.5;
}

// =============================================================================
// Component types for benchmarks
// =============================================================================

struct Damage    { int dmg = 10; };
struct Sprite    { uint32_t tex_id = 0; float u = 0, v = 0, w = 16, h = 16; };
struct Cooldown  { float remaining = 0; };
struct Team      { uint8_t team_id = 0; };
struct BigComp   { float data[64] = {}; }; // 256 bytes — cache pressure test

// Hellfire-accurate component types for game workload benchmarks
struct BMonster  { pm::Vec2 pos, vel; float shoot_timer = 0, size = 12; };
struct BBullet   { pm::Vec2 pos, vel; float lifetime = 1.5f, size = 4; bool player_owned = true; };
struct BPlayer   { pm::Vec2 pos; float hp = 100, cooldown = 0, invuln = 0; bool alive = true; };

// Helper to prevent compiler from optimizing away a value
template <typename T>
static void do_not_optimize(T const &val)
{
    asm volatile("" : : "r,m"(val) : "memory");
}

// State types for state benchmarks
struct GameConfig  { int width = 800, height = 600; float fov = 90; };

// =============================================================================
// BENCHMARKS
// =============================================================================

TEST_SUITE("bench") {

// --- Pool: add ---

TEST_CASE("pool add") {
    auto r1 = bench("pool add 10k", 10000, []() {
        pm::Pm pm;
        auto *pool = pm.pool_get<Pos>("pos");
        for (int i = 0; i < 10000; i++)
        {
            pm::Id id = pm.id_add();
            pool->add(id, {static_cast<float>(i), static_cast<float>(i)});
        }
    });
    CHECK(r1.ns_per_op < threshold::POOL_ADD_10K);

    auto r2 = bench("pool add 100k", 100000, []() {
        pm::Pm pm;
        auto *pool = pm.pool_get<Pos>("pos");
        for (int i = 0; i < 100000; i++)
        {
            pm::Id id = pm.id_add();
            pool->add(id, {static_cast<float>(i), static_cast<float>(i)});
        }
    });
    CHECK(r2.ns_per_op < threshold::POOL_ADD_100K);

    // add overwrite (entity already in pool)
    {
        pm::Pm pm;
        auto *pool = pm.pool_get<Pos>("pos");
        std::vector<pm::Id> ids;
        ids.reserve(10000);
        for (int i = 0; i < 10000; i++)
        {
            pm::Id id = pm.id_add();
            pool->add(id, {0, 0});
            ids.push_back(id);
        }
        auto r3 = bench("pool add overwrite 10k", 10000, [&]() {
            for (int i = 0; i < 10000; i++)
                pool->add(ids[static_cast<size_t>(i)], {static_cast<float>(i), static_cast<float>(i)});
        });
        CHECK(r3.ns_per_op < threshold::POOL_ADD_OVERWRITE);
    }

    auto r4 = bench("pool add 10k (256B component)", 10000, []() {
        pm::Pm pm;
        auto *pool = pm.pool_get<BigComp>("big");
        for (int i = 0; i < 10000; i++)
        {
            pm::Id id = pm.id_add();
            BigComp c{};
            c.data[0] = static_cast<float>(i);
            pool->add(id, c);
        }
    });
    CHECK(r4.ns_per_op < threshold::POOL_ADD_BIG);
}

// --- Pool: get ---

TEST_CASE("pool get") {
    pm::Pm pm;
    auto *pool = pm.pool_get<Pos>("pos");
    std::vector<pm::Id> ids;
    ids.reserve(10000);
    for (int i = 0; i < 10000; i++)
    {
        pm::Id id = pm.id_add();
        pool->add(id, {static_cast<float>(i), 0});
        ids.push_back(id);
    }

    auto r1 = bench("pool get 10k (sequential)", 10000, [&]() {
        float sum = 0;
        for (auto id : ids)
        {
            auto *p = pool->get(id);
            if (p) sum += p->x;
        }
        assert(sum > 0);
    });
    CHECK(r1.ns_per_op < threshold::POOL_GET_SEQ);

    // get with stale ids (should return nullptr)
    pm::Pm pm2;
    auto *pool2 = pm2.pool_get<Pos>("pos");
    std::vector<pm::Id> stale_ids;
    stale_ids.reserve(10000);
    for (int i = 0; i < 10000; i++)
    {
        pm::Id id = pm2.id_add();
        pool2->add(id, {static_cast<float>(i), 0});
        stale_ids.push_back(id);
    }
    for (auto id : stale_ids)
        pm2.id_remove(id);
    pm2.id_process_removes();

    auto r2 = bench("pool get 10k (stale ids — all miss)", 10000, [&]() {
        int misses = 0;
        for (auto id : stale_ids)
        {
            if (!pool2->get(id)) misses++;
        }
        assert(misses == 10000);
    });
    CHECK(r2.ns_per_op < threshold::POOL_GET_STALE);

    auto r3 = bench("pool has 10k", 10000, [&]() {
        int hits = 0;
        for (auto id : ids)
        {
            if (pool->has(id)) hits++;
        }
        assert(hits == 10000);
    });
    CHECK(r3.ns_per_op < threshold::POOL_HAS);
}

// --- Pool: remove ---

TEST_CASE("pool remove") {
    {
        pm::Pm pm;
        auto *pool = pm.pool_get<Pos>("pos");
        std::vector<pm::Id> ids;
        ids.reserve(10000);
        for (int i = 0; i < 10000; i++)
        {
            pm::Id id = pm.id_add();
            pool->add(id, {static_cast<float>(i), 0});
            ids.push_back(id);
        }
        auto r1 = bench("pool remove 10k (swap-and-pop)", 10000,
            [&]() {
                for (auto id : ids)
                    pool->add(id, {0, 0});
            },
            [&]() {
                for (auto id : ids)
                    pool->remove(id);
            });
        CHECK(r1.ns_per_op < threshold::POOL_REMOVE_SWAP);
    }

    {
        pm::Pm pm;
        auto *pos     = pm.pool_get<Pos>("pos");
        auto *vel     = pm.pool_get<Vel>("vel");
        auto *hp      = pm.pool_get<Health>("hp");
        auto *dmg     = pm.pool_get<Damage>("dmg");
        auto *spr     = pm.pool_get<Sprite>("spr");
        auto *cd      = pm.pool_get<Cooldown>("cd");
        auto *team    = pm.pool_get<Team>("team");
        auto *big     = pm.pool_get<BigComp>("big");

        // Create 12 more pools (empty, but present in pool_by_id for broadcast)
        pm.pool_get<Pos>("pos2");  pm.pool_get<Pos>("pos3");  pm.pool_get<Pos>("pos4");
        pm.pool_get<Pos>("pos5");  pm.pool_get<Pos>("pos6");  pm.pool_get<Pos>("pos7");
        pm.pool_get<Pos>("pos8");  pm.pool_get<Pos>("pos9");  pm.pool_get<Pos>("pos10");
        pm.pool_get<Pos>("pos11"); pm.pool_get<Pos>("pos12"); pm.pool_get<Pos>("pos13");

        std::vector<pm::Id> ids;
        ids.reserve(10000);

        auto r2 = bench("pool remove 10k via id_process_removes (20 pools)", 10000,
            [&]() {
                ids.clear();
                for (int i = 0; i < 10000; i++)
                {
                    pm::Id id = pm.id_add();
                    pos->add(id, {0, 0});
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
                    pm.id_remove(id);
            },
            [&]() {
                pm.id_process_removes();
            });
        CHECK(r2.ns_per_op < threshold::POOL_REMOVE_20POOLS);
    }
}

// --- Pool: each (read-only) ---

TEST_CASE("pool each") {
    // Trivial work (baseline)
    {
        pm::Pm pm;
        auto *pool = pm.pool_get<Pos>("pos");
        for (int i = 0; i < 100000; i++)
        {
            pm::Id id = pm.id_add();
            pool->add(id, {static_cast<float>(i), static_cast<float>(i)});
        }

        auto r1 = bench("each 100k trivial (seq)", 100000, [&]() {
            float sum = 0;
            pool->each([&](const Pos &p) { sum += p.x; }, pm::Parallel::Off);
            do_not_optimize(sum);
        });
        CHECK(r1.ns_per_op < threshold::EACH_TRIVIAL_SEQ);

        auto r2 = bench("each 100k trivial (parallel)", 100000, [&]() {
            pool->each([](const Pos &p) { do_not_optimize(p.x); }, pm::Parallel::On);
        });
        CHECK(r2.ns_per_op < threshold::EACH_TRIVIAL_PAR);
    }

    // Medium work: trig per element
    {
        pm::Pm pm;
        auto *pool = pm.pool_get<Pos>("pos");
        for (int i = 0; i < 100000; i++)
        {
            pm::Id id = pm.id_add();
            pool->add(id, {static_cast<float>(i) * 0.01f, static_cast<float>(i) * 0.02f});
        }

        auto r1 = bench("each 100k trig (seq)", 100000, [&]() {
            pool->each([](const Pos &p) {
                float d = sqrtf(p.x * p.x + p.y * p.y);
                float a = atan2f(p.y, p.x);
                float r = sinf(a) * cosf(d);
                do_not_optimize(r);
            }, pm::Parallel::Off);
        });
        CHECK(r1.ns_per_op < threshold::EACH_TRIG_SEQ);

        auto r2 = bench("each 100k trig (parallel)", 100000, [&]() {
            pool->each([](const Pos &p) {
                float d = sqrtf(p.x * p.x + p.y * p.y);
                float a = atan2f(p.y, p.x);
                float r = sinf(a) * cosf(d);
                do_not_optimize(r);
            }, pm::Parallel::On);
        });
        CHECK(r2.ns_per_op < threshold::EACH_TRIG_PAR);
    }

    // Heavy work: 256B component
    {
        pm::Pm pm;
        auto *pool = pm.pool_get<BigComp>("big");
        for (int i = 0; i < 100000; i++)
        {
            pm::Id id = pm.id_add();
            BigComp c{};
            for (int j = 0; j < 64; j++)
                c.data[j] = static_cast<float>(i * 64 + j) * 0.001f;
            pool->add(id, c);
        }

        auto r1 = bench("each 100k 256B read-all (seq)", 100000, [&]() {
            pool->each([](const BigComp &c) {
                float sum = 0;
                for (int j = 0; j < 64; j++)
                    sum += sinf(c.data[j]);
                do_not_optimize(sum);
            }, pm::Parallel::Off);
        });
        CHECK(r1.ns_per_op < threshold::EACH_256B_SEQ);

        auto r2 = bench("each 100k 256B read-all (parallel)", 100000, [&]() {
            pool->each([](const BigComp &c) {
                float sum = 0;
                for (int j = 0; j < 64; j++)
                    sum += sinf(c.data[j]);
                do_not_optimize(sum);
            }, pm::Parallel::On);
        });
        CHECK(r2.ns_per_op < threshold::EACH_256B_PAR);
    }

    // Cross-pool join
    {
        pm::Pm pm;
        auto *pos = pm.pool_get<Pos>("pos");
        auto *vel = pm.pool_get<Vel>("vel");
        auto *hp  = pm.pool_get<Health>("hp");
        for (int i = 0; i < 100000; i++)
        {
            pm::Id id = pm.id_add();
            pos->add(id, {static_cast<float>(i), static_cast<float>(i)});
            vel->add(id, {1.0f, 0.5f});
            if (i % 3 == 0) hp->add(id, {100 + i % 50});
        }

        auto r1 = bench("each 100k join 2 pools + branch (seq)", 100000, [&]() {
            pos->each([&](pm::Id id, const Pos &p) {
                auto *v = vel->get(id);
                auto *h = hp->get(id);
                float r = p.x + p.y;
                if (v) r += sqrtf(v->dx * v->dx + v->dy * v->dy);
                if (h) r *= static_cast<float>(h->hp) * 0.01f;
                do_not_optimize(r);
            }, pm::Parallel::Off);
        });
        CHECK(r1.ns_per_op < threshold::EACH_JOIN_SEQ);

        auto r2 = bench("each 100k join 2 pools + branch (parallel)", 100000, [&]() {
            pos->each([&](pm::Id id, const Pos &p) {
                auto *v = vel->get(id);
                auto *h = hp->get(id);
                float r = p.x + p.y;
                if (v) r += sqrtf(v->dx * v->dx + v->dy * v->dy);
                if (h) r *= static_cast<float>(h->hp) * 0.01f;
                do_not_optimize(r);
            }, pm::Parallel::On);
        });
        CHECK(r2.ns_per_op < threshold::EACH_JOIN_PAR);
    }

    // Scale: 500k
    {
        pm::Pm pm;
        auto *pool = pm.pool_get<Pos>("pos");
        for (int i = 0; i < 500000; i++)
        {
            pm::Id id = pm.id_add();
            pool->add(id, {static_cast<float>(i) * 0.01f, static_cast<float>(i) * 0.02f});
        }

        auto r1 = bench("each 500k trig (seq)", 500000, [&]() {
            pool->each([](const Pos &p) {
                float d = sqrtf(p.x * p.x + p.y * p.y);
                float r = sinf(d) * cosf(d);
                do_not_optimize(r);
            }, pm::Parallel::Off);
        });
        CHECK(r1.ns_per_op < threshold::EACH_500K_TRIG_SEQ);

        auto r2 = bench("each 500k trig (parallel)", 500000, [&]() {
            pool->each([](const Pos &p) {
                float d = sqrtf(p.x * p.x + p.y * p.y);
                float r = sinf(d) * cosf(d);
                do_not_optimize(r);
            }, pm::Parallel::On);
        });
        CHECK(r2.ns_per_op < threshold::EACH_500K_TRIG_PAR);
    }
}

// --- Pool: each_mut (mutable) ---

TEST_CASE("pool each_mut") {
    // Trivial work
    {
        pm::Pm pm;
        auto *pool = pm.pool_get<Pos>("pos");
        for (int i = 0; i < 100000; i++)
        {
            pm::Id id = pm.id_add();
            pool->add(id, {static_cast<float>(i), static_cast<float>(i)});
        }

        auto r1 = bench("each_mut 100k trivial (seq)", 100000, [&]() {
            pool->each_mut([](Pos &p) { p.x += 1.0f; }, pm::Parallel::Off);
        });
        CHECK(r1.ns_per_op < threshold::EACH_MUT_TRIV_SEQ);

        auto r2 = bench("each_mut 100k trivial (parallel)", 100000, [&]() {
            pool->each_mut([](Pos &p) { p.x += 1.0f; }, pm::Parallel::On);
        });
        CHECK(r2.ns_per_op < threshold::EACH_MUT_TRIV_PAR);
    }

    // Physics sim
    {
        pm::Pm pm;
        auto *pool = pm.pool_get<Pos>("pos");
        for (int i = 0; i < 100000; i++)
        {
            pm::Id id = pm.id_add();
            pool->add(id, {static_cast<float>(i) * 0.1f, static_cast<float>(i) * 0.05f});
        }

        auto r1 = bench("each_mut 100k physics sim (seq)", 100000, [&]() {
            pool->each_mut([](Pos &p) {
                float angle = atan2f(p.y, p.x);
                float dist = sqrtf(p.x * p.x + p.y * p.y);
                angle += 0.01f;
                p.x = cosf(angle) * dist;
                p.y = sinf(angle) * dist;
            }, pm::Parallel::Off);
        });
        CHECK(r1.ns_per_op < threshold::EACH_MUT_PHYS_SEQ);

        auto r2 = bench("each_mut 100k physics sim (parallel)", 100000, [&]() {
            pool->each_mut([](Pos &p) {
                float angle = atan2f(p.y, p.x);
                float dist = sqrtf(p.x * p.x + p.y * p.y);
                angle += 0.01f;
                p.x = cosf(angle) * dist;
                p.y = sinf(angle) * dist;
            }, pm::Parallel::On);
        });
        CHECK(r2.ns_per_op < threshold::EACH_MUT_PHYS_PAR);
    }

    // 256B transform
    {
        pm::Pm pm;
        auto *pool = pm.pool_get<BigComp>("big");
        for (int i = 0; i < 100000; i++)
        {
            pm::Id id = pm.id_add();
            BigComp c{};
            for (int j = 0; j < 64; j++)
                c.data[j] = static_cast<float>(i * 64 + j) * 0.001f;
            pool->add(id, c);
        }

        auto r1 = bench("each_mut 100k 256B transform (seq)", 100000, [&]() {
            pool->each_mut([](BigComp &c) {
                for (int j = 0; j < 64; j++)
                    c.data[j] = sinf(c.data[j]) * 0.99f + 0.01f;
            }, pm::Parallel::Off);
        });
        CHECK(r1.ns_per_op < threshold::EACH_MUT_256B_SEQ);

        auto r2 = bench("each_mut 100k 256B transform (parallel)", 100000, [&]() {
            pool->each_mut([](BigComp &c) {
                for (int j = 0; j < 64; j++)
                    c.data[j] = sinf(c.data[j]) * 0.99f + 0.01f;
            }, pm::Parallel::On);
        });
        CHECK(r2.ns_per_op < threshold::EACH_MUT_256B_PAR);
    }

    // Cross-pool join
    {
        pm::Pm pm;
        auto *pos = pm.pool_get<Pos>("pos");
        auto *vel = pm.pool_get<Vel>("vel");
        for (int i = 0; i < 100000; i++)
        {
            pm::Id id = pm.id_add();
            pos->add(id, {static_cast<float>(i) * 0.1f, static_cast<float>(i) * 0.05f});
            vel->add(id, {sinf(static_cast<float>(i)), cosf(static_cast<float>(i))});
        }

        auto r1 = bench("each_mut 100k join + physics (seq)", 100000, [&]() {
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
        CHECK(r1.ns_per_op < threshold::EACH_MUT_JOIN_SEQ);

        auto r2 = bench("each_mut 100k join + physics (parallel)", 100000, [&]() {
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
        CHECK(r2.ns_per_op < threshold::EACH_MUT_JOIN_PAR);
    }

    // Change hook overhead
    {
        pm::Pm pm;
        auto *pool = pm.pool_get<Pos>("pos");
        for (int i = 0; i < 100000; i++)
        {
            pm::Id id = pm.id_add();
            pool->add(id, {static_cast<float>(i), static_cast<float>(i)});
        }

        int hook_count = 0;
        pool->set_change_hook([](void *ctx, pm::Id) {
            (*static_cast<int *>(ctx))++;
        }, &hook_count);

        auto r = bench("each_mut 100k with change hook", 100000, [&]() {
            hook_count = 0;
            pool->each_mut([](Pos &p) { p.x += 1.0f; });
            assert(hook_count == 100000);
        });
        CHECK(r.ns_per_op < threshold::EACH_MUT_HOOK);

        pool->set_change_hook(nullptr, nullptr);
    }

    // Scale: 500k
    {
        pm::Pm pm;
        auto *pool = pm.pool_get<Pos>("pos");
        for (int i = 0; i < 500000; i++)
        {
            pm::Id id = pm.id_add();
            pool->add(id, {static_cast<float>(i) * 0.1f, static_cast<float>(i) * 0.05f});
        }

        auto r1 = bench("each_mut 500k physics sim (seq)", 500000, [&]() {
            pool->each_mut([](Pos &p) {
                float angle = atan2f(p.y, p.x);
                float dist = sqrtf(p.x * p.x + p.y * p.y);
                angle += 0.01f;
                p.x = cosf(angle) * dist;
                p.y = sinf(angle) * dist;
            }, pm::Parallel::Off);
        });
        CHECK(r1.ns_per_op < threshold::EACH_MUT_500K_SEQ);

        auto r2 = bench("each_mut 500k physics sim (parallel)", 500000, [&]() {
            pool->each_mut([](Pos &p) {
                float angle = atan2f(p.y, p.x);
                float dist = sqrtf(p.x * p.x + p.y * p.y);
                angle += 0.01f;
                p.x = cosf(angle) * dist;
                p.y = sinf(angle) * dist;
            }, pm::Parallel::On);
        });
        CHECK(r2.ns_per_op < threshold::EACH_MUT_500K_PAR);
    }
}

// --- Pool: clear ---

TEST_CASE("pool clear") {
    pm::Pm pm;
    auto *pool = pm.pool_get<Pos>("pos");
    std::vector<pm::Id> ids;
    ids.reserve(100000);
    for (int i = 0; i < 100000; i++)
        ids.push_back(pm.id_add());

    auto r = bench("pool clear_all 100k", 100000,
        [&]() {
            for (int i = 0; i < 100000; i++)
                pool->add(ids[static_cast<size_t>(i)], {static_cast<float>(i), 0});
        },
        [&]() {
            pool->clear_all();
        });
    CHECK(r.ns_per_op < threshold::POOL_CLEAR);
}

// --- Pool: mixed ---

TEST_CASE("pool mixed ops") {
    auto r = bench("pool mixed ops 10k (add, get, remove interleaved)", 30000, []() {
        pm::Pm pm;
        auto *pool = pm.pool_get<Pos>("pos");
        std::vector<pm::Id> ids;
        ids.reserve(10000);

        for (int i = 0; i < 10000; i++)
        {
            pm::Id id = pm.id_add();
            pool->add(id, {static_cast<float>(i), 0});
            ids.push_back(id);
        }

        float sum = 0;
        for (auto id : ids)
        {
            auto *p = pool->get(id);
            if (p) sum += p->x;
        }
        assert(sum > 0);

        for (auto id : ids)
            pm.id_remove(id);
        pm.id_process_removes();
        assert(pool->size() == 0);
    });
    CHECK(r.ns_per_op < threshold::POOL_MIXED);
}

// --- State ---

TEST_CASE("state") {
    auto r1 = bench("state fetch 10k (same state, repeated)", 10000, []() {
        pm::Pm pm;
        auto *cfg = pm.state_get<GameConfig>("config");
        cfg->width = 1920;
        for (int i = 0; i < 10000; i++)
        {
            auto *c = pm.state_get<GameConfig>("config");
            assert(c->width == 1920);
        }
    });
    CHECK(r1.ns_per_op < threshold::STATE_FETCH);

    auto r2 = bench("state create 100 distinct states", 100, []() {
        pm::Pm pm;
        char buf[32];
        for (int i = 0; i < 100; i++)
        {
            snprintf(buf, sizeof(buf), "state_%d", i);
            auto *s = pm.state_get<GameConfig>(buf);
            s->width = i;
        }
    });
    CHECK(r2.ns_per_op < threshold::STATE_CREATE);
}

// --- Entity: id_add ---

TEST_CASE("id_add") {
    auto r1 = bench("id_add10k", 10000, []() {
        pm::Pm pm;
        for (int i = 0; i < 10000; i++)
            pm.id_add();
    });
    CHECK(r1.ns_per_op < threshold::ID_ADD_10K);

    auto r2 = bench("id_add100k", 100000, []() {
        pm::Pm pm;
        for (int i = 0; i < 100000; i++)
            pm.id_add();
    });
    CHECK(r2.ns_per_op < threshold::ID_ADD_100K);

    // Spawn with free-list reuse
    {
        pm::Pm pm;
        std::vector<pm::Id> ids;
        ids.reserve(10000);
        for (int i = 0; i < 10000; i++)
            ids.push_back(pm.id_add());

        auto r3 = bench("id_add10k after remove (free-list reuse)", 10000,
            [&]() {
                for (auto id : ids)
                    pm.id_remove(id);
                pm.id_process_removes();
            },
            [&]() {
                ids.clear();
                for (int i = 0; i < 10000; i++)
                    ids.push_back(pm.id_add());
            });
        CHECK(r3.ns_per_op < threshold::ID_ADD_REUSE);
    }
}

// --- Entity: id_process_removes ---

TEST_CASE("flush") {
    // 1 pool
    {
        pm::Pm pm;
        auto *pool = pm.pool_get<Pos>("pos");
        std::vector<pm::Id> ids;
        ids.reserve(10000);

        auto r = bench("flush 10k removes (1 pool)", 10000,
            [&]() {
                ids.clear();
                for (int i = 0; i < 10000; i++)
                {
                    pm::Id id = pm.id_add();
                    pool->add(id, {0, 0});
                    ids.push_back(id);
                }
                for (auto id : ids)
                    pm.id_remove(id);
            },
            [&]() {
                pm.id_process_removes();
            });
        CHECK(r.ns_per_op < threshold::FLUSH_1POOL);
    }

    // 8 pools
    {
        pm::Pm pm;
        auto *p1 = pm.pool_get<Pos>("pos");
        auto *p2 = pm.pool_get<Vel>("vel");
        auto *p3 = pm.pool_get<Health>("hp");
        auto *p4 = pm.pool_get<Damage>("dmg");
        auto *p5 = pm.pool_get<Sprite>("spr");
        auto *p6 = pm.pool_get<Cooldown>("cd");
        auto *p7 = pm.pool_get<Team>("team");
        auto *p8 = pm.pool_get<BigComp>("big");

        std::vector<pm::Id> ids;
        ids.reserve(10000);

        auto r = bench("flush 10k removes (8 pools)", 10000,
            [&]() {
                ids.clear();
                for (int i = 0; i < 10000; i++)
                {
                    pm::Id id = pm.id_add();
                    p1->add(id, {0, 0});
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
                    pm.id_remove(id);
            },
            [&]() {
                pm.id_process_removes();
            });
        CHECK(r.ns_per_op < threshold::FLUSH_8POOLS);
    }

    // 20 pools, sparse
    {
        pm::Pm pm;
        auto *pos  = pm.pool_get<Pos>("pos");
        auto *vel  = pm.pool_get<Vel>("vel");
        auto *hp   = pm.pool_get<Health>("hp");
        pm.pool_get<Pos>("e1");  pm.pool_get<Pos>("e2");  pm.pool_get<Pos>("e3");
        pm.pool_get<Pos>("e4");  pm.pool_get<Pos>("e5");  pm.pool_get<Pos>("e6");
        pm.pool_get<Pos>("e7");  pm.pool_get<Pos>("e8");  pm.pool_get<Pos>("e9");
        pm.pool_get<Pos>("e10"); pm.pool_get<Pos>("e11"); pm.pool_get<Pos>("e12");
        pm.pool_get<Pos>("e13"); pm.pool_get<Pos>("e14"); pm.pool_get<Pos>("e15");
        pm.pool_get<Pos>("e16"); pm.pool_get<Pos>("e17");

        std::vector<pm::Id> ids;
        ids.reserve(1000);

        auto r = bench("flush 1k removes (20 pools, 3 populated)", 1000,
            [&]() {
                ids.clear();
                for (int i = 0; i < 1000; i++)
                {
                    pm::Id id = pm.id_add();
                    pos->add(id, {0, 0});
                    vel->add(id, {1, 0});
                    hp->add(id, {100});
                    ids.push_back(id);
                }
                for (auto id : ids)
                    pm.id_remove(id);
            },
            [&]() {
                pm.id_process_removes();
            });
        CHECK(r.ns_per_op < threshold::FLUSH_20POOLS_SPARSE);
    }
}

// --- Entity: churn ---

TEST_CASE("entity churn") {
    auto r = bench("entity churn 10k (spawn, add 3 components, remove, repeat)", 10000, []() {
        pm::Pm pm;
        auto *pos = pm.pool_get<Pos>("pos");
        auto *vel = pm.pool_get<Vel>("vel");
        auto *hp  = pm.pool_get<Health>("hp");

        for (int i = 0; i < 10000; i++)
        {
            pm::Id id = pm.id_add();
            pos->add(id, {static_cast<float>(i), 0});
            vel->add(id, {1, 1});
            hp->add(id, {100});
            pm.id_remove(id);
            if (i % 100 == 99)
                pm.id_process_removes();
        }
        pm.id_process_removes();
        assert(pos->size() == 0);
    });
    CHECK(r.ns_per_op < threshold::ENTITY_CHURN);
}

// --- Integrated: game tick ---

TEST_CASE("integrated game tick") {
    pm::Pm pm;
    auto *pos = pm.pool_get<Pos>("pos");
    auto *vel = pm.pool_get<Vel>("vel");
    auto *hp  = pm.pool_get<Health>("hp");

    for (int i = 0; i < 5000; i++)
    {
        pm::Id id = pm.id_add();
        pos->add(id, {static_cast<float>(i % 100), static_cast<float>(i / 100)});
        vel->add(id, {1.0f, 0.5f});
        if (i < 500) hp->add(id, {100});
    }

    float dt = 0.016f;

    auto r = bench("game tick: 5k physics + 500 health checks", 5500, [&]() {
        pos->each_mut([&](pm::Id id, Pos &p) {
            auto *v = vel->get(id);
            if (v)
            {
                p.x += v->dx * dt;
                p.y += v->dy * dt;
            }
        }, pm::Parallel::Off);

        hp->each([&](pm::Id id, const Health &h) {
            (void)h;
            auto *p = pos->get(id);
            if (p && p->x > 50.0f) {}
        }, pm::Parallel::Off);
    });
    CHECK(r.ns_per_op < threshold::GAME_TICK);
}

// --- Integrated: multi-archetype ---

TEST_CASE("integrated multi-archetype") {
    pm::Pm pm;
    auto *pos  = pm.pool_get<Pos>("pos");
    auto *vel  = pm.pool_get<Vel>("vel");
    auto *hp   = pm.pool_get<Health>("hp");
    auto *dmg  = pm.pool_get<Damage>("dmg");
    auto *spr  = pm.pool_get<Sprite>("spr");
    auto *cd   = pm.pool_get<Cooldown>("cd");
    auto *team = pm.pool_get<Team>("team");
    auto *big  = pm.pool_get<BigComp>("big");

    for (int i = 0; i < 100; i++)
    {
        pm::Id id = pm.id_add();
        pos->add(id, {static_cast<float>(i * 10), 0});
        vel->add(id, {0, 0});
        hp->add(id, {200});
        team->add(id, {static_cast<uint8_t>(i % 2)});
    }

    std::vector<pm::Id> bullet_ids;
    bullet_ids.reserve(2000);
    for (int i = 0; i < 2000; i++)
    {
        pm::Id id = pm.id_add();
        pos->add(id, {static_cast<float>(i), static_cast<float>(i % 50)});
        vel->add(id, {10.0f, 0});
        dmg->add(id, {25});
        cd->add(id, {1.0f});
        bullet_ids.push_back(id);
    }

    for (int i = 0; i < 500; i++)
    {
        pm::Id id = pm.id_add();
        pos->add(id, {static_cast<float>(i * 5), static_cast<float>(i * 2)});
        hp->add(id, {150});
        spr->add(id, {1, 0, 0, 32, 32});
        team->add(id, {2});
    }

    for (int i = 0; i < 200; i++)
    {
        pm::Id id = pm.id_add();
        pos->add(id, {static_cast<float>(i * 20), 100});
        spr->add(id, {2, 0, 0, 16, 16});
    }

    for (int i = 0; i < 300; i++)
    {
        pm::Id id = pm.id_add();
        pos->add(id, {static_cast<float>(i * 3), 0});
        big->add(id);
    }

    float dt = 0.016f;

    auto r = bench("multi-archetype: 3100 entities, 8 pools, tick sim", 3100, [&]() {
        pos->each_mut([&](pm::Id id, Pos &p) {
            auto *v = vel->get(id);
            if (v) { p.x += v->dx * dt; p.y += v->dy * dt; }
        }, pm::Parallel::Off);

        cd->each_mut([&](Cooldown &c) {
            c.remaining -= dt;
            if (c.remaining < 0) c.remaining = 0;
        }, pm::Parallel::Off);

        for (auto bid : bullet_ids)
        {
            auto *c = cd->get(bid);
            if (c && c->remaining <= 0)
                pm.id_remove(bid);
        }

        pm.id_process_removes();
    });
    CHECK(r.ns_per_op < threshold::MULTI_ARCH);
}

// --- Integrated: heavy iteration ---

TEST_CASE("integrated heavy iteration") {
    pm::Pm pm;
    auto *pos = pm.pool_get<Pos>("pos");
    auto *vel = pm.pool_get<Vel>("vel");
    auto *hp  = pm.pool_get<Health>("hp");

    for (int i = 0; i < 50000; i++)
    {
        pm::Id id = pm.id_add();
        pos->add(id, {static_cast<float>(i), static_cast<float>(i)});
        vel->add(id, {1, 1});
        if (i < 10000) hp->add(id, {100});
    }

    auto r1 = bench("iterate 50k pos, lookup vel (join pattern, seq)", 50000, [&]() {
        float dt = 0.016f;
        pos->each_mut([&](pm::Id id, Pos &p) {
            auto *v = vel->get(id);
            if (v) { p.x += v->dx * dt; p.y += v->dy * dt; }
        }, pm::Parallel::Off);
    });
    CHECK(r1.ns_per_op < threshold::JOIN_50K_SEQ);

    auto r2 = bench("iterate 50k pos, lookup vel (join pattern, parallel)", 50000, [&]() {
        float dt = 0.016f;
        pos->each_mut([&](pm::Id id, Pos &p) {
            auto *v = vel->get(id);
            if (v) { p.x += v->dx * dt; p.y += v->dy * dt; }
        }, pm::Parallel::On);
    });
    CHECK(r2.ns_per_op < threshold::JOIN_50K_PAR);

    auto r3 = bench("iterate 10k health, lookup pos (smaller iterates larger)", 10000, [&]() {
        hp->each([&](pm::Id id, const Health &h) {
            (void)h;
            auto *p = pos->get(id);
            if (p) { /* read position */ }
        }, pm::Parallel::Off);
    });
    CHECK(r3.ns_per_op < threshold::JOIN_10K_SMALL);
}

// --- Integrated: sustained churn ---

TEST_CASE("integrated sustained churn") {
    auto r = bench("30 frames: 1k spawn + 1k remove + iterate 5k", 30, []() {
        pm::Pm pm;
        auto *pos = pm.pool_get<Pos>("pos");
        auto *vel = pm.pool_get<Vel>("vel");
        auto *hp  = pm.pool_get<Health>("hp");

        std::vector<pm::Id> live_ids;
        live_ids.reserve(10000);
        for (int i = 0; i < 5000; i++)
        {
            pm::Id id = pm.id_add();
            pos->add(id, {static_cast<float>(i), static_cast<float>(i)});
            vel->add(id, {1, 0});
            if (i % 5 == 0) hp->add(id, {100});
            live_ids.push_back(id);
        }

        float dt = 0.016f;

        for (int frame = 0; frame < 30; frame++)
        {
            pos->each_mut([&](pm::Id id, Pos &p) {
                auto *v = vel->get(id);
                if (v) { p.x += v->dx * dt; p.y += v->dy * dt; }
            }, pm::Parallel::Off);

            int to_remove = std::min(1000, static_cast<int>(live_ids.size()));
            for (int i = 0; i < to_remove; i++)
                pm.id_remove(live_ids[static_cast<size_t>(i)]);
            live_ids.erase(live_ids.begin(), live_ids.begin() + to_remove);
            pm.id_process_removes();

            for (int i = 0; i < 1000; i++)
            {
                pm::Id id = pm.id_add();
                pos->add(id, {static_cast<float>(frame * 1000 + i), 0});
                vel->add(id, {1, 0});
                if (i % 5 == 0) hp->add(id, {100});
                live_ids.push_back(id);
            }
        }
    });
    CHECK(r.ns_per_op < threshold::SUSTAINED_CHURN);
}

// --- Thread scaling (informational — no thresholds) ---

TEST_CASE("thread scaling") {
    uint32_t hw = std::thread::hardware_concurrency();
    if (hw == 0) hw = 4;

    std::vector<uint32_t> counts;
    for (uint32_t n = 1; n <= hw; n *= 2)
        counts.push_back(n);
    if (counts.back() != hw)
        counts.push_back(hw);

    pm::Pm pm;

    printf("\n  (hardware_concurrency = %u)\n", hw);

    {
        auto *pool = pm.pool_get<Pos>("pos_scale1");
        for (int i = 0; i < 200000; i++)
        {
            pm::Id id = pm.id_add();
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

    {
        auto *pool = pm.pool_get<Pos>("pos_scale2");
        for (int i = 0; i < 200000; i++)
        {
            pm::Id id = pm.id_add();
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

    {
        auto *pool = pm.pool_get<BigComp>("big_scale");
        for (int i = 0; i < 100000; i++)
        {
            pm::Id id = pm.id_add();
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

// --- Spatial grid ---

TEST_CASE("spatial grid") {
    auto r1 = bench("spatial insert 400 (hellfire peak)", 400, []() {
        pm::SpatialGrid grid(900, 700, 64);
        pm::Rng rng{42};
        for (int i = 0; i < 400; i++) {
            pm::Id id = pm::Id(static_cast<uint64_t>(i) << 16);
            grid.insert(id, {rng.rfr(0, 900), rng.rfr(0, 700)});
        }
    });
    CHECK(r1.ns_per_op < threshold::SPATIAL_INSERT_400);

    auto r2 = bench("spatial insert 4k (10x stress)", 4000, []() {
        pm::SpatialGrid grid(900, 700, 64);
        pm::Rng rng{42};
        for (int i = 0; i < 4000; i++) {
            pm::Id id = pm::Id(static_cast<uint64_t>(i) << 16);
            grid.insert(id, {rng.rfr(0, 900), rng.rfr(0, 700)});
        }
    });
    CHECK(r2.ns_per_op < threshold::SPATIAL_INSERT_4K);

    auto r3 = bench("spatial clear + insert 400 (per-frame rebuild)", 400, []() {
        static pm::SpatialGrid grid(900, 700, 64);
        pm::Rng rng{42};
        grid.clear();
        for (int i = 0; i < 400; i++) {
            pm::Id id = pm::Id(static_cast<uint64_t>(i) << 16);
            grid.insert(id, {rng.rfr(0, 900), rng.rfr(0, 700)});
        }
    });
    CHECK(r3.ns_per_op < threshold::SPATIAL_REBUILD_400);

    // Queries
    {
        pm::SpatialGrid grid(900, 700, 64);
        pm::Rng rng{42};
        for (int i = 0; i < 400; i++) {
            pm::Id id = pm::Id(static_cast<uint64_t>(i) << 16);
            grid.insert(id, {rng.rfr(0, 900), rng.rfr(0, 700)});
        }

        auto r4 = bench("spatial query small radius (r=20, 1k queries)", 1000, [&]() {
            pm::Rng qrng{99};
            int hits = 0;
            for (int i = 0; i < 1000; i++) {
                pm::Vec2 c = {qrng.rfr(0, 900), qrng.rfr(0, 700)};
                grid.query(c, 20.f, [&](pm::Id, pm::Vec2) { hits++; });
            }
            do_not_optimize(hits);
        });
        CHECK(r4.ns_per_op < threshold::SPATIAL_QUERY_SMALL);

        auto r5 = bench("spatial query large radius (r=100, 1k queries)", 1000, [&]() {
            pm::Rng qrng{99};
            int hits = 0;
            for (int i = 0; i < 1000; i++) {
                pm::Vec2 c = {qrng.rfr(0, 900), qrng.rfr(0, 700)};
                grid.query(c, 100.f, [&](pm::Id, pm::Vec2) { hits++; });
            }
            do_not_optimize(hits);
        });
        CHECK(r5.ns_per_op < threshold::SPATIAL_QUERY_LARGE);
    }

    auto r6 = bench("spatial full frame (400 insert + 600 queries)", 1000, []() {
        pm::SpatialGrid grid(900, 700, 64);
        pm::Rng rng{42};
        for (int i = 0; i < 400; i++) {
            pm::Id id = pm::Id(static_cast<uint64_t>(i) << 16);
            grid.insert(id, {rng.rfr(0, 900), rng.rfr(0, 700)});
        }
        int hits = 0;
        for (int i = 0; i < 600; i++) {
            pm::Vec2 c = {rng.rfr(0, 900), rng.rfr(0, 700)};
            grid.query(c, 20.f, [&](pm::Id, pm::Vec2) { hits++; });
        }
        do_not_optimize(hits);
    });
    CHECK(r6.ns_per_op < threshold::SPATIAL_FULL_FRAME);
}

// --- Bullet churn ---

TEST_CASE("bullet churn") {
    auto r1 = bench("bullet churn: 30f, 50 spawn + 40 expire/f", 30 * 90, []() {
        pm::Pm pm;
        auto* bp = pm.pool_get<BBullet>("bullets");
        pm::Rng rng{42};
        std::vector<pm::Id> live;
        live.reserve(600);

        for (int i = 0; i < 200; i++) {
            pm::Id id = pm.id_add();
            bp->add(id, BBullet{{rng.rfr(0, 900), rng.rfr(0, 700)},
                                {rng.rfr(-750, 750), rng.rfr(-750, 750)},
                                rng.rfr(0.5f, 1.5f), 4, true});
            live.push_back(id);
        }

        for (int f = 0; f < 30; f++) {
            int to_remove = std::min(40, static_cast<int>(live.size()));
            for (int i = 0; i < to_remove; i++)
                pm.id_remove(live[static_cast<size_t>(i)]);
            live.erase(live.begin(), live.begin() + to_remove);
            pm.id_process_removes();

            for (int i = 0; i < 50; i++) {
                pm::Id id = pm.id_add();
                bp->add(id, BBullet{{rng.rfr(0, 900), rng.rfr(0, 700)},
                                    {rng.rfr(-750, 750), rng.rfr(-750, 750)},
                                    rng.rfr(0.5f, 1.5f), 4, true});
                live.push_back(id);
            }
        }
    });
    CHECK(r1.ns_per_op < threshold::BULLET_CHURN_50);

    auto r2 = bench("bullet churn: 30f, 200 spawn + 180 expire/f", 30 * 380, []() {
        pm::Pm pm;
        auto* bp = pm.pool_get<BBullet>("bullets");
        pm::Rng rng{42};
        std::vector<pm::Id> live;
        live.reserve(2000);

        for (int i = 0; i < 400; i++) {
            pm::Id id = pm.id_add();
            bp->add(id, BBullet{{rng.rfr(0, 900), rng.rfr(0, 700)},
                                {rng.rfr(-750, 750), rng.rfr(-750, 750)},
                                rng.rfr(0.5f, 1.5f), 4, true});
            live.push_back(id);
        }

        for (int f = 0; f < 30; f++) {
            int to_remove = std::min(180, static_cast<int>(live.size()));
            for (int i = 0; i < to_remove; i++)
                pm.id_remove(live[static_cast<size_t>(i)]);
            live.erase(live.begin(), live.begin() + to_remove);
            pm.id_process_removes();

            for (int i = 0; i < 200; i++) {
                pm::Id id = pm.id_add();
                bp->add(id, BBullet{{rng.rfr(0, 900), rng.rfr(0, 700)},
                                    {rng.rfr(-750, 750), rng.rfr(-750, 750)},
                                    rng.rfr(0.5f, 1.5f), 4, true});
                live.push_back(id);
            }
        }
    });
    CHECK(r2.ns_per_op < threshold::BULLET_CHURN_200);

    auto r3 = bench("bullet physics: each_mut 600 (pos += vel*dt)", 600, []() {
        pm::Pm pm;
        auto* bp = pm.pool_get<BBullet>("bullets");
        pm::Rng rng{42};
        for (int i = 0; i < 600; i++) {
            pm::Id id = pm.id_add();
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
    CHECK(r3.ns_per_op < threshold::BULLET_PHYSICS);
}

// --- Monster AI ---

TEST_CASE("monster AI") {
    static const pm::Vec2 player_pos[4] = {
        {225, 280}, {675, 280}, {225, 420}, {675, 420}
    };

    // 400 seq
    {
        pm::Pm pm;
        auto* mp = pm.pool_get<BMonster>("monsters");
        pm::Rng rng{42};
        for (int i = 0; i < 400; i++) {
            pm::Id id = pm.id_add();
            BMonster m;
            m.pos = {rng.rfr(0, 900), rng.rfr(0, 700)};
            m.vel = pm::norm(pm::Vec2{450, 350} - m.pos) * 60.f;
            m.size = rng.rfr(8, 16);
            m.shoot_timer = rng.rfr(1.5f, 4.f);
            mp->add(id, m);
        }

        auto r1 = bench("monster AI 400 (closest of 4 + steer, seq)", 400, [&]() {
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
        CHECK(r1.ns_per_op < threshold::MONSTER_AI_400_SEQ);

        auto r2 = bench("monster AI 400 (closest of 4 + steer, parallel)", 400, [&]() {
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
        CHECK(r2.ns_per_op < threshold::MONSTER_AI_400_PAR);
    }

    // 2000 stress
    {
        pm::Pm pm;
        auto* mp = pm.pool_get<BMonster>("monsters");
        pm::Rng rng{42};
        for (int i = 0; i < 2000; i++) {
            pm::Id id = pm.id_add();
            BMonster m;
            m.pos = {rng.rfr(0, 900), rng.rfr(0, 700)};
            m.vel = pm::norm(pm::Vec2{450, 350} - m.pos) * 60.f;
            m.size = rng.rfr(8, 16);
            m.shoot_timer = rng.rfr(1.5f, 4.f);
            mp->add(id, m);
        }

        auto r = bench("monster AI 2000 (5x stress, sequential)", 2000, [&]() {
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
        CHECK(r.ns_per_op < threshold::MONSTER_AI_2000);
    }
}

// --- Collision frame ---

TEST_CASE("collision frame") {
    static const pm::Vec2 player_pos[4] = {
        {225, 280}, {675, 280}, {225, 420}, {675, 420}
    };
    static constexpr float PLAYER_R = 32.f;
    static constexpr float QUERY_R  = 20.f;

    auto r1 = bench("collision frame: 400 mon, 300 bul, 4 players", 1300, []() {
        pm::Pm pm;
        auto* mp = pm.pool_get<BMonster>("monsters");
        auto* bp = pm.pool_get<BBullet>("bullets");
        pm::Rng rng{42};
        pm::SpatialGrid grid(900, 700, 64);

        for (int i = 0; i < 400; i++) {
            pm::Id id = pm.id_add();
            BMonster m;
            m.pos = {rng.rfr(0, 900), rng.rfr(0, 700)};
            m.vel = {rng.rfr(-60, 60), rng.rfr(-60, 60)};
            m.size = rng.rfr(8, 16);
            mp->add(id, m);
        }
        for (int i = 0; i < 300; i++) {
            pm::Id id = pm.id_add();
            BBullet b;
            b.pos = {rng.rfr(0, 900), rng.rfr(0, 700)};
            b.vel = {rng.rfr(-750, 750), rng.rfr(-750, 750)};
            b.lifetime = 1.5f;
            b.size = 4;
            b.player_owned = i < 200;
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
    CHECK(r1.ns_per_op < threshold::COLLISION_MID);

    auto r2 = bench("collision frame: 400 mon, 600 bul, 4 players", 1600, []() {
        pm::Pm pm;
        auto* mp = pm.pool_get<BMonster>("monsters");
        auto* bp = pm.pool_get<BBullet>("bullets");
        pm::Rng rng{42};
        pm::SpatialGrid grid(900, 700, 64);

        for (int i = 0; i < 400; i++) {
            pm::Id id = pm.id_add();
            BMonster m;
            m.pos = {rng.rfr(0, 900), rng.rfr(0, 700)};
            m.vel = {rng.rfr(-60, 60), rng.rfr(-60, 60)};
            m.size = rng.rfr(8, 16);
            mp->add(id, m);
        }
        for (int i = 0; i < 600; i++) {
            pm::Id id = pm.id_add();
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
    CHECK(r2.ns_per_op < threshold::COLLISION_PEAK);

    auto r3 = bench("collision brute force: 400x600 (no grid)", 240000, []() {
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
    CHECK(r3.ns_per_op < threshold::COLLISION_BRUTE);
}

// --- Server tick ---

TEST_CASE("server tick") {
    static const pm::Vec2 player_pos[4] = {
        {225, 280}, {675, 280}, {225, 420}, {675, 420}
    };

    auto run_tick = [](int n_monsters, int n_bullets, const char* label, uint64_t ops) {
        return bench(label, ops, [n_monsters, n_bullets]() {
            pm::Pm pm;
            auto* mp = pm.pool_get<BMonster>("monsters");
            auto* bp = pm.pool_get<BBullet>("bullets");
            pm::Rng rng{42};
            pm::SpatialGrid grid(900, 700, 64);

            for (int i = 0; i < n_monsters; i++) {
                pm::Id id = pm.id_add();
                BMonster m;
                m.pos = {rng.rfr(0, 900), rng.rfr(0, 700)};
                m.vel = pm::norm(pm::Vec2{450, 350} - m.pos) * 60.f;
                m.size = rng.rfr(8, 16);
                m.shoot_timer = rng.rfr(1.5f, 4.f);
                mp->add(id, m);
            }
            for (int i = 0; i < n_bullets; i++) {
                pm::Id id = pm.id_add();
                BBullet b;
                b.pos = {rng.rfr(0, 900), rng.rfr(0, 700)};
                b.vel = {rng.rfr(-750, 750), rng.rfr(-750, 750)};
                b.lifetime = rng.rfr(0.5f, 1.5f);
                b.size = 4;
                b.player_owned = (rng.next() % 3) != 0;
                bp->add(id, b);
            }

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

            bp->each_mut([&](pm::Id id, BBullet& b) {
                b.pos.x += b.vel.x * dt;
                b.pos.y += b.vel.y * dt;
                b.lifetime -= dt;
                if (b.lifetime <= 0) pm.id_remove(id);
            }, pm::Parallel::Off);

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
                        pm.id_remove(mid);
                        pm.id_remove(bid);
                        hit = true;
                    }
                });
            }, pm::Parallel::Off);

            mp->each([&](pm::Id id, const BMonster& m) {
                if (m.pos.x < -100 || m.pos.x > 1000 || m.pos.y < -100 || m.pos.y > 800)
                    pm.id_remove(id);
            }, pm::Parallel::Off);
            bp->each([&](pm::Id id, const BBullet& b) {
                if (b.pos.x < -50 || b.pos.x > 950 || b.pos.y < -50 || b.pos.y > 750)
                    pm.id_remove(id);
            }, pm::Parallel::Off);

            pm.id_process_removes();
        });
    };

    auto r1 = run_tick(60, 50, "server tick: level 1 (60 mon, 50 bul)", 200);
    CHECK(r1.ns_per_op < threshold::SERVER_TICK_L1);

    auto r2 = run_tick(400, 600, "server tick: level 5 (400 mon, 600 bul)", 1600);
    CHECK(r2.ns_per_op < threshold::SERVER_TICK_L5);

    auto r3 = bench("30 server ticks: level 5 sustained", 30 * 1600, []() {
        pm::Pm pm;
        auto* mp = pm.pool_get<BMonster>("monsters");
        auto* bp = pm.pool_get<BBullet>("bullets");
        pm::Rng rng{42};
        pm::SpatialGrid grid(900, 700, 64);

        for (int i = 0; i < 400; i++) {
            pm::Id id = pm.id_add();
            BMonster m;
            m.pos = {rng.rfr(0, 900), rng.rfr(0, 700)};
            m.vel = pm::norm(pm::Vec2{450, 350} - m.pos) * 60.f;
            m.size = rng.rfr(8, 16);
            m.shoot_timer = rng.rfr(1.5f, 4.f);
            mp->add(id, m);
        }
        for (int i = 0; i < 600; i++) {
            pm::Id id = pm.id_add();
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

            bp->each_mut([&](pm::Id id, BBullet& b) {
                b.pos.x += b.vel.x * dt;
                b.pos.y += b.vel.y * dt;
                b.lifetime -= dt;
                if (b.lifetime <= 0) pm.id_remove(id);
            }, pm::Parallel::Off);

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
                        pm.id_remove(mid);
                        pm.id_remove(bid);
                        hit = true;
                    }
                });
            }, pm::Parallel::Off);

            mp->each([&](pm::Id id, const BMonster& m) {
                if (m.pos.x < -100 || m.pos.x > 1000 || m.pos.y < -100 || m.pos.y > 800)
                    pm.id_remove(id);
            }, pm::Parallel::Off);
            bp->each([&](pm::Id id, const BBullet& b) {
                if (b.pos.x < -50 || b.pos.x > 950 || b.pos.y < -50 || b.pos.y > 750)
                    pm.id_remove(id);
            }, pm::Parallel::Off);

            pm.id_process_removes();

            while (static_cast<int>(mp->items.size()) < 400) {
                pm::Id id = pm.id_add();
                BMonster m;
                m.pos = {rng.rfr(0, 900), rng.rfr(0, 700)};
                m.vel = pm::norm(pm::Vec2{450, 350} - m.pos) * 60.f;
                m.size = rng.rfr(8, 16);
                m.shoot_timer = rng.rfr(1.5f, 4.f);
                mp->add(id, m);
            }
            while (static_cast<int>(bp->items.size()) < 600) {
                pm::Id id = pm.id_add();
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
    CHECK(r3.ns_per_op < threshold::SERVER_TICK_30);
}

// --- PLC utils ---

TEST_CASE("plc utils") {
    auto r1 = bench("Cooldown::ready 100k", 100000, []() {
        std::vector<pm::Cooldown> cds(100000, pm::Cooldown(0.5f));
        float dt = 0.016f;
        int fires = 0;
        for (auto& cd : cds) {
            if (cd.ready(dt)) fires++;
        }
        do_not_optimize(fires);
    });
    CHECK(r1.ns_per_op < threshold::COOLDOWN);

    auto r2 = bench("Hysteresis update+set 100k", 100000, []() {
        std::vector<pm::Hysteresis<bool>> hs(100000, pm::Hysteresis<bool>(false, 0.1f));
        float dt = 0.016f;
        for (size_t i = 0; i < hs.size(); i++) {
            hs[i].update(dt);
            hs[i].set(i % 3 == 0);
        }
        do_not_optimize(hs[0].get());
    });
    CHECK(r2.ns_per_op < threshold::HYSTERESIS);

    auto r3 = bench("RisingEdge update 100k", 100000, []() {
        std::vector<pm::RisingEdge> edges(100000);
        int fires = 0;
        for (size_t i = 0; i < edges.size(); i++) {
            if (edges[i].update(i % 7 == 0)) fires++;
        }
        do_not_optimize(fires);
    });
    CHECK(r3.ns_per_op < threshold::RISING_EDGE);

    auto r4 = bench("DelayTimer update 100k", 100000, []() {
        std::vector<pm::DelayTimer> timers(100000, pm::DelayTimer(0.5f, 0.2f));
        float dt = 0.016f;
        int active = 0;
        for (size_t i = 0; i < timers.size(); i++) {
            timers[i].update(i % 5 == 0, dt);
            if (timers[i]) active++;
        }
        do_not_optimize(active);
    });
    CHECK(r4.ns_per_op < threshold::DELAY_TIMER);

    auto r5 = bench("Counter increment 100k", 100000, []() {
        std::vector<pm::Counter> ctrs(100000, pm::Counter(10));
        int done = 0;
        for (auto& c : ctrs) {
            c.increment();
            if (c.done) done++;
        }
        do_not_optimize(done);
    });
    CHECK(r5.ns_per_op < threshold::COUNTER);
}

// --- Multi-pool tick ---

TEST_CASE("multi-pool tick") {
    static const pm::Vec2 player_pos[4] = {
        {225, 280}, {675, 280}, {225, 420}, {675, 420}
    };

    auto r1 = bench("multi-pool tick: 4p + 400m + 600b, iterate all", 1004, []() {
        pm::Pm pm;
        auto* pp = pm.pool_get<BPlayer>("players");
        auto* mp = pm.pool_get<BMonster>("monsters");
        auto* bp = pm.pool_get<BBullet>("bullets");
        pm::Rng rng{42};

        for (int i = 0; i < 4; i++) {
            pm::Id id = pm.id_add();
            pp->add(id, BPlayer{player_pos[i], 100, 0, 0, true});
        }
        for (int i = 0; i < 400; i++) {
            pm::Id id = pm.id_add();
            BMonster m;
            m.pos = {rng.rfr(0, 900), rng.rfr(0, 700)};
            m.vel = pm::norm(pm::Vec2{450, 350} - m.pos) * 60.f;
            m.size = rng.rfr(8, 16);
            mp->add(id, m);
        }
        for (int i = 0; i < 600; i++) {
            pm::Id id = pm.id_add();
            BBullet b;
            b.pos = {rng.rfr(0, 900), rng.rfr(0, 700)};
            b.vel = {rng.rfr(-750, 750), rng.rfr(-750, 750)};
            b.lifetime = rng.rfr(0.5f, 1.5f);
            b.size = 4;
            b.player_owned = (rng.next() % 3) != 0;
            bp->add(id, b);
        }

        float dt = 0.016f;

        pp->each_mut([dt](BPlayer& p) {
            if (p.cooldown > 0) p.cooldown -= dt;
            if (p.invuln > 0) p.invuln -= dt;
        }, pm::Parallel::Off);

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

        bp->each_mut([dt](BBullet& b) {
            b.pos.x += b.vel.x * dt;
            b.pos.y += b.vel.y * dt;
            b.lifetime -= dt;
        }, pm::Parallel::Off);
    });
    CHECK(r1.ns_per_op < threshold::MULTI_POOL_TICK);

    auto r2 = bench("multi-pool tick + spatial grid collision", 1004, []() {
        pm::Pm pm;
        auto* pp = pm.pool_get<BPlayer>("players");
        auto* mp = pm.pool_get<BMonster>("monsters");
        auto* bp = pm.pool_get<BBullet>("bullets");
        pm::Rng rng{42};
        pm::SpatialGrid grid(900, 700, 64);

        for (int i = 0; i < 4; i++) {
            pm::Id id = pm.id_add();
            pp->add(id, BPlayer{player_pos[i], 100, 0, 0, true});
        }
        for (int i = 0; i < 400; i++) {
            pm::Id id = pm.id_add();
            BMonster m;
            m.pos = {rng.rfr(0, 900), rng.rfr(0, 700)};
            m.vel = pm::norm(pm::Vec2{450, 350} - m.pos) * 60.f;
            m.size = rng.rfr(8, 16);
            mp->add(id, m);
        }
        for (int i = 0; i < 600; i++) {
            pm::Id id = pm.id_add();
            BBullet b;
            b.pos = {rng.rfr(0, 900), rng.rfr(0, 700)};
            b.vel = {rng.rfr(-750, 750), rng.rfr(-750, 750)};
            b.lifetime = rng.rfr(0.5f, 1.5f);
            b.size = 4;
            b.player_owned = (rng.next() % 3) != 0;
            bp->add(id, b);
        }

        float dt = 0.016f;

        pp->each_mut([dt](BPlayer& p) {
            if (p.cooldown > 0) p.cooldown -= dt;
            if (p.invuln > 0) p.invuln -= dt;
        }, pm::Parallel::Off);

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

        bp->each_mut([dt](BBullet& b) {
            b.pos.x += b.vel.x * dt;
            b.pos.y += b.vel.y * dt;
            b.lifetime -= dt;
        }, pm::Parallel::Off);

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
    CHECK(r2.ns_per_op < threshold::MULTI_POOL_TICK_GRID);
}

} // TEST_SUITE("bench")
