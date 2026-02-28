#include "doctest/doctest.h"
#include "test_types.hpp"
#include "pm_core.hpp"
#include "pm_udp.hpp"
#include "pm_util.hpp"
#include <cstdio>
#include <cstring>

// Test state + init function (replaces old PhysicsSystem)
struct PhysicsState { float gravity = -9.8f; };

void physics_init(pm::Pm& pm)
{
    auto* ps = pm.state_get<PhysicsState>("physics");
    auto* pos_pool = pm.pool_get<Pos>("pos");
    auto* vel_pool = pm.pool_get<Vel>("vel");
    pm.task_add("physics/tick", 30.f, [ps, pos_pool, vel_pool](pm::Pm& pm) {
        (void)ps;
        pos_pool->each_mut([&](pm::Id id, Pos& pos) {
            auto* vel = vel_pool->get(id);
            if (vel)
            {
                pos.x += vel->dx * pm.loop_dt();
                pos.y += vel->dy * pm.loop_dt();
            }
        }, pm::Parallel::Off);
    });
}

// =========================================================================
// CORE ECS TESTS
// =========================================================================

TEST_SUITE("core") {

TEST_CASE("name interning") {
    pm::NameTable names;
    pm::NameId a = names.intern("hello");
    pm::NameId b = names.intern("world");
    pm::NameId c = names.intern("hello");
    CHECK(a == c);
    CHECK(a != b);
    CHECK(strcmp(names.str(a), "hello") == 0);
    CHECK(names.intern(nullptr) == pm::NULL_NAME);
    CHECK(names.intern("") == pm::NULL_NAME);
    CHECK(names.find("hello") == a);
    CHECK(names.find("nonexistent") == pm::NULL_NAME);
    CHECK(names.find(nullptr) == pm::NULL_NAME);
}

TEST_CASE("id add/remove") {
    pm::Pm pm;
    pm::Id a = pm.id_add();
    pm::Id b = pm.id_add();
    pm::Id c = pm.id_add();
    (void)c;
    CHECK(a != pm::NULL_ID);
    CHECK(b != pm::NULL_ID);
    CHECK(a != b);

    auto *pos = pm.pool_get<Pos>("pos");
    pos->add(a, {1, 2});
    pm.id_remove(a);
    pm.id_process_removes();
    CHECK(!pos->has(a));
    CHECK(pm.id_count() == 2);
}

TEST_CASE("deferred removal") {
    pm::Pm pm;
    auto *pos = pm.pool_get<Pos>("pos");
    pm::Id a = pm.id_add();
    pos->add(a, {1, 2});
    CHECK(pos->has(a));
    pm.id_remove(a);
    // Deferred: entity still alive until flush
    CHECK(pos->has(a));
    CHECK(pm.id_count() == 1);
    pm.id_process_removes();
    CHECK(!pos->has(a));
    CHECK(pm.id_count() == 0);
}

TEST_CASE("pool basics") {
    pm::Pm pm;
    auto *pos = pm.pool_get<Pos>("pos");
    auto *vel = pm.pool_get<Vel>("vel");
    REQUIRE(pos != nullptr);
    REQUIRE(vel != nullptr);
    CHECK(pos->pool_id != vel->pool_id);

    pm::Id e = pm.id_add();
    pos->add(e, {10.f, 20.f});
    vel->add(e, {1.f, 2.f});

    auto *p = pos->get(e);
    REQUIRE(p != nullptr);
    CHECK(p->x == 10.f);
    CHECK(p->y == 20.f);
    CHECK(pos->has(e));
    CHECK(vel->has(e));
    CHECK(pos->size() == 1);
}

TEST_CASE("entity->pool remove") {
    pm::Pm pm;
    auto *pos = pm.pool_get<Pos>("pos");
    auto *vel = pm.pool_get<Vel>("vel");
    auto *hp = pm.pool_get<Health>("health");

    pm::Id e = pm.id_add();
    pos->add(e, {1, 2});
    vel->add(e, {3, 4});
    hp->add(e, {50});

    pm.id_remove(e);
    pm.loop_once();

    CHECK(!pos->has(e));
    CHECK(!vel->has(e));
    CHECK(!hp->has(e));
}

TEST_CASE("change hook on add and each_mut") {
    pm::Pm pm;
    auto *pos = pm.pool_get<Pos>("pos");

    int change_count = 0;
    pos->set_change_hook([](void* ctx, pm::Id) {
        auto* count = static_cast<int*>(ctx);
        (*count)++;
    }, &change_count);

    pm::Id e1 = pm.id_add();
    pos->add(e1, {1.f, 0.f});
    CHECK(change_count == 1);

    pm::Id e2 = pm.id_add();
    pos->add(e2, {2.f, 0.f});
    CHECK(change_count == 2);

    pos->each_mut([&](pm::Id, Pos& p) {
        p.x += 10.f;
    }, pm::Parallel::Off);
    CHECK(change_count == 4);
}

TEST_CASE("state + init function") {
    pm::Pm pm;
    physics_init(pm);
    auto *ps = pm.state_get<PhysicsState>("physics");
    CHECK(ps->gravity == -9.8f);
    CHECK(pm.task_get("physics/tick") != nullptr);
}

TEST_CASE("state re-fetch") {
    pm::Pm pm;
    struct Config { int width = 0; int height = 0; };
    auto* cfg = pm.state_get<Config>("config");
    cfg->width = 1920;
    cfg->height = 1080;

    // Re-fetch returns same instance
    auto* cfg2 = pm.state_get<Config>("config");
    CHECK(cfg2 == cfg);
    CHECK(cfg2->width == 1920);
}

TEST_CASE("scheduler") {
    pm::Pm pm;
    int counter = 0;
    pm.task_add("test/count", 50.f, [&counter](pm::Pm &) { counter++; });
    pm.loop_once();
    CHECK(counter == 1);
    pm.loop_once();
    CHECK(counter == 2);
}

TEST_CASE("id_sync") {
    pm::Pm pm;
    pm::Id remote_id = pm::Id::make(42, 7);
    pm.id_sync(remote_id);

    auto *pos = pm.pool_get<Pos>("pos");
    pos->add(remote_id, {1, 2});
    CHECK(pos->has(remote_id));
}

TEST_CASE("id_sync remove+resync cycle") {
    pm::Pm pm;
    auto *pool = pm.pool_get<Pos>("pos");

    for (uint32_t gen = 1; gen <= 100; gen++) {
        pm::Id id = pm::Id::make(5, gen);
        pm.id_sync(id);
        pool->add(id, {(float)gen, 0});
        CHECK(pm.id_count() < 1000000);
        pm.id_remove(id);
        pm.loop_once();
    }
    CHECK(pm.id_count() < 100);
}

TEST_CASE("id_sync rejects stale") {
    pm::Pm pm;
    auto *pool = pm.pool_get<Pos>("pos");

    pm::Id id_v1 = pm::Id::make(5, 1);
    CHECK(pm.id_sync(id_v1) == true);
    pool->add(id_v1, {1, 2});

    pm.id_remove(id_v1);
    pm.loop_once();

    CHECK(pm.id_sync(id_v1) == false);

    pm::Id id_v2 = pm::Id::make(5, 2);
    CHECK(pm.id_sync(id_v2) == true);
    pool->add(id_v2, {3, 4});
    CHECK(pool->has(id_v2));
}

TEST_CASE("pool add updates dense_indices on gen change") {
    pm::Pm pm;
    auto *pool = pm.pool_get<Pos>("pos");

    pm::Id v1 = pm::Id::make(5, 1);
    pm.id_sync(v1);
    pool->add(v1, {10, 20});

    pm::Id v2 = pm::Id::make(5, 2);
    pm.id_sync(v2);
    pool->add(v2, {30, 40});

    CHECK(pool->has(v2));
    CHECK(!pool->has(v1));
    CHECK(pool->get(v2)->x == 30);
}

TEST_CASE("id_count under id_sync churn") {
    pm::Pm pm;
    auto *pool = pm.pool_get<Pos>("pos");

    for (uint32_t i = 0; i < 10; i++) {
        pm::Id id = pm::Id::make(i * 3, 1);
        pm.id_sync(id);
        pool->add(id, {(float)i, 0});
    }
    CHECK(pm.id_count() == 10);

    for (uint32_t i = 0; i < 5; i++)
        pm.id_remove(pm::Id::make(i * 3, 1));
    pm.loop_once();
    CHECK(pm.id_count() == 5);

    for (uint32_t i = 0; i < 5; i++) {
        pm::Id id = pm::Id::make(i * 3, 2);
        pm.id_sync(id);
        pool->add(id, {(float)i + 100, 0});
    }
    CHECK(pm.id_count() == 10);
}

TEST_CASE("id_sync after deferred remove") {
    pm::Pm pm;
    auto *pool = pm.pool_get<Pos>("pos");

    pm::Id v1 = pm::Id::make(5, 1);
    pm.id_sync(v1);
    pool->add(v1, {1, 1});
    pm.id_remove(v1);
    pm.id_process_removes();
    CHECK(!pool->has(v1));

    pm::Id v2 = pm::Id::make(5, 2);
    CHECK(pm.id_sync(v2));
    pool->add(v2, {99, 99});

    CHECK(pool->has(v2));
    CHECK(pool->get(v2)->x == 99);
}

TEST_CASE("deferred remove cleans pool entry") {
    pm::Pm pm;
    auto *pool = pm.pool_get<Pos>("pos");

    pm::Id v1 = pm::Id::make(5, 1);
    pm.id_sync(v1);
    pool->add(v1, {10, 20});
    CHECK(pool->has(v1));

    pm.id_remove(v1);
    CHECK(pool->has(v1)); // still alive until flush
    pm.id_process_removes();
    CHECK(!pool->has(v1));
}

TEST_CASE("id_remove + task_stop + pool reset") {
    pm::Pm pm;
    auto *pos = pm.pool_get<Pos>("pos");
    pm::Id e = pm.id_add();
    pos->add(e, {1, 2});

    pm.id_remove(e);
    pm.task_stop("thing");
    pm.loop_once();
    CHECK(!pos->has(e));

    pm::Id e2 = pm.id_add();
    pos->add(e2, {3, 4});
    pos->reset();
    CHECK(pos->items.size() == 0);

    pm::Id e3 = pm.id_add();
    pos->add(e3, {5, 6});
    CHECK(pos->has(e3));
}

TEST_CASE("add overwrite") {
    pm::Pm pm;
    auto *pos = pm.pool_get<Pos>("pos");

    pm::Id e = pm.id_add();
    pos->add(e, {1, 2});
    pos->add(e, {5, 6});
    CHECK(pos->get(e)->x == 5.f);
    CHECK(pos->size() == 1);
}

TEST_CASE("const char* overloads") {
    pm::Pm pm;

    auto *pos = pm.pool_get<Pos>("mypool");
    REQUIRE(pos != nullptr);

    auto *s = pm.state_get<PhysicsState>("mystate");
    REQUIRE(s != nullptr);

    pm.task_add("mytask", 50.f, [](pm::Pm &) {});
    CHECK(pm.task_get("mytask") != nullptr);
}

TEST_CASE("type safety same-type re-fetch") {
    pm::Pm pm1;
    pm1.pool_get<Pos>("pos");
    auto *pos2 = pm1.pool_get<Pos>("pos");
    REQUIRE(pos2 != nullptr);

    pm::Pm pm2;
    pm2.state_get<PhysicsState>("phys");
    auto *s2 = pm2.state_get<PhysicsState>("phys");
    REQUIRE(s2 != nullptr);
}

TEST_CASE("task stopping") {
    pm::Pm pm;
    int counter_a = 0, counter_b = 0;
    pm.task_add("task_a", 50.f, [&counter_a](pm::Pm &) { counter_a++; });
    pm.task_add("task_b", 60.f, [&counter_b](pm::Pm &) { counter_b++; });

    pm.loop_once();
    CHECK(counter_a == 1);
    CHECK(counter_b == 1);

    pm.task_stop("task_a");
    pm.loop_once();
    CHECK(counter_a == 1);
    CHECK(counter_b == 2);
}

TEST_CASE("task_stop standalone") {
    pm::Pm pm;
    int counter = 0;
    pm.task_add("mysys", 50.f, [&counter](pm::Pm &) { counter++; });

    pm.loop_once();
    CHECK(counter == 1);

    pm.task_stop("mysys");
    pm.loop_once();
    CHECK(counter == 1);
}

TEST_CASE("deferred remove during each") {
    pm::Pm pm;
    auto *pos = pm.pool_get<Pos>("pos");

    pm::Id e1 = pm.id_add();
    pm::Id e2 = pm.id_add();
    pm::Id e3 = pm.id_add();
    pos->add(e1, {1, 0});
    pos->add(e2, {2, 0});
    pos->add(e3, {3, 0});

    int visited = 0;
    pos->each([&](pm::Id id, const Pos& val) {
        visited++;
        if (val.x == 2.f) pm.id_remove(id);
    }, pm::Parallel::Off);
    CHECK(visited == 3);
    // Deferred: entity still in pool until flush
    CHECK(pos->has(e2));
    CHECK(pm.id_pending_removes() == 1);
    pm.id_process_removes();
    CHECK(!pos->has(e2));
    CHECK(pos->has(e1));
    CHECK(pos->has(e3));
}

TEST_CASE("each_mut void(T&) parallel") {
    pm::Pm pm;
    auto *pool = pm.pool_get<Pos>("pos");
    for (int i = 0; i < 2000; i++) {
        pm::Id e = pm.id_add();
        pool->add(e, {(float)i, 0.f});
    }
    pool->each_mut([](Pos& p) { p.y = p.x * 2.f; });
    for (size_t i = 0; i < pool->items.size(); i++)
        CHECK(pool->items[i].y == pool->items[i].x * 2.f);
}

TEST_CASE("each_mut void(Id, T&) parallel") {
    pm::Pm pm;
    auto *pool = pm.pool_get<Pos>("pos");
    for (int i = 0; i < 2000; i++) {
        pm::Id e = pm.id_add();
        pool->add(e, {(float)i, 0.f});
    }
    pool->each_mut([](pm::Id id, Pos& p) {
        p.y = (float)id.index();
    });
    for (size_t i = 0; i < pool->items.size(); i++) {
        uint32_t slot = pool->dense_indices[i];
        CHECK(pool->items[i].y == (float)slot);
    }
}

TEST_CASE("each_mut empty pool") {
    pm::Pm pm;
    auto *pool = pm.pool_get<Pos>("pos");
    pool->each_mut([](Pos& p) { p.x = 999.f; });
    CHECK(pool->size() == 0);
}

TEST_CASE("each_mut Parallel::Off") {
    pm::Pm pm;
    auto *pool = pm.pool_get<Pos>("pos");
    for (int i = 0; i < 10; i++) {
        pm::Id e = pm.id_add();
        pool->add(e, {(float)i, 0.f});
    }
    pool->each_mut([](Pos& p) { p.y = 42.f; }, pm::Parallel::Off);
    for (auto& p : pool->items) CHECK(p.y == 42.f);
}

TEST_CASE("each + deferred remove via tick") {
    pm::Pm pm;
    auto *pool = pm.pool_get<Pos>("pos");

    pm::Id e1 = pm.id_add();
    pm::Id e2 = pm.id_add();
    pm::Id e3 = pm.id_add();
    pool->add(e1, {1, 0});
    pool->add(e2, {2, 0});
    pool->add(e3, {3, 0});

    pm.task_add("test", 1.f, [pool](pm::Pm& pm) {
        pool->each([&](pm::Id id, const Pos& p) {
            if (p.x == 2.f) pm.id_remove(id);
        }, pm::Parallel::Off);
    });
    pm.loop_once();
    CHECK(pool->size() == 2);
    CHECK(pool->has(e1));
    CHECK(!pool->has(e2));
    CHECK(pool->has(e3));
}

TEST_CASE("each_mut auto-fires change hooks") {
    pm::Pm pm;
    auto *pool = pm.pool_get<Pos>("pos");
    int changes = 0;
    pool->set_change_hook([](void* ctx, pm::Id) {
        (*static_cast<int*>(ctx))++;
    }, &changes);
    for (int i = 0; i < 100; i++) {
        pm::Id e = pm.id_add();
        pool->add(e, {(float)i, 0.f});
    }
    int before = changes;
    pool->each_mut([](Pos& p) { p.y = 99.f; }, pm::Parallel::Off);
    CHECK(changes == before + 100);
}

TEST_CASE("generation wrap-around fix") {
    uint32_t max_gen = 0xFFFFFF;
    uint32_t new_gen = (max_gen + 1) & 0xFFFFFF;
    CHECK(new_gen == 0);
    if (new_gen == 0) new_gen = 1;
    CHECK(new_gen == 1);

    uint32_t normal_gen = (5 + 1) & 0xFFFFFF;
    CHECK(normal_gen == 6);
}

TEST_CASE("each does not fire change hooks") {
    pm::Pm pm;
    auto *pool = pm.pool_get<Pos>("pos");
    int changes = 0;
    pool->set_change_hook([](void* ctx, pm::Id) {
        (*static_cast<int*>(ctx))++;
    }, &changes);
    for (int i = 0; i < 50; i++) {
        pm::Id e = pm.id_add();
        pool->add(e, {(float)i, 0.f});
    }
    int before = changes;
    pool->each([](const Pos& p) { (void)p; }, pm::Parallel::Off);
    CHECK(changes == before);
    pool->each([](pm::Id, const Pos& p) { (void)p; }, pm::Parallel::Off);
    CHECK(changes == before);
}

TEST_CASE("each_mut does fire change hooks") {
    pm::Pm pm;
    auto *pool = pm.pool_get<Pos>("pos");
    int changes = 0;
    pool->set_change_hook([](void* ctx, pm::Id) {
        (*static_cast<int*>(ctx))++;
    }, &changes);
    for (int i = 0; i < 50; i++) {
        pm::Id e = pm.id_add();
        pool->add(e, {(float)i, 0.f});
    }
    int before = changes;
    pool->each_mut([](Pos& p) { p.y = 1.f; }, pm::Parallel::Off);
    CHECK(changes == before + 50);
}

TEST_CASE("dense_ids tracks full Ids") {
    pm::Pm pm;
    auto *pool = pm.pool_get<Pos>("pos");

    pm::Id a = pm.id_add();
    pm::Id b = pm.id_add();
    pm::Id c = pm.id_add();
    pool->add(a, {1, 0});
    pool->add(b, {2, 0});
    pool->add(c, {3, 0});

    CHECK(pool->id_at(0) == a);
    CHECK(pool->id_at(1) == b);
    CHECK(pool->id_at(2) == c);

    // Remove first — swap-remove should update dense_ids
    pool->remove(a);
    CHECK(pool->size() == 2);
    CHECK(pool->id_at(0) == c);
    CHECK(pool->id_at(1) == b);
}

TEST_CASE("each_mut parallel with hook falls back to sequential") {
    pm::Pm pm;
    auto *pool = pm.pool_get<Pos>("pos");
    int changes = 0;
    pool->set_change_hook([](void* ctx, pm::Id) {
        (*static_cast<int*>(ctx))++;
    }, &changes);
    for (int i = 0; i < 2000; i++) {
        pm::Id e = pm.id_add();
        pool->add(e, {(float)i, 0.f});
    }
    int before = changes;
    pool->each_mut([](Pos& p) { p.y = 1.f; }, pm::Parallel::On);
    CHECK(changes == before + 2000);
}

TEST_CASE("each passes const reference") {
    pm::Pm pm;
    auto *pool = pm.pool_get<Pos>("pos");
    pm::Id e = pm.id_add();
    pool->add(e, {1.f, 2.f});
    pool->each([](const Pos& p) { CHECK(p.x == 1.f); }, pm::Parallel::Off);
    pool->each([](pm::Id, const Pos& p) { CHECK(p.x == 1.f); }, pm::Parallel::Off);
}

TEST_CASE("fault handling") {
    pm::Pm pm;
    int counter = 0;
    pm.task_add("faulty", 50.f, [&counter](pm::Pm &) {
        counter++;
        if (counter >= 2) throw pm::TaskFault("boom");
    });

    pm.loop_once();
    CHECK(counter == 1);
    pm.loop_once();
    CHECK(counter == 2);
    CHECK(pm.task_faults().size() == 1);
    pm.loop_once();
    CHECK(counter == 2);
}

TEST_CASE("pause/step") {
    pm::Pm pm;
    int always_count = 0, game_count = 0;

    pm.task_add("input", 10.f, [&](pm::Pm&) { always_count++; });
    pm.task_add("physics", 30.f, [&](pm::Pm&) { game_count++; },
                true);

    pm.loop_once();
    CHECK(always_count == 1);
    CHECK(game_count == 1);

    pm.paused = true;
    pm.loop_once();
    CHECK(always_count == 2);
    CHECK(game_count == 1);

    pm.loop_step();
    pm.loop_once();
    CHECK(always_count == 3);
    CHECK(game_count == 2);
    CHECK(pm.paused);

    pm.paused = false;
    pm.loop_once();
    CHECK(always_count == 4);
    CHECK(game_count == 3);
}

} // TEST_SUITE("core")

// =========================================================================
// NETWORKING / SYNC TESTS (via pm_udp)
// =========================================================================

TEST_SUITE("net") {

TEST_CASE("peer range iteration") {
    pm::Pm pm;
    auto* net = pm.state_get<pm::NetSys>("net");

    uint8_t p1 = net->connect();
    uint8_t p2 = net->connect();
    uint8_t p3 = net->connect();
    CHECK(p1 == 1);
    CHECK(p2 == 2);
    CHECK(p3 == 3);

    std::vector<uint8_t> all;
    for (uint8_t p : net->peers()) all.push_back(p);
    CHECK(all.size() == 4);

    std::vector<uint8_t> remotes;
    for (uint8_t p : net->remote_peers()) remotes.push_back(p);
    CHECK(remotes.size() == 3);

    CHECK(net->peer_count() == 4);
    CHECK(net->peers().has(0));
    CHECK(!net->peers().has(55));

    net->disconnect(p2);
    CHECK(net->peer_count() == 3);
    CHECK(!net->peers().has(2));

    uint8_t p4 = net->connect();
    CHECK(p4 == 2);
}

TEST_CASE("peer metadata") {
    pm::Pm pm;
    auto* net = pm.state_get<pm::NetSys>("net");

    CHECK(net->peer(0).id == 0);
    CHECK(net->peer(0).connected == true);

    uint8_t p1 = net->connect();
    CHECK(net->peer(p1).connected == true);

    int my_data = 42;
    net->peer(p1).user_data = &my_data;
    CHECK(*(int*)net->peer(p1).user_data == 42);

    net->disconnect(p1);
    CHECK(net->peer(p1).connected == false);
    CHECK(net->peer(p1).user_data == nullptr);
}

TEST_CASE("peer lifecycle hooks") {
    pm::Pm pm;
    auto* net = pm.state_get<pm::NetSys>("net");
    std::vector<std::pair<char, uint8_t>> events;

    net->on_connect([&](pm::NetSys&, uint8_t id) { events.push_back({'C', id}); });
    net->on_disconnect([&](pm::NetSys&, uint8_t id) { events.push_back({'D', id}); });

    uint8_t p1 = net->connect();
    uint8_t p2 = net->connect();
    (void)p2;
    CHECK(events.size() == 2);

    net->disconnect(p1);
    CHECK(events.size() == 3);
    CHECK(events[2] == std::make_pair('D', p1));

    net->disconnect(p1);
    CHECK(events.size() == 3);
}

TEST_CASE("set_peer_id") {
    pm::Pm pm;
    auto* net = pm.state_get<pm::NetSys>("net");

    CHECK((net->peer_slots() & 1));

    net->set_peer_id(255);
    CHECK(net->peer_slots() == 0);

    net->set_peer_id(3);
    CHECK(net->peer_slots() == (1ULL << 3));
    CHECK(net->peer(3).connected == true);

    uint8_t s = net->connect();
    CHECK(net->remote_peers().count() == 1);
    bool found_self = false;
    for (uint8_t p : net->remote_peers())
        if (p == 3) found_self = true;
    CHECK(!found_self);
    (void)s;
}

TEST_CASE("pool sync state basics") {
    pm::Pm pm;
    auto* pool = pm.pool_get<int>("items");

    pm::PoolSyncState ss;
    ss.pool_id = pool->pool_id;

    pool->set_change_hook([](void* ctx, pm::Id id) {
        static_cast<pm::PoolSyncState*>(ctx)->mark_changed(id);
    }, &ss);

    pm::Id a = pm.id_add(); pool->add(a, 10);
    pm::Id b = pm.id_add(); pool->add(b, 20);
    pm::Id c = pm.id_add(); pool->add(c, 30);

    CHECK(!ss.is_synced_to(1, a));
    CHECK(!ss.is_synced_to(1, b));

    ss.mark_synced(1, a);
    ss.mark_synced(1, b);
    CHECK(ss.is_synced_to(1, a));
    CHECK(ss.is_synced_to(1, b));
    CHECK(!ss.is_synced_to(1, c));
    CHECK(!ss.is_synced_to(2, a));

    ss.mark_unsynced_for(1, a);
    CHECK(!ss.is_synced_to(1, a));
    CHECK(ss.is_synced_to(1, b));
}

TEST_CASE("pool sync state each_unsynced") {
    pm::Pm pm;
    auto* net = pm.state_get<pm::NetSys>("net");
    auto* pool = pm.pool_get<Pos>("pos");

    pm::PoolSyncState ss;
    pool->set_change_hook([](void* ctx, pm::Id id) {
        static_cast<pm::PoolSyncState*>(ctx)->mark_changed(id);
    }, &ss);

    uint8_t peer = net->connect();

    for (int i = 0; i < 10; i++) {
        pm::Id e = pm.id_add();
        pool->add(e, {(float)i, 0});
    }

    int count = 0;
    uint64_t remote_mask = net->remote_peers().bits;
    ss.each_unsynced(pool, peer, remote_mask, [&](pm::Id id, Pos&, size_t) {
        ss.mark_synced(peer, id);
        count++;
    });
    CHECK(count == 10);

    count = 0;
    ss.each_unsynced(pool, peer, remote_mask, [&](pm::Id, Pos&, size_t) { count++; });
    CHECK(count == 0);

    CHECK(ss.pending_count() == 0);
}

TEST_CASE("pending list compaction") {
    pm::Pm pm;
    auto* net = pm.state_get<pm::NetSys>("net");
    auto* pool = pm.pool_get<Pos>("pos");

    pm::PoolSyncState ss;
    pool->set_change_hook([](void* ctx, pm::Id id) {
        static_cast<pm::PoolSyncState*>(ctx)->mark_changed(id);
    }, &ss);

    pm::Id ids[100];
    for (int i = 0; i < 100; i++) {
        ids[i] = pm.id_add();
        pool->add(ids[i], {(float)i, 0});
    }
    CHECK(ss.pending_count() == 100);

    uint8_t peer = net->connect();
    uint64_t remote_mask = net->remote_peers().bits;

    ss.each_unsynced(pool, peer, remote_mask, [&](pm::Id id, Pos&, size_t) { ss.mark_synced(peer, id); });
    ss.each_unsynced(pool, peer, remote_mask, [](pm::Id, Pos&, size_t) {});
    CHECK(ss.pending_count() == 0);

    ss.mark_changed(ids[10]);
    ss.mark_changed(ids[50]);
    ss.mark_changed(ids[90]);
    CHECK(ss.pending_count() == 3);

    int count = 0;
    ss.each_unsynced(pool, peer, remote_mask, [&](pm::Id, Pos&, size_t) { count++; });
    CHECK(count == 3);

    pool->remove(ids[50]);
    count = 0;
    ss.each_unsynced(pool, peer, remote_mask, [&](pm::Id, Pos&, size_t) { count++; });
    CHECK(count == 2);
    CHECK(ss.pending_count() == 2);
}

TEST_CASE("interest unsync_for + re-sync") {
    pm::Pm pm;
    auto* net = pm.state_get<pm::NetSys>("net");
    auto* pool = pm.pool_get<float>("positions");

    pm::PoolSyncState ss;
    pool->set_change_hook([](void* ctx, pm::Id id) {
        static_cast<pm::PoolSyncState*>(ctx)->mark_changed(id);
    }, &ss);

    uint8_t p1 = net->connect();
    uint64_t remote_mask = net->remote_peers().bits;

    pm::Id near1 = pm.id_add(); pool->add(near1, 50.f);
    pm::Id near2 = pm.id_add(); pool->add(near2, 100.f);
    pm::Id far1  = pm.id_add(); pool->add(far1, 900.f);

    float interest_radius = 200.f;
    float peer_pos = 80.f;

    std::vector<pm::Id> synced_ids;
    ss.each_unsynced(pool, p1, remote_mask, [&](pm::Id id, float& x, size_t) {
        if (std::abs(x - peer_pos) <= interest_radius) {
            synced_ids.push_back(id);
            ss.mark_synced(p1, id);
        }
    });
    CHECK(synced_ids.size() == 2);

    CHECK(ss.is_synced_to(p1, near2));
    ss.mark_unsynced_for(p1, near2);
    CHECK(!ss.is_synced_to(p1, near2));

    synced_ids.clear();
    ss.each_unsynced(pool, p1, remote_mask, [&](pm::Id id, float&, size_t) {
        synced_ids.push_back(id);
    });
    CHECK(std::find(synced_ids.begin(), synced_ids.end(), near2) != synced_ids.end());
    CHECK(std::find(synced_ids.begin(), synced_ids.end(), far1) != synced_ids.end());
    CHECK(std::find(synced_ids.begin(), synced_ids.end(), near1) == synced_ids.end());
}

TEST_CASE("sync tracking immune to swap-remove") {
    pm::Pm pm;
    auto* pool = pm.pool_get<Pos>("pos");

    pm::PoolSyncState ss;
    pool->set_change_hook([](void* ctx, pm::Id id) {
        static_cast<pm::PoolSyncState*>(ctx)->mark_changed(id);
    }, &ss);

    pm::Id a = pm.id_add();
    pm::Id b = pm.id_add();
    pm::Id c = pm.id_add();
    pool->add(a, {1, 0});
    pool->add(b, {2, 0});
    pool->add(c, {3, 0});

    ss.mark_synced(1, a);
    CHECK(ss.is_synced_to(1, a));
    CHECK(!ss.is_synced_to(1, c));

    pool->remove(a);
    CHECK(pool->items.size() == 2);

    CHECK(!ss.is_synced_to(1, c));
    CHECK(!ss.is_synced_to(1, b));
}

TEST_CASE("repend_all on connect") {
    pm::Pm pm;
    auto* net = pm.state_get<pm::NetSys>("net");
    auto* pool = pm.pool_get<Pos>("pos");

    pm::PoolSyncState ss;
    pool->set_change_hook([](void* ctx, pm::Id id) {
        static_cast<pm::PoolSyncState*>(ctx)->mark_changed(id);
    }, &ss);

    pm::Id a = pm.id_add(); pool->add(a, {1, 2});
    pm::Id b = pm.id_add(); pool->add(b, {3, 4});

    uint8_t p1 = net->connect();
    uint64_t remote_mask = net->remote_peers().bits;
    ss.each_unsynced(pool, p1, remote_mask, [&](pm::Id id, Pos&, size_t) { ss.mark_synced(p1, id); });
    ss.each_unsynced(pool, p1, remote_mask, [](pm::Id, Pos&, size_t) {});
    CHECK(ss.pending_count() == 0);

    ss.repend_all(pool);
    uint8_t p2 = net->connect();
    remote_mask = net->remote_peers().bits;

    int count = 0;
    ss.each_unsynced(pool, p2, remote_mask, [&](pm::Id, Pos&, size_t) { count++; });
    CHECK(count == 2);
}

TEST_CASE("net_init registers tasks") {
    pm::Pm pm;
    auto* net = pm.state_get<pm::NetSys>("net");
    net_init(pm, net, 15.f, 95.f);

    CHECK(pm.task_get("net/recv") != nullptr);
    CHECK(pm.task_get("net/tick") != nullptr);
    CHECK(pm.task_get("net/flush") != nullptr);
}

TEST_CASE("pool swap hook") {
    pm::Pm pm;
    auto* pool = pm.pool_get<Pos>("pos");

    // Track swaps
    struct SwapLog { uint32_t removed, last; };
    static std::vector<SwapLog> swaps;
    swaps.clear();
    pool->set_swap_hook([](void*, uint32_t removed_di, uint32_t last_di) {
        swaps.push_back({removed_di, last_di});
    }, nullptr);

    pm::Id a = pm.id_add(); pool->add(a, {1, 0});  // dense 0
    pm::Id b = pm.id_add(); pool->add(b, {2, 0});  // dense 1
    pm::Id c = pm.id_add(); pool->add(c, {3, 0});  // dense 2

    pool->remove(a);  // swap: dense[0] gets dense[2], pop
    CHECK(swaps.size() == 1);
    CHECK(swaps[0].removed == 0);
    CHECK(swaps[0].last == 2);
    CHECK(pool->items.size() == 2);

    pool->remove(c);  // c was at dense[0] after swap, last=dense[1]
    CHECK(swaps.size() == 2);
    CHECK(swaps[1].removed == 0);
    CHECK(swaps[1].last == 1);

    pool->remove(b);  // only element, removed == last
    CHECK(swaps.size() == 3);
    CHECK(swaps[2].removed == 0);
    CHECK(swaps[2].last == 0);
    CHECK(pool->items.empty());
}

TEST_CASE("pool sync state change-tracked basics") {
    pm::Pm pm;
    auto* net = pm.state_get<pm::NetSys>("net");
    auto* pool = pm.pool_get<Pos>("pos");

    pm::PoolSyncState ss;
    pool->set_change_hook([](void* ctx, pm::Id id) {
        static_cast<pm::PoolSyncState*>(ctx)->mark_changed(id);
    }, &ss);

    uint8_t peer = net->connect();
    uint64_t remote_mask = net->remote_peers().bits;

    pm::Id a = pm.id_add(); pool->add(a, {1, 0});
    pm::Id b = pm.id_add(); pool->add(b, {2, 0});
    pm::Id c = pm.id_add(); pool->add(c, {3, 0});

    // All 3 should appear in pending (add calls notify_change)
    int count = 0;
    ss.each_unsynced(pool, peer, remote_mask, [&](pm::Id, Pos&, size_t) {
        ss.mark_synced(peer, a); // dummy; real code marks each id
        count++;
    });
    CHECK(count == 3);

    // Re-mark all synced properly then confirm nothing pending
    ss.mark_synced(peer, a);
    ss.mark_synced(peer, b);
    ss.mark_synced(peer, c);
    ss.clear_pending();
    count = 0;
    ss.each_unsynced(pool, peer, remote_mask, [&](pm::Id, Pos&, size_t) { count++; });
    CHECK(count == 0);
}

TEST_CASE("change-tracked remove leaves no ghost entries") {
    pm::Pm pm;
    auto* net = pm.state_get<pm::NetSys>("net");
    auto* pool = pm.pool_get<Pos>("pos");

    pm::PoolSyncState ss;
    pool->set_change_hook([](void* ctx, pm::Id id) {
        static_cast<pm::PoolSyncState*>(ctx)->mark_changed(id);
    }, &ss);

    uint8_t peer = net->connect();
    uint64_t remote_mask = net->remote_peers().bits;

    pm::Id a = pm.id_add(); pool->add(a, {10, 0});
    pm::Id b = pm.id_add(); pool->add(b, {20, 0});
    pm::Id c = pm.id_add(); pool->add(c, {30, 0});

    // Sync all
    ss.each_unsynced(pool, peer, remote_mask, [&](pm::Id id, Pos&, size_t) {
        ss.mark_synced(peer, id);
    });
    ss.clear_pending();

    // Remove a — pool swap-removes, pending stays clean
    pool->remove(a);
    // each_unsynced skips ids not in pool
    int count = 0;
    ss.each_unsynced(pool, peer, remote_mask, [&](pm::Id, Pos&, size_t) { count++; });
    CHECK(count == 0);
}

TEST_CASE("change-tracked new entries start unsynced") {
    pm::Pm pm;
    auto* net = pm.state_get<pm::NetSys>("net");
    auto* pool = pm.pool_get<int>("items");

    pm::PoolSyncState ss;
    pool->set_change_hook([](void* ctx, pm::Id id) {
        static_cast<pm::PoolSyncState*>(ctx)->mark_changed(id);
    }, &ss);

    uint8_t peer = net->connect();
    uint64_t remote_mask = net->remote_peers().bits;

    pm::Id a = pm.id_add(); pool->add(a, 10);

    // Sync a
    ss.each_unsynced(pool, peer, remote_mask, [&](pm::Id id, int&, size_t) {
        ss.mark_synced(peer, id);
    });
    ss.clear_pending();

    // Add a new entry — triggers change hook, goes into pending
    pm::Id b = pm.id_add(); pool->add(b, 20);

    int count = 0;
    pm::Id found = pm::NULL_ID;
    ss.each_unsynced(pool, peer, remote_mask, [&](pm::Id id, int&, size_t) {
        count++;
        found = id;
    });
    CHECK(count == 1);
    CHECK(found == b);
}

TEST_CASE("change-tracked multi-peer independence") {
    pm::Pm pm;
    auto* net = pm.state_get<pm::NetSys>("net");
    auto* pool = pm.pool_get<int>("vals");

    pm::PoolSyncState ss;
    pool->set_change_hook([](void* ctx, pm::Id id) {
        static_cast<pm::PoolSyncState*>(ctx)->mark_changed(id);
    }, &ss);

    uint8_t p1 = net->connect();
    uint8_t p2 = net->connect();
    uint64_t remote_mask = net->remote_peers().bits;

    pm::Id a = pm.id_add(); pool->add(a, 1);
    pm::Id b = pm.id_add(); pool->add(b, 2);

    // Sync only to p1
    ss.each_unsynced(pool, p1, remote_mask, [&](pm::Id id, int&, size_t) {
        ss.mark_synced(p1, id);
    });
    // Don't clear_pending: p2 still hasn't synced

    // p2 should still see both as unsynced
    int p2_count = 0;
    ss.each_unsynced(pool, p2, remote_mask, [&](pm::Id id, int&, size_t) {
        ss.mark_synced(p2, id);
        p2_count++;
    });
    CHECK(p2_count == 2);

    // Now both peers synced — pending cleared
    ss.clear_pending();
    int p1_count = 0;
    ss.each_unsynced(pool, p1, remote_mask, [&](pm::Id, int&, size_t) { p1_count++; });
    CHECK(p1_count == 0);
}

TEST_CASE("ordered custom packet sequencing") {
    pm::Pm pm;
    auto* net = pm.state_get<pm::NetSys>("net");
    uint8_t p = net->connect();

    // next_send_seq increments
    CHECK(net->next_send_seq(p) == 1);
    CHECK(net->next_send_seq(p) == 2);
    CHECK(net->next_send_seq(p) == 3);

    // accept_seq accepts newer, rejects older/equal
    CHECK(net->accept_seq(p, 1) == true);
    CHECK(net->accept_seq(p, 1) == false);  // same
    CHECK(net->accept_seq(p, 0) == false);   // older (wrapping-aware: 0 < 1)
    CHECK(net->accept_seq(p, 5) == true);
    CHECK(net->accept_seq(p, 3) == false);   // older than 5
}

TEST_CASE("reliable message dedup ring") {
    pm::Pm pm;
    auto* net = pm.state_get<pm::NetSys>("net");

    auto& pn = net->peer_net[0];

    // Fresh — nothing seen
    CHECK(!pn.has_seen_reliable(1));
    CHECK(!pn.has_seen_reliable(42));

    // Mark and check
    pn.mark_seen_reliable(1);
    CHECK(pn.has_seen_reliable(1));
    CHECK(!pn.has_seen_reliable(2));

    pn.mark_seen_reliable(2);
    CHECK(pn.has_seen_reliable(1));
    CHECK(pn.has_seen_reliable(2));

    // Fill dedup ring past capacity — oldest should be evicted
    for (uint16_t i = 3; i < 3 + pm::NetSys::PeerNet::RELIABLE_DEDUP_SIZE; i++)
        pn.mark_seen_reliable(i);

    // msg_id 1 should be evicted (ring wrapped)
    CHECK(!pn.has_seen_reliable(1));
    // Recent ones should still be there
    CHECK(pn.has_seen_reliable(3 + pm::NetSys::PeerNet::RELIABLE_DEDUP_SIZE - 1));
}

TEST_CASE("reliable message outbox + ack") {
    pm::Pm pm;
    auto* net = pm.state_get<pm::NetSys>("net");
    uint8_t p = net->connect();

    // Send a reliable message
    uint8_t payload[] = {10, 20, 30};
    net->send_reliable(p, 0x42, payload, 3);
    CHECK(net->peer_net[p].reliable_outbox.size() == 1);

    auto& entry = net->peer_net[p].reliable_outbox[0];
    CHECK(entry.msg_id == 1);
    CHECK(entry.sends_remaining == pm::NetSys::PeerNet::RELIABLE_SEND_COUNT);
    CHECK(entry.envelope_size == sizeof(pm::PktReliable) + 3);
    // Verify envelope contains correct header
    pm::PktReliable hdr;
    memcpy(&hdr, entry.envelope, sizeof(hdr));
    CHECK(hdr.type == pm::PKT_RELIABLE);
    CHECK(hdr.msg_id == 1);
    CHECK(hdr.inner_type == 0x42);

    // Send another
    net->send_reliable(p, 0x43, nullptr, 0);
    CHECK(net->peer_net[p].reliable_outbox.size() == 2);

    // Ack first message — should be removed
    net->ack_reliable(p, 1);
    CHECK(net->peer_net[p].reliable_outbox.size() == 1);
    CHECK(net->peer_net[p].reliable_outbox[0].msg_id == 2);
}

TEST_CASE("send_reliable_all broadcasts") {
    pm::Pm pm;
    auto* net = pm.state_get<pm::NetSys>("net");
    uint8_t p1 = net->connect();
    uint8_t p2 = net->connect();

    uint8_t data = 0xFF;
    net->send_reliable_all(0x50, &data, 1);
    CHECK(net->peer_net[p1].reliable_outbox.size() == 1);
    CHECK(net->peer_net[p2].reliable_outbox.size() == 1);
    // Different msg_ids per peer
    CHECK(net->peer_net[p1].reliable_outbox[0].msg_id == 1);
    CHECK(net->peer_net[p2].reliable_outbox[0].msg_id == 1);
}

TEST_CASE("find_peer_by_addr") {
    pm::Pm pm;
    auto* net = pm.state_get<pm::NetSys>("net");
    uint8_t p = net->connect();

    struct sockaddr_in addr{};
    addr.sin_family = AF_INET;
    addr.sin_port = htons(9999);
    addr.sin_addr.s_addr = htonl(0x7F000001); // 127.0.0.1
    net->peer_addrs[p] = addr;
    net->has_addr[p] = true;

    CHECK(net->find_peer_by_addr(addr) == p);

    // Different port — should not match
    struct sockaddr_in addr2 = addr;
    addr2.sin_port = htons(8888);
    CHECK(net->find_peer_by_addr(addr2) == 255);
}

TEST_CASE("clock sync fields") {
    pm::Pm pm;
    auto* net = pm.state_get<pm::NetSys>("net");

    net->local_time = 10.0f;
    net->clock_offset = 5.0f;  // server is 5s ahead
    float est = net->server_time_estimate();
    CHECK(est > 14.99f);
    CHECK(est < 15.01f);
}

TEST_CASE("PktSync wire format") {
    pm::PktSync hdr{};
    hdr.seq = 42;
    hdr.frame = 100;
    hdr.server_time = 3.14f;
    hdr.section_count = 2;

    uint8_t buf[sizeof(pm::PktSync)];
    memcpy(buf, &hdr, sizeof(hdr));

    pm::PktSync decoded;
    memcpy(&decoded, buf, sizeof(decoded));
    CHECK(decoded.type == pm::PKT_SYNC_TYPE);
    CHECK(decoded.seq == 42);
    CHECK(decoded.frame == 100);
    CHECK(decoded.server_time > 3.13f);
    CHECK(decoded.server_time < 3.15f);
    CHECK(decoded.section_count == 2);
}

TEST_CASE("heartbeat packet roundtrip") {
    pm::PktHeartbeat hb{};
    CHECK(hb.type == pm::PKT_HEARTBEAT);
    CHECK(sizeof(hb) == 1);
}

TEST_CASE("SectionHeader no rm_count") {
    pm::SectionHeader sh{};
    sh.pool_id = 42;
    sh.sync_count = 10;
    sh.entry_size = 16;

    uint8_t buf[sizeof(pm::SectionHeader)];
    memcpy(buf, &sh, sizeof(sh));

    pm::SectionHeader decoded;
    memcpy(&decoded, buf, sizeof(decoded));
    CHECK(decoded.pool_id == 42);
    CHECK(decoded.sync_count == 10);
    CHECK(decoded.entry_size == 16);
    // No rm_count field — struct is smaller
    CHECK(sizeof(pm::SectionHeader) == 4 + 2 + 2);
}

TEST_CASE("reliable removal batching") {
    pm::Pm pm;
    auto* net = pm.state_get<pm::NetSys>("net");
    auto* pool = pm.pool_get<int>("items");

    uint8_t p = net->connect();

    // Track some removals
    pm::Id a = pm.id_add(); pool->add(a, 1);
    pm::Id b = pm.id_add(); pool->add(b, 2);
    pm::Id c = pm.id_add(); pool->add(c, 3);

    net->track_removal(pool->pool_id, a);
    net->track_removal(pool->pool_id, b);
    net->track_removal(pool->pool_id, c);

    // Removals are buffered in pending_removals
    CHECK(net->peer_net[p].pending_removals.size() == 3);

    // flush_removals converts them to reliable messages
    net->flush_removals(p);
    CHECK(net->peer_net[p].pending_removals.empty());
    // Should have created 1 reliable message (3 ids fit in one batch)
    CHECK(net->peer_net[p].reliable_outbox.size() == 1);

    // Verify the reliable entry contains correct inner type
    pm::PktReliable hdr;
    memcpy(&hdr, net->peer_net[p].reliable_outbox[0].envelope, sizeof(hdr));
    CHECK(hdr.inner_type == pm::RELIABLE_INNER_REMOVAL);

    // Verify payload: pool_id(4) + count(2) + 3 * Id(8)
    const uint8_t* payload = net->peer_net[p].reliable_outbox[0].envelope + sizeof(pm::PktReliable);
    uint32_t pid; memcpy(&pid, payload, 4);
    uint16_t count; memcpy(&count, payload + 4, 2);
    CHECK(pid == pool->pool_id);
    CHECK(count == 3);
}

TEST_CASE("tracked_remove + clear_pool use reliable removals") {
    pm::Pm pm;
    auto* net = pm.state_get<pm::NetSys>("net");
    auto* pool = pm.pool_get<int>("vals");

    uint8_t p1 = net->connect();
    uint8_t p2 = net->connect();

    pm::Id a = pm.id_add(); pool->add(a, 10);
    pm::Id b = pm.id_add(); pool->add(b, 20);

    // tracked_remove buffers for all remote peers
    net->track_removal(pool->pool_id, a);
    CHECK(net->peer_net[p1].pending_removals.size() == 1);
    CHECK(net->peer_net[p2].pending_removals.size() == 1);
}

TEST_CASE("state sync push + PktStateSync wire format") {
    pm::Pm pm;
    auto* net = pm.state_get<pm::NetSys>("net");
    uint8_t p = net->connect();

    uint8_t data[] = {1, 2, 3, 4, 5};
    net->push_state(p, 42, data, 5);
    CHECK(net->peer_net[p].state_outbox.size() == 1);
    CHECK(net->peer_net[p].state_outbox[0].state_id == 42);
    CHECK(net->peer_net[p].state_outbox[0].size == 5);
    CHECK(memcmp(net->peer_net[p].state_outbox[0].data, data, 5) == 0);

    // PktStateSync wire format
    pm::PktStateSync hdr{};
    hdr.state_id = 99;
    hdr.size = 10;
    CHECK(hdr.type == pm::PKT_STATE_SYNC);

    uint8_t buf[sizeof(pm::PktStateSync)];
    memcpy(buf, &hdr, sizeof(hdr));
    pm::PktStateSync decoded;
    memcpy(&decoded, buf, sizeof(decoded));
    CHECK(decoded.state_id == 99);
    CHECK(decoded.size == 10);
}

TEST_CASE("push_state_all broadcasts") {
    pm::Pm pm;
    auto* net = pm.state_get<pm::NetSys>("net");
    uint8_t p1 = net->connect();
    uint8_t p2 = net->connect();

    uint8_t data = 0xAB;
    net->push_state_all(7, &data, 1);
    CHECK(net->peer_net[p1].state_outbox.size() == 1);
    CHECK(net->peer_net[p2].state_outbox.size() == 1);
    CHECK(net->peer_net[p1].state_outbox[0].state_id == 7);
    CHECK(net->peer_net[p2].state_outbox[0].state_id == 7);
}

TEST_CASE("on_state_recv registration") {
    pm::Pm pm;
    auto* net = pm.state_get<pm::NetSys>("net");

    bool called = false;
    net->on_state_recv(42, [&](pm::Pm&, const uint8_t*, uint16_t) {
        called = true;
    });

    CHECK(net->state_recv_handlers.count(42) == 1);
}

TEST_CASE("clear_frame clears state_outbox") {
    pm::Pm pm;
    auto* net = pm.state_get<pm::NetSys>("net");
    uint8_t p = net->connect();

    uint8_t data = 1;
    net->push_state(p, 1, &data, 1);
    CHECK(net->peer_net[p].state_outbox.size() == 1);
    net->peer_net[p].clear_frame();
    CHECK(net->peer_net[p].state_outbox.empty());
}

TEST_CASE("alloc_peer_slot") {
    pm::Pm pm;
    auto* net = pm.state_get<pm::NetSys>("net");

    // Self is 0, so first free is 1
    CHECK(net->alloc_peer_slot() == 1);

    // Allocate 1
    net->activate_peer(1);
    CHECK(net->alloc_peer_slot() == 2);

    // Allocate 2
    net->activate_peer(2);
    CHECK(net->alloc_peer_slot() == 3);

    // Disconnect 1, it becomes free again
    net->disconnect(1);
    CHECK(net->alloc_peer_slot() == 1);
}

TEST_CASE("activate_peer fires callbacks") {
    pm::Pm pm;
    auto* net = pm.state_get<pm::NetSys>("net");

    int cb_count = 0;
    uint8_t cb_peer = 255;
    net->on_connect([&](pm::NetSys&, uint8_t p) { cb_count++; cb_peer = p; });

    net->activate_peer(1);
    CHECK(cb_count == 1);
    CHECK(cb_peer == 1);
    CHECK(net->peers_arr[1].connected);
}

TEST_CASE("ConnectResult factory methods") {
    auto accept = pm::NetSys::ConnectResult::accept();
    CHECK(accept.accepted);
    CHECK(accept.response_size == 0);

    uint8_t data[] = {10, 20, 30};
    auto accept_with = pm::NetSys::ConnectResult::accept(data, 3);
    CHECK(accept_with.accepted);
    CHECK(accept_with.response_size == 3);
    CHECK(accept_with.response[0] == 10);

    auto deny = pm::NetSys::ConnectResult::deny(pm::DENY_SERVER_FULL);
    CHECK(!deny.accepted);
    CHECK(deny.deny_reason == pm::DENY_SERVER_FULL);
}

TEST_CASE("request_connect sets client state") {
    pm::Pm pm;
    auto* net = pm.state_get<pm::NetSys>("net");
    // Don't actually send (no socket) — just test state setup
    // Manually set up state without calling request_connect (which needs socket)
    net->set_client();
    net->conn_state = pm::NetSys::ConnState::CONNECTING;
    net->connect_payload_size = 5;
    uint8_t payload[] = {1, 2, 3, 4, 5};
    memcpy(net->connect_payload_buf, payload, 5);
    net->connect_timer = 0.f;
    net->connect_elapsed = 0.f;

    CHECK(net->conn_state == pm::NetSys::ConnState::CONNECTING);
    CHECK(!net->host_mode);
    CHECK(net->self_id == 255);
    CHECK(net->connect_payload_size == 5);
}

TEST_CASE("PktConnectReq wire format") {
    pm::PktConnectReq req{};
    req.version = 42;
    CHECK(req.type == pm::PKT_CONNECT_REQ);

    uint8_t buf[sizeof(pm::PktConnectReq)];
    memcpy(buf, &req, sizeof(req));
    pm::PktConnectReq decoded;
    memcpy(&decoded, buf, sizeof(decoded));
    CHECK(decoded.type == pm::PKT_CONNECT_REQ);
    CHECK(decoded.version == 42);
}

TEST_CASE("PktConnectAck wire format") {
    pm::PktConnectAck ack{};
    ack.peer_id = 7;
    CHECK(ack.type == pm::PKT_CONNECT_ACK);

    uint8_t buf[sizeof(pm::PktConnectAck)];
    memcpy(buf, &ack, sizeof(ack));
    pm::PktConnectAck decoded;
    memcpy(&decoded, buf, sizeof(decoded));
    CHECK(decoded.peer_id == 7);
}

TEST_CASE("PktConnectDeny wire format") {
    pm::PktConnectDeny deny{};
    deny.reason = pm::DENY_VERSION_MISMATCH;
    CHECK(deny.type == pm::PKT_CONNECT_DENY);
    CHECK(sizeof(deny) == 2);
}

TEST_CASE("cached ACK stored in Peer on activation") {
    pm::Pm pm;
    auto* net = pm.state_get<pm::NetSys>("net");

    // Fresh peer has no cached ACK
    CHECK(net->peers_arr[1].ack_size == 0);

    // After activate + cache, ack_buf is populated
    net->activate_peer(1);
    // Simulate caching like the server handshake does
    pm::PktConnectAck ack{}; ack.peer_id = 1;
    uint8_t response[] = {0xAA, 0xBB};
    uint8_t ack_buf[sizeof(pm::PktConnectAck) + 2];
    memcpy(ack_buf, &ack, sizeof(ack));
    memcpy(ack_buf + sizeof(ack), response, 2);
    memcpy(net->peers_arr[1].ack_buf, ack_buf, sizeof(ack) + 2);
    net->peers_arr[1].ack_size = sizeof(ack) + 2;

    CHECK(net->peers_arr[1].ack_size == sizeof(pm::PktConnectAck) + 2);

    // Reset clears it
    net->peers_arr[1].reset();
    CHECK(net->peers_arr[1].ack_size == 0);
}

TEST_CASE("ConnState transitions") {
    pm::Pm pm;
    auto* net = pm.state_get<pm::NetSys>("net");

    // Default is DISCONNECTED (client hasn't called request_connect)
    CHECK(net->conn_state == pm::NetSys::ConnState::DISCONNECTED);

    net->conn_state = pm::NetSys::ConnState::CONNECTING;
    CHECK(net->conn_state == pm::NetSys::ConnState::CONNECTING);

    net->conn_state = pm::NetSys::ConnState::CONNECTED;
    CHECK(net->conn_state == pm::NetSys::ConnState::CONNECTED);
}

TEST_CASE("server connect validator registration") {
    pm::Pm pm;
    auto* net = pm.state_get<pm::NetSys>("net");

    bool called = false;
    net->connect_validator = [&](uint8_t, struct sockaddr_in&, const uint8_t*, uint16_t) {
        called = true;
        return pm::NetSys::ConnectResult::accept();
    };

    struct sockaddr_in addr{};
    uint8_t payload[] = {1};
    auto result = net->connect_validator(0, addr, payload, 1);
    CHECK(called);
    CHECK(result.accepted);
}

} // TEST_SUITE("net")

// =========================================================================
// UTILITY TESTS
// =========================================================================

TEST_SUITE("util") {

TEST_CASE("hysteresis hold blocks changes") {
    pm::Hysteresis<bool> h(false, 0.1f);
    h.set(true);
    CHECK(h.get() == true);
    // Cooldown active — can't flip back immediately
    h.set(false);
    CHECK(h.get() == true);
    // Partially tick — still blocked
    h.update(0.05f);
    h.set(false);
    CHECK(h.get() == true);
    // Tick past hold — now unblocked
    h.update(0.06f);
    h.set(false);
    CHECK(h.get() == false);
}

TEST_CASE("hysteresis same value no cooldown") {
    pm::Hysteresis<int> h(5, 1.0f);
    h.set(5);  // same value
    CHECK(h.cooldown == 0.f);
    h.set(10);  // different value
    CHECK(h.cooldown == 1.0f);
}

TEST_CASE("hysteresis operator T") {
    pm::Hysteresis<bool> h(true, 0.5f);
    bool val = h;
    CHECK(val == true);
}

TEST_CASE("cooldown fires at interval") {
    pm::Cooldown cd(0.5f);
    CHECK(!cd.ready(0.3f));
    CHECK(cd.ready(0.3f));  // 0.6 total >= 0.5
    // Overshoot preserved: elapsed should be 0.1
    CHECK(cd.remaining() > 0.39f);
    CHECK(cd.remaining() < 0.41f);
}

TEST_CASE("cooldown reset") {
    pm::Cooldown cd(1.0f);
    cd.ready(0.8f);
    cd.reset();
    CHECK(!cd.ready(0.5f));  // only 0.5 after reset
    CHECK(cd.ready(0.6f));   // 1.1 total
}

TEST_CASE("delay timer on-delay") {
    pm::DelayTimer dt(0.5f, 0.f);
    dt.update(true, 0.3f);
    CHECK(!dt.output);
    dt.update(true, 0.3f);
    CHECK(dt.output);  // 0.6 >= 0.5
}

TEST_CASE("delay timer off-delay") {
    pm::DelayTimer dt(0.f, 0.5f);
    dt.update(true, 0.1f);
    CHECK(dt.output);  // on_delay=0, instant on
    dt.update(false, 0.3f);
    CHECK(dt.output);  // off_delay not reached
    dt.update(false, 0.3f);
    CHECK(!dt.output);  // 0.6 >= 0.5
}

TEST_CASE("delay timer reassertion resets elapsed") {
    pm::DelayTimer dt(0.f, 1.0f);
    dt.update(true, 0.1f);
    CHECK(dt.output);
    dt.update(false, 0.5f);
    CHECK(dt.output);
    dt.update(true, 0.1f);  // reassert — resets off-delay elapsed
    dt.update(false, 0.5f);
    CHECK(dt.output);  // only 0.5 since reassertion, need 1.0
    dt.update(false, 0.6f);
    CHECK(!dt.output);  // 1.1 >= 1.0
}

TEST_CASE("delay timer pulse mode") {
    pm::DelayTimer pulse(0.f, 0.3f);
    // Simulate pulse: feed !output as input
    pulse.update(!pulse.output, 0.01f);  // input=true, on_delay=0 -> output=true
    CHECK(pulse.output);
    pulse.update(!pulse.output, 0.1f);   // input=false, start off-delay
    CHECK(pulse.output);
    pulse.update(!pulse.output, 0.1f);
    CHECK(pulse.output);
    pulse.update(!pulse.output, 0.15f);  // 0.35 >= 0.3 -> output=false
    CHECK(!pulse.output);
}

TEST_CASE("delay timer reset") {
    pm::DelayTimer dt(0.f, 1.0f);
    dt.update(true, 0.1f);
    CHECK(dt.output);
    dt.reset();
    CHECK(!dt.output);
    CHECK(dt.elapsed == 0.f);
}

TEST_CASE("rising edge") {
    pm::RisingEdge re;
    CHECK(!re.update(false));
    CHECK(re.update(true));   // false->true
    CHECK(!re.update(true));  // stays true — no edge
    CHECK(!re.update(false)); // true->false is falling, not rising
    CHECK(re.update(true));   // false->true again
}

TEST_CASE("falling edge") {
    pm::FallingEdge fe;
    CHECK(!fe.update(false));  // starts false, no transition
    CHECK(!fe.update(true));   // false->true is rising
    CHECK(fe.update(false));   // true->false
    CHECK(!fe.update(false));  // stays false
}

TEST_CASE("latch reset-dominant") {
    pm::Latch l;
    CHECK(!l.output);
    l.update(true, false);
    CHECK(l.output);
    l.update(false, true);
    CHECK(!l.output);
    // Both set and reset — reset wins
    l.update(true, true);
    CHECK(!l.output);
}

TEST_CASE("latch set-dominant") {
    pm::Latch l(false);  // reset_dominant = false
    l.update(true, true);
    CHECK(l.output);  // set wins
    l.update(false, true);
    CHECK(!l.output);
}

TEST_CASE("latch operator bool") {
    pm::Latch l;
    l.update(true, false);
    CHECK(l);
}

TEST_CASE("counter increment") {
    pm::Counter c(3);
    CHECK(c.count == 0);
    CHECK(!c.done);
    c.increment();
    CHECK(c.count == 1);
    CHECK(!c.done);
    c.increment();
    c.increment();
    CHECK(c.count == 3);
    CHECK(c.done);
    c.increment();  // no-op when done
    CHECK(c.count == 3);
}

TEST_CASE("counter decrement") {
    pm::Counter c(0);
    c.count = 5;
    c.decrement();
    CHECK(c.count == 4);
    CHECK(!c.done);
    c.count = 1;
    c.decrement();
    CHECK(c.count == 0);
    CHECK(c.done);
}

TEST_CASE("counter reset") {
    pm::Counter c(10);
    c.increment(); c.increment();
    c.reset();
    CHECK(c.count == 0);
    CHECK(!c.done);
    CHECK(c.preset == 10);
    c.reset(5);
    CHECK(c.preset == 5);
    CHECK(c.count == 0);
}

TEST_CASE("counter + rising edge composition") {
    pm::RisingEdge edge;
    pm::Counter c(2);
    // Simulate a signal that goes true, false, true, false, true
    bool signal[] = {true, false, true, false, true};
    for (bool s : signal) {
        if (edge.update(s)) c.increment();
    }
    CHECK(c.count == 2);
    CHECK(c.done);  // 3 rising edges but done at 2
}

} // TEST_SUITE("util")
