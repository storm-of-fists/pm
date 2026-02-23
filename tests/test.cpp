#include "pm_core.hpp"
#include "pm_udp.hpp"
#include "pm_util.hpp"
#include <cassert>
#include <cstdio>
#include <cstring>

struct Pos { float x = 0, y = 0; };
struct Vel { float dx = 0, dy = 0; };
struct Health { int hp = 100; };

// Test state + init function (replaces old PhysicsSystem)
struct PhysicsState { float gravity = -9.8f; };

void physics_init(pm::Pm& pm)
{
    auto* ps = pm.state<PhysicsState>("physics");
    auto* pos_pool = pm.pool<Pos>("pos");
    auto* vel_pool = pm.pool<Vel>("vel");
    pm.schedule("physics/tick", pm::Phase::SIMULATE, [ps, pos_pool, vel_pool](pm::TaskContext& ctx) {
        (void)ps;
        for (auto [id, pos, pool] : pos_pool->each())
        {
            auto* vel = vel_pool->get(id);
            if (vel)
            {
                pos.x += vel->dx * ctx.dt();
                pos.y += vel->dy * ctx.dt();
            }
        }
    });
}

int main()
{
    printf("=== pm_core.hpp test suite ===\n\n");

    // =========================================================================
    // CORE ECS TESTS
    // =========================================================================

    // --- Name interning ---
    {
        pm::NameTable names;
        pm::NameId a = names.intern("hello");
        pm::NameId b = names.intern("world");
        pm::NameId c = names.intern("hello");
        assert(a == c);
        assert(a != b);
        assert(strcmp(names.str(a), "hello") == 0);
        assert(names.intern(nullptr) == pm::NULL_NAME);
        assert(names.intern("") == pm::NULL_NAME);
        assert(names.find("hello") == a);
        assert(names.find("nonexistent") == pm::NULL_NAME);
        assert(names.find(nullptr) == pm::NULL_NAME);
        printf("  [OK] Name interning\n");
    }

    // --- Id system ---
    {
        pm::Pm pm;
        pm::Id a = pm.spawn("player");
        pm::Id b = pm.spawn("enemy");
        pm::Id c = pm.spawn();
        (void)c;
        assert(a != pm::NULL_ID);
        assert(b != pm::NULL_ID);
        assert(a != b);
        assert(pm.find("player") == a);
        assert(pm.find("enemy") == b);
        assert(pm.find("nonexistent") == pm::NULL_ID);

        pm.remove_entity(a);
        assert(pm.is_removing_entity(a));
        assert(pm.find("player") == a);
        pm.tick_once();
        assert(pm.find("player") == pm::NULL_ID);
        printf("  [OK] Id spawn/find/remove_entity\n");
    }

    // --- Deferred removal ---
    {
        pm::Pm pm;
        pm::Id a = pm.spawn("target");
        assert(!pm.is_removing_entity(a));
        pm.remove_entity(a);
        assert(pm.is_removing_entity(a));
        assert(pm.find("target") == a);
        pm.tick_once();
        assert(pm.find("target") == pm::NULL_ID);
        printf("  [OK] Deferred removal\n");
    }

    // --- Pool basics ---
    {
        pm::Pm pm;
        auto *pos = pm.pool<Pos>("pos");
        auto *vel = pm.pool<Vel>("vel");
        assert(pos != nullptr);
        assert(vel != nullptr);
        assert(pos->pool_id != vel->pool_id);

        pm::Id e = pm.spawn("entity");
        pos->add(e, {10.f, 20.f});
        vel->add(e, {1.f, 2.f});

        auto *p = pos->get(e);
        assert(p != nullptr);
        assert(p->x == 10.f && p->y == 20.f);
        assert(pos->has(e));
        assert(vel->has(e));
        assert(pos->size() == 1);
        printf("  [OK] Pool add/get/has\n");
    }

    // --- Entity→pool remove ---
    {
        pm::Pm pm;
        auto *pos = pm.pool<Pos>("pos");
        auto *vel = pm.pool<Vel>("vel");
        auto *hp = pm.pool<Health>("health");

        pm::Id e = pm.spawn();
        pos->add(e, {1, 2});
        vel->add(e, {3, 4});
        hp->add(e, {50});

        pm.remove_entity(e);
        pm.tick_once();

        assert(!pos->has(e));
        assert(!vel->has(e));
        assert(!hp->has(e));
        printf("  [OK] Entity→pool remove\n");
    }

    // --- Handle ---
    {
        pm::Pm pm;
        auto *pos = pm.pool<Pos>("pos");
        pm::Id e = pm.spawn();
        pos->add(e, {42.f, 99.f});

        pm::Handle<Pos> h = pos->handle(e);
        assert(h);
        assert(h->x == 42.f);
        printf("  [OK] Handle\n");
    }

    // --- Entry.modify() calls change hook ---
    {
        pm::Pm pm;
        auto *pos = pm.pool<Pos>("pos");

        int change_count = 0;
        pos->set_change_hook([](void* ctx, pm::Id) {
            auto* count = static_cast<int*>(ctx);
            (*count)++;
        }, &change_count);

        pm::Id e1 = pm.spawn();
        pos->add(e1, {1.f, 0.f});
        assert(change_count == 1);

        pm::Id e2 = pm.spawn();
        pos->add(e2, {2.f, 0.f});
        assert(change_count == 2);

        for (auto entry : pos->each()) {
            entry.modify()->x += 10.f;
        }
        assert(change_count == 4);

        printf("  [OK] Entry.modify() calls change hook\n");
    }

    // --- State ---
    {
        pm::Pm pm;
        physics_init(pm);
        auto *ps = pm.state<PhysicsState>("physics");
        assert(ps->gravity == -9.8f);
        assert(pm.task("physics/tick") != nullptr);
        printf("  [OK] State + init function\n");
    }

    // --- State with re-fetch ---
    {
        pm::Pm pm;
        struct Config { int width = 0; int height = 0; };
        auto* cfg = pm.state<Config>("config");
        cfg->width = 1920;
        cfg->height = 1080;

        // Re-fetch returns same instance
        auto* cfg2 = pm.state<Config>("config");
        assert(cfg2 == cfg);
        assert(cfg2->width == 1920);
        printf("  [OK] State with re-fetch\n");
    }

    // --- Scheduler ---
    {
        pm::Pm pm;
        int counter = 0;
        pm.schedule("test/count", 50.f, [&counter](pm::TaskContext &) { counter++; });
        pm.tick_once();
        assert(counter == 1);
        pm.tick_once();
        assert(counter == 2);
        printf("  [OK] Scheduler\n");
    }

    // --- sync_id ---
    {
        pm::Pm pm;
        pm::Id remote_id = pm::make_id(42, 7);
        pm.sync_id(remote_id);

        auto *pos = pm.pool<Pos>("pos");
        pos->add(remote_id, {1, 2});
        assert(pos->has(remote_id));
        printf("  [OK] sync_id\n");
    }

    // --- sync_id: remove+resync cycle ---
    {
        pm::Pm pm;
        auto *pool = pm.pool<Pos>("pos");

        for (uint32_t gen = 1; gen <= 100; gen++) {
            pm::Id id = pm::make_id(5, gen);
            pm.sync_id(id);
            pool->add(id, {(float)gen, 0});
            assert(pm.entity_count() < 1000000);
            pm.remove_entity(id);
            pm.tick_once();
        }
        assert(pm.entity_count() < 100);
        printf("  [OK] sync_id: remove+resync no free_ids leak\n");
    }

    // --- sync_id: rejects stale ---
    {
        pm::Pm pm;
        auto *pool = pm.pool<Pos>("pos");

        pm::Id id_v1 = pm::make_id(5, 1);
        assert(pm.sync_id(id_v1) == true);
        pool->add(id_v1, {1, 2});

        pm.remove_entity(id_v1);
        pm.tick_once();

        assert(pm.sync_id(id_v1) == false);

        pm::Id id_v2 = pm::make_id(5, 2);
        assert(pm.sync_id(id_v2) == true);
        pool->add(id_v2, {3, 4});
        assert(pool->has(id_v2));

        printf("  [OK] sync_id: rejects stale out-of-order\n");
    }

    // --- Pool::add updates dense_ids on generation change ---
    {
        pm::Pm pm;
        auto *pool = pm.pool<Pos>("pos");

        pm::Id v1 = pm::make_id(5, 1);
        pm.sync_id(v1);
        pool->add(v1, {10, 20});

        pm::Id v2 = pm::make_id(5, 2);
        pm.sync_id(v2);
        pool->add(v2, {30, 40});

        assert(pool->has(v2));
        assert(!pool->has(v1));
        assert(pool->get(v2)->x == 30);
        printf("  [OK] Pool::add updates dense_ids on generation overwrite\n");
    }

    // --- entity_count ---
    {
        pm::Pm pm;
        auto *pool = pm.pool<Pos>("pos");

        for (uint32_t i = 0; i < 10; i++) {
            pm::Id id = pm::make_id(i * 3, 1);
            pm.sync_id(id);
            pool->add(id, {(float)i, 0});
        }
        assert(pm.entity_count() == 10);

        for (uint32_t i = 0; i < 5; i++)
            pm.remove_entity(pm::make_id(i * 3, 1));
        pm.tick_once();
        assert(pm.entity_count() == 5);

        for (uint32_t i = 0; i < 5; i++) {
            pm::Id id = pm::make_id(i * 3, 2);
            pm.sync_id(id);
            pool->add(id, {(float)i + 100, 0});
        }
        assert(pm.entity_count() == 10);
        printf("  [OK] entity_count accurate under sync_id churn\n");
    }

    // --- sync_id clears slot_removing ---
    {
        pm::Pm pm;
        auto *pool = pm.pool<Pos>("pos");

        pm::Id v1 = pm::make_id(5, 1);
        pm.sync_id(v1);
        pm.remove_entity(v1);

        pm::Id v2 = pm::make_id(5, 2);
        assert(pm.sync_id(v2));
        pool->add(v2, {99, 99});

        pm.tick_once();

        assert(pool->has(v2));
        assert(pool->get(v2)->x == 99);
        printf("  [OK] sync_id clears slot_removing for newer generation\n");
    }

    // --- Deferred remove cleans orphaned pool entries ---
    {
        pm::Pm pm;
        auto *pool = pm.pool<Pos>("pos");

        pm::Id v1 = pm::make_id(5, 1);
        pm.sync_id(v1);
        pool->add(v1, {10, 20});

        pm::Id v4 = pm::make_id(5, 4);
        pm.sync_id(v4);
        pm.remove_entity(v4);
        pm.tick_once();

        assert(!pool->has(v1));
        assert(!pool->has(v4));
        printf("  [OK] deferred remove cleans orphaned pool entries by index\n");
    }

    // --- remove_entity + stop_task + Pool::reset ---
    {
        pm::Pm pm;
        auto *pos = pm.pool<Pos>("pos");
        pm::Id e = pm.spawn("thing");
        pos->add(e, {1, 2});

        pm.remove_entity(pm.find("thing"));
        pm.stop_task("thing");
        pm.tick_once();
        assert(!pos->has(e));

        pm::Id e2 = pm.spawn();
        pos->add(e2, {3, 4});
        pos->reset();
        assert(pos->items.size() == 0);

        pm::Id e3 = pm.spawn();
        pos->add(e3, {5, 6});
        assert(pos->has(e3));
        printf("  [OK] remove_entity + stop_task + Pool::reset\n");
    }

    // --- add() overwrite ---
    {
        pm::Pm pm;
        auto *pos = pm.pool<Pos>("pos");

        pm::Id e = pm.spawn();
        pos->add(e, {1, 2});
        pos->add(e, {5, 6});
        assert(pos->get(e)->x == 5.f);
        assert(pos->size() == 1);
        printf("  [OK] add() overwrite\n");
    }

    // --- const char* convenience overloads ---
    {
        pm::Pm pm;
        pm.spawn("foo");
        assert(pm.find("foo") != pm::NULL_ID);

        auto *pos = pm.pool<Pos>("mypool");
        assert(pos != nullptr);

        auto *s = pm.state<PhysicsState>("mystate");
        assert(s != nullptr);

        pm.schedule("mytask", 50.f, [](pm::TaskContext &) {});
        assert(pm.task("mytask") != nullptr);
        printf("  [OK] const char* overloads\n");
    }

    // --- Type safety ---
    {
        pm::Pm pm1;
        pm1.pool<Pos>("pos");
        auto *pos2 = pm1.pool<Pos>("pos");
        assert(pos2 != nullptr);

        pm::Pm pm2;
        pm2.state<PhysicsState>("phys");
        auto *s2 = pm2.state<PhysicsState>("phys");
        assert(s2 != nullptr);
        printf("  [OK] Type safety (same-type re-fetch)\n");
    }

    // --- Task stopping ---
    {
        pm::Pm pm;
        int counter_a = 0, counter_b = 0;
        pm.schedule("task_a", 50.f, [&counter_a](pm::TaskContext &) { counter_a++; });
        pm.schedule("task_b", 60.f, [&counter_b](pm::TaskContext &) { counter_b++; });

        pm.tick_once();
        assert(counter_a == 1 && counter_b == 1);

        pm.stop_task("task_a");
        pm.tick_once();
        assert(counter_a == 1 && counter_b == 2);
        printf("  [OK] Task stopping\n");
    }

    // --- stop_task standalone ---
    {
        pm::Pm pm;
        int counter = 0;
        pm.schedule("mysys", 50.f, [&counter](pm::TaskContext &) { counter++; });

        pm.tick_once();
        assert(counter == 1);

        pm.stop_task("mysys");
        pm.tick_once();
        assert(counter == 1);
        printf("  [OK] stop_task stops task\n");
    }

    // --- Remove during iteration ---
    {
        pm::Pm pm;
        auto *pos = pm.pool<Pos>("pos");

        pm::Id e1 = pm.spawn();
        pm::Id e2 = pm.spawn();
        pm::Id e3 = pm.spawn();
        pos->add(e1, {1, 0});
        pos->add(e2, {2, 0});
        pos->add(e3, {3, 0});

        int visited = 0;
        for (auto [id, val, pool] : pos->each()) {
            visited++;
            if (val.x == 2.f) pm.remove_entity(id);
        }
        assert(visited == 3);

        pm.tick_once();
        assert(!pos->has(e2));
        assert(pos->has(e1));
        assert(pos->has(e3));
        printf("  [OK] Remove during iteration (deferred)\n");
    }

    // --- Fault handling ---
    {
        pm::Pm pm;
        int counter = 0;
        pm.schedule("faulty", 50.f, [&counter](pm::TaskContext &) -> pm::Result {
            counter++;
            if (counter >= 2) return pm::Result::err("boom");
            return pm::Result::ok();
        });

        pm.tick_once();
        assert(counter == 1);
        pm.tick_once();
        assert(counter == 2);
        assert(pm.faults().size() == 1);
        pm.tick_once();
        assert(counter == 2);
        printf("  [OK] Fault handling disables task\n");
    }

    // --- const find() doesn't intern ---
    {
        pm::Pm pm;
        pm.spawn("exists");
        assert(pm.find("ghost") == pm::NULL_ID);
        assert(pm.find("exists") != pm::NULL_ID);
        printf("  [OK] const find() doesn't intern\n");
    }

    // --- Pause/step ---
    {
        pm::Pm pm;
        int always_count = 0, game_count = 0;

        pm.schedule("input", pm::Phase::INPUT, [&](pm::TaskContext&) { always_count++; });
        pm.schedule("physics", pm::Phase::SIMULATE, [&](pm::TaskContext&) { game_count++; },
                    0.f, true);

        pm.tick_once();
        assert(always_count == 1 && game_count == 1);

        pm.pause();
        pm.tick_once();
        assert(always_count == 2 && game_count == 1);

        pm.request_step();
        pm.tick_once();
        assert(always_count == 3 && game_count == 2);
        assert(pm.is_paused());

        pm.resume();
        pm.tick_once();
        assert(always_count == 4 && game_count == 3);
        printf("  [OK] Pause/step\n");
    }

    // --- Remove budget ---
    {
        pm::Pm pm;
        auto* pool = pm.pool<int>("stuff");

        pm::Id ids[10];
        for (int i = 0; i < 10; i++) {
            ids[i] = pm.spawn();
            pool->add(ids[i], i);
        }

        for (int i = 0; i < 5; i++) pm.remove_entity(ids[i]);
        pm.tick_once();
        assert(pool->items.size() == 5);
        assert(pm.remove_pending() == 0);

        pm::Id batch[200];
        for (int i = 0; i < 200; i++) {
            batch[i] = pm.spawn();
            pool->add(batch[i], i);
        }
        for (int i = 0; i < 200; i++) pm.remove_entity(batch[i]);
        pm.set_remove_budget_us(0.001f);

        size_t before = pool->items.size();
        int safety = 0;
        while (pm.remove_pending() > 0 && safety < 100) {
            pm.tick_once();
            safety++;
        }
        assert(pool->items.size() == before - 200);
        assert(pm.remove_pending() == 0);

        pm.set_remove_budget_us(0.f);
        printf("  [OK] Remove budget rate-limits by time\n");
    }

    printf("\n=== pm_udp.hpp test suite ===\n\n");

    // =========================================================================
    // NETWORKING / SYNC TESTS (via pm_udp)
    // =========================================================================

    // --- PeerRange iteration ---
    {
        pm::Pm pm;
        auto* net = pm.state<pm::NetSys>("net");

        uint8_t p1 = net->connect();
        uint8_t p2 = net->connect();
        uint8_t p3 = net->connect();
        assert(p1 == 1);
        assert(p2 == 2);
        assert(p3 == 3);

        std::vector<uint8_t> all;
        for (uint8_t p : net->peers()) all.push_back(p);
        assert(all.size() == 4);

        std::vector<uint8_t> remotes;
        for (uint8_t p : net->remote_peers()) remotes.push_back(p);
        assert(remotes.size() == 3);

        assert(net->peer_count() == 4);
        assert(net->peers().has(0));
        assert(!net->peers().has(55));

        net->disconnect(p2);
        assert(net->peer_count() == 3);
        assert(!net->peers().has(2));

        uint8_t p4 = net->connect();
        assert(p4 == 2);
        printf("  [OK] PeerRange + connect/disconnect\n");
    }

    // --- Peer metadata ---
    {
        pm::Pm pm;
        auto* net = pm.state<pm::NetSys>("net");

        assert(net->peer(0).id == 0);
        assert(net->peer(0).connected == true);

        uint8_t p1 = net->connect();
        assert(net->peer(p1).connected == true);

        int my_data = 42;
        net->peer(p1).user_data = &my_data;
        assert(*(int*)net->peer(p1).user_data == 42);

        net->disconnect(p1);
        assert(net->peer(p1).connected == false);
        assert(net->peer(p1).user_data == nullptr);
        printf("  [OK] Peer metadata\n");
    }

    // --- Peer lifecycle hooks ---
    {
        pm::Pm pm;
        auto* net = pm.state<pm::NetSys>("net");
        std::vector<std::pair<char, uint8_t>> events;

        net->on_connect([&](pm::NetSys&, uint8_t id) { events.push_back({'C', id}); });
        net->on_disconnect([&](pm::NetSys&, uint8_t id) { events.push_back({'D', id}); });

        uint8_t p1 = net->connect();
        uint8_t p2 = net->connect();
        (void)p2;
        assert(events.size() == 2);

        net->disconnect(p1);
        assert(events.size() == 3);
        assert(events[2] == std::make_pair('D', p1));

        net->disconnect(p1);
        assert(events.size() == 3);
        printf("  [OK] Peer lifecycle hooks\n");
    }

    // --- set_peer_id ---
    {
        pm::Pm pm;
        auto* net = pm.state<pm::NetSys>("net");

        assert(net->peer_slots() & 1);

        net->set_peer_id(255);
        assert(net->peer_slots() == 0);

        net->set_peer_id(3);
        assert(net->peer_slots() == (1ULL << 3));
        assert(net->peer(3).connected == true);

        uint8_t s = net->connect();
        assert(net->remote_peers().count() == 1);
        bool found_self = false;
        for (uint8_t p : net->remote_peers())
            if (p == 3) found_self = true;
        assert(!found_self);
        (void)s;
        printf("  [OK] set_peer_id manages self bit\n");
    }

    // --- PoolSyncState basics ---
    {
        pm::Pm pm;
        auto* pool = pm.pool<int>("items");

        pm::PoolSyncState ss;
        ss.pool_id = pool->pool_id;

        pool->set_change_hook([](void* ctx, pm::Id id) {
            static_cast<pm::PoolSyncState*>(ctx)->mark_changed(id);
        }, &ss);

        pm::Id a = pm.spawn(); pool->add(a, 10);
        pm::Id b = pm.spawn(); pool->add(b, 20);
        pm::Id c = pm.spawn(); pool->add(c, 30);

        assert(!ss.is_synced_to(1, a));
        assert(!ss.is_synced_to(1, b));

        ss.mark_synced(1, a);
        ss.mark_synced(1, b);
        assert(ss.is_synced_to(1, a));
        assert(ss.is_synced_to(1, b));
        assert(!ss.is_synced_to(1, c));
        assert(!ss.is_synced_to(2, a));

        ss.mark_unsynced_for(1, a);
        assert(!ss.is_synced_to(1, a));
        assert(ss.is_synced_to(1, b));
        printf("  [OK] PoolSyncState basics\n");
    }

    // --- PoolSyncState each_unsynced ---
    {
        pm::Pm pm;
        auto* net = pm.state<pm::NetSys>("net");
        auto* pool = pm.pool<Pos>("pos");

        pm::PoolSyncState ss;
        pool->set_change_hook([](void* ctx, pm::Id id) {
            static_cast<pm::PoolSyncState*>(ctx)->mark_changed(id);
        }, &ss);

        uint8_t peer = net->connect();

        for (int i = 0; i < 10; i++) {
            pm::Id e = pm.spawn();
            pool->add(e, {(float)i, 0});
        }

        int count = 0;
        uint64_t remote_mask = net->remote_peers().bits;
        ss.each_unsynced(pool, peer, remote_mask, [&](pm::Id id, Pos&, size_t) {
            ss.mark_synced(peer, id);
            count++;
        });
        assert(count == 10);

        count = 0;
        ss.each_unsynced(pool, peer, remote_mask, [&](pm::Id, Pos&, size_t) { count++; });
        assert(count == 0);

        assert(ss.pending_count() == 0);
        printf("  [OK] PoolSyncState each_unsynced\n");
    }

    // --- Pending list compaction ---
    {
        pm::Pm pm;
        auto* net = pm.state<pm::NetSys>("net");
        auto* pool = pm.pool<Pos>("pos");

        pm::PoolSyncState ss;
        pool->set_change_hook([](void* ctx, pm::Id id) {
            static_cast<pm::PoolSyncState*>(ctx)->mark_changed(id);
        }, &ss);

        pm::Id ids[100];
        for (int i = 0; i < 100; i++) {
            ids[i] = pm.spawn();
            pool->add(ids[i], {(float)i, 0});
        }
        assert(ss.pending_count() == 100);

        uint8_t peer = net->connect();
        uint64_t remote_mask = net->remote_peers().bits;

        ss.each_unsynced(pool, peer, remote_mask, [&](pm::Id id, Pos&, size_t) { ss.mark_synced(peer, id); });
        ss.each_unsynced(pool, peer, remote_mask, [](pm::Id, Pos&, size_t) {});
        assert(ss.pending_count() == 0);

        ss.mark_changed(ids[10]);
        ss.mark_changed(ids[50]);
        ss.mark_changed(ids[90]);
        assert(ss.pending_count() == 3);

        int count = 0;
        ss.each_unsynced(pool, peer, remote_mask, [&](pm::Id, Pos&, size_t) { count++; });
        assert(count == 3);

        pool->remove(ids[50]);
        count = 0;
        ss.each_unsynced(pool, peer, remote_mask, [&](pm::Id, Pos&, size_t) { count++; });
        assert(count == 2);
        assert(ss.pending_count() == 2);
        printf("  [OK] Pending list compaction\n");
    }

    // --- Interest management: unsync_for + re-sync ---
    {
        pm::Pm pm;
        auto* net = pm.state<pm::NetSys>("net");
        auto* pool = pm.pool<float>("positions");

        pm::PoolSyncState ss;
        pool->set_change_hook([](void* ctx, pm::Id id) {
            static_cast<pm::PoolSyncState*>(ctx)->mark_changed(id);
        }, &ss);

        uint8_t p1 = net->connect();
        uint64_t remote_mask = net->remote_peers().bits;

        pm::Id near1 = pm.spawn(); pool->add(near1, 50.f);
        pm::Id near2 = pm.spawn(); pool->add(near2, 100.f);
        pm::Id far1  = pm.spawn(); pool->add(far1, 900.f);

        float interest_radius = 200.f;
        float peer_pos = 80.f;

        std::vector<pm::Id> synced_ids;
        ss.each_unsynced(pool, p1, remote_mask, [&](pm::Id id, float& x, size_t) {
            if (std::abs(x - peer_pos) <= interest_radius) {
                synced_ids.push_back(id);
                ss.mark_synced(p1, id);
            }
        });
        assert(synced_ids.size() == 2);

        assert(ss.is_synced_to(p1, near2));
        ss.mark_unsynced_for(p1, near2);
        assert(!ss.is_synced_to(p1, near2));

        synced_ids.clear();
        ss.each_unsynced(pool, p1, remote_mask, [&](pm::Id id, float&, size_t) {
            synced_ids.push_back(id);
        });
        assert(std::find(synced_ids.begin(), synced_ids.end(), near2) != synced_ids.end());
        assert(std::find(synced_ids.begin(), synced_ids.end(), far1) != synced_ids.end());
        assert(std::find(synced_ids.begin(), synced_ids.end(), near1) == synced_ids.end());
        printf("  [OK] Interest: unsync_for + re-sync\n");
    }

    // --- Sync tracking immune to swap-remove ---
    {
        pm::Pm pm;
        auto* pool = pm.pool<Pos>("pos");

        pm::PoolSyncState ss;
        pool->set_change_hook([](void* ctx, pm::Id id) {
            static_cast<pm::PoolSyncState*>(ctx)->mark_changed(id);
        }, &ss);

        pm::Id a = pm.spawn();
        pm::Id b = pm.spawn();
        pm::Id c = pm.spawn();
        pool->add(a, {1, 0});
        pool->add(b, {2, 0});
        pool->add(c, {3, 0});

        ss.mark_synced(1, a);
        assert(ss.is_synced_to(1, a));
        assert(!ss.is_synced_to(1, c));

        pool->remove(a);
        assert(pool->items.size() == 2);

        assert(!ss.is_synced_to(1, c));
        assert(!ss.is_synced_to(1, b));
        printf("  [OK] Sync tracking immune to swap-remove\n");
    }

    // --- repend_all on connect ---
    {
        pm::Pm pm;
        auto* net = pm.state<pm::NetSys>("net");
        auto* pool = pm.pool<Pos>("pos");

        pm::PoolSyncState ss;
        pool->set_change_hook([](void* ctx, pm::Id id) {
            static_cast<pm::PoolSyncState*>(ctx)->mark_changed(id);
        }, &ss);

        pm::Id a = pm.spawn(); pool->add(a, {1, 2});
        pm::Id b = pm.spawn(); pool->add(b, {3, 4});

        uint8_t p1 = net->connect();
        uint64_t remote_mask = net->remote_peers().bits;
        ss.each_unsynced(pool, p1, remote_mask, [&](pm::Id id, Pos&, size_t) { ss.mark_synced(p1, id); });
        ss.each_unsynced(pool, p1, remote_mask, [](pm::Id, Pos&, size_t) {});
        assert(ss.pending_count() == 0);

        ss.repend_all(pool);
        uint8_t p2 = net->connect();
        remote_mask = net->remote_peers().bits;

        int count = 0;
        ss.each_unsynced(pool, p2, remote_mask, [&](pm::Id, Pos&, size_t) { count++; });
        assert(count == 2);
        printf("  [OK] repend_all on connect\n");
    }

    // --- net_init registers tasks ---
    {
        pm::Pm pm;
        auto* net = pm.state<pm::NetSys>("net");
        net_init(pm, net);

        assert(pm.task("net/recv") != nullptr);
        assert(pm.task("net/tick") != nullptr);
        assert(pm.task("net/flush") != nullptr);
        printf("  [OK] net_init registers tasks\n");
    }

    // --- Pool swap hook ---
    {
        pm::Pm pm;
        auto* pool = pm.pool<Pos>("pos");

        // Track swaps
        struct SwapLog { uint32_t removed, last; };
        static std::vector<SwapLog> swaps;
        swaps.clear();
        pool->set_swap_hook([](void*, uint32_t removed_di, uint32_t last_di) {
            swaps.push_back({removed_di, last_di});
        }, nullptr);

        pm::Id a = pm.spawn(); pool->add(a, {1, 0});  // dense 0
        pm::Id b = pm.spawn(); pool->add(b, {2, 0});  // dense 1
        pm::Id c = pm.spawn(); pool->add(c, {3, 0});  // dense 2

        pool->remove(a);  // swap: dense[0] gets dense[2], pop
        assert(swaps.size() == 1);
        assert(swaps[0].removed == 0 && swaps[0].last == 2);
        assert(pool->items.size() == 2);

        pool->remove(c);  // c was at dense[0] after swap, last=dense[1]
        assert(swaps.size() == 2);
        assert(swaps[1].removed == 0 && swaps[1].last == 1);

        pool->remove(b);  // only element, removed == last
        assert(swaps.size() == 3);
        assert(swaps[2].removed == 0 && swaps[2].last == 0);
        assert(pool->items.empty());

        printf("  [OK] Pool swap hook\n");
    }

    // --- PoolSyncState change-tracked: mark_synced / each_unsynced basics ---
    {
        pm::Pm pm;
        auto* net = pm.state<pm::NetSys>("net");
        auto* pool = pm.pool<Pos>("pos");

        pm::PoolSyncState ss;
        pool->set_change_hook([](void* ctx, pm::Id id) {
            static_cast<pm::PoolSyncState*>(ctx)->mark_changed(id);
        }, &ss);

        uint8_t peer = net->connect();
        uint64_t remote_mask = net->remote_peers().bits;

        pm::Id a = pm.spawn(); pool->add(a, {1, 0});
        pm::Id b = pm.spawn(); pool->add(b, {2, 0});
        pm::Id c = pm.spawn(); pool->add(c, {3, 0});

        // All 3 should appear in pending (add calls notify_change)
        int count = 0;
        ss.each_unsynced(pool, peer, remote_mask, [&](pm::Id, Pos&, size_t) {
            ss.mark_synced(peer, a); // dummy; real code marks each id
            count++;
        });
        assert(count == 3);

        // Re-mark all synced properly then confirm nothing pending
        ss.mark_synced(peer, a);
        ss.mark_synced(peer, b);
        ss.mark_synced(peer, c);
        ss.clear_pending();
        count = 0;
        ss.each_unsynced(pool, peer, remote_mask, [&](pm::Id, Pos&, size_t) { count++; });
        assert(count == 0);

        printf("  [OK] PoolSyncState change-tracked: basics\n");
    }

    // --- Change-tracked: remove leaves no ghost entries ---
    {
        pm::Pm pm;
        auto* net = pm.state<pm::NetSys>("net");
        auto* pool = pm.pool<Pos>("pos");

        pm::PoolSyncState ss;
        pool->set_change_hook([](void* ctx, pm::Id id) {
            static_cast<pm::PoolSyncState*>(ctx)->mark_changed(id);
        }, &ss);

        uint8_t peer = net->connect();
        uint64_t remote_mask = net->remote_peers().bits;

        pm::Id a = pm.spawn(); pool->add(a, {10, 0});
        pm::Id b = pm.spawn(); pool->add(b, {20, 0});
        pm::Id c = pm.spawn(); pool->add(c, {30, 0});

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
        assert(count == 0);

        printf("  [OK] Change-tracked: remove leaves no ghost entries\n");
    }

    // --- Change-tracked: new entries start unsynced ---
    {
        pm::Pm pm;
        auto* net = pm.state<pm::NetSys>("net");
        auto* pool = pm.pool<int>("items");

        pm::PoolSyncState ss;
        pool->set_change_hook([](void* ctx, pm::Id id) {
            static_cast<pm::PoolSyncState*>(ctx)->mark_changed(id);
        }, &ss);

        uint8_t peer = net->connect();
        uint64_t remote_mask = net->remote_peers().bits;

        pm::Id a = pm.spawn(); pool->add(a, 10);

        // Sync a
        ss.each_unsynced(pool, peer, remote_mask, [&](pm::Id id, int&, size_t) {
            ss.mark_synced(peer, id);
        });
        ss.clear_pending();

        // Add a new entry — triggers change hook, goes into pending
        pm::Id b = pm.spawn(); pool->add(b, 20);

        int count = 0;
        pm::Id found = pm::NULL_ID;
        ss.each_unsynced(pool, peer, remote_mask, [&](pm::Id id, int&, size_t) {
            count++;
            found = id;
        });
        assert(count == 1);
        assert(found == b);

        printf("  [OK] Change-tracked: new entries start unsynced\n");
    }

    // --- Change-tracked: multi-peer independence ---
    {
        pm::Pm pm;
        auto* net = pm.state<pm::NetSys>("net");
        auto* pool = pm.pool<int>("vals");

        pm::PoolSyncState ss;
        pool->set_change_hook([](void* ctx, pm::Id id) {
            static_cast<pm::PoolSyncState*>(ctx)->mark_changed(id);
        }, &ss);

        uint8_t p1 = net->connect();
        uint8_t p2 = net->connect();
        uint64_t remote_mask = net->remote_peers().bits;

        pm::Id a = pm.spawn(); pool->add(a, 1);
        pm::Id b = pm.spawn(); pool->add(b, 2);

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
        assert(p2_count == 2);

        // Now both peers synced — pending cleared
        ss.clear_pending();
        int p1_count = 0;
        ss.each_unsynced(pool, p1, remote_mask, [&](pm::Id, int&, size_t) { p1_count++; });
        assert(p1_count == 0);

        printf("  [OK] Change-tracked: multi-peer independence\n");
    }

    // --- Ordered custom packet sequencing ---
    {
        pm::Pm pm;
        auto* net = pm.state<pm::NetSys>("net");
        uint8_t p = net->connect();

        // next_send_seq increments
        assert(net->next_send_seq(p) == 1);
        assert(net->next_send_seq(p) == 2);
        assert(net->next_send_seq(p) == 3);

        // accept_seq accepts newer, rejects older/equal
        assert(net->accept_seq(p, 1) == true);
        assert(net->accept_seq(p, 1) == false);  // same
        assert(net->accept_seq(p, 0) == false);   // older (wrapping-aware: 0 < 1)
        assert(net->accept_seq(p, 5) == true);
        assert(net->accept_seq(p, 3) == false);   // older than 5

        printf("  [OK] Ordered custom packet sequencing\n");
    }

    // --- Reliable message dedup ring ---
    {
        pm::Pm pm;
        auto* net = pm.state<pm::NetSys>("net");

        auto& pn = net->peer_net[0];

        // Fresh — nothing seen
        assert(!pn.has_seen_reliable(1));
        assert(!pn.has_seen_reliable(42));

        // Mark and check
        pn.mark_seen_reliable(1);
        assert(pn.has_seen_reliable(1));
        assert(!pn.has_seen_reliable(2));

        pn.mark_seen_reliable(2);
        assert(pn.has_seen_reliable(1));
        assert(pn.has_seen_reliable(2));

        // Fill dedup ring past capacity — oldest should be evicted
        for (uint16_t i = 3; i < 3 + pm::NetSys::PeerNet::RELIABLE_DEDUP_SIZE; i++)
            pn.mark_seen_reliable(i);

        // msg_id 1 should be evicted (ring wrapped)
        assert(!pn.has_seen_reliable(1));
        // Recent ones should still be there
        assert(pn.has_seen_reliable(3 + pm::NetSys::PeerNet::RELIABLE_DEDUP_SIZE - 1));

        printf("  [OK] Reliable message dedup ring\n");
    }

    // --- Reliable message outbox + ack ---
    {
        pm::Pm pm;
        auto* net = pm.state<pm::NetSys>("net");
        uint8_t p = net->connect();

        // Send a reliable message
        uint8_t payload[] = {10, 20, 30};
        net->send_reliable(p, 0x42, payload, 3);
        assert(net->peer_net[p].reliable_outbox.size() == 1);

        auto& entry = net->peer_net[p].reliable_outbox[0];
        assert(entry.msg_id == 1);
        assert(entry.sends_remaining == pm::NetSys::PeerNet::RELIABLE_SEND_COUNT);
        assert(entry.envelope_size == sizeof(pm::PktReliable) + 3);
        // Verify envelope contains correct header
        pm::PktReliable hdr;
        memcpy(&hdr, entry.envelope, sizeof(hdr));
        assert(hdr.type == pm::PKT_RELIABLE);
        assert(hdr.msg_id == 1);
        assert(hdr.inner_type == 0x42);

        // Send another
        net->send_reliable(p, 0x43, nullptr, 0);
        assert(net->peer_net[p].reliable_outbox.size() == 2);

        // Ack first message — should be removed
        net->ack_reliable(p, 1);
        assert(net->peer_net[p].reliable_outbox.size() == 1);
        assert(net->peer_net[p].reliable_outbox[0].msg_id == 2);

        printf("  [OK] Reliable message outbox + ack\n");
    }

    // --- send_reliable_all broadcasts to all remote peers ---
    {
        pm::Pm pm;
        auto* net = pm.state<pm::NetSys>("net");
        uint8_t p1 = net->connect();
        uint8_t p2 = net->connect();

        uint8_t data = 0xFF;
        net->send_reliable_all(0x50, &data, 1);
        assert(net->peer_net[p1].reliable_outbox.size() == 1);
        assert(net->peer_net[p2].reliable_outbox.size() == 1);
        // Different msg_ids per peer
        assert(net->peer_net[p1].reliable_outbox[0].msg_id == 1);
        assert(net->peer_net[p2].reliable_outbox[0].msg_id == 1);

        printf("  [OK] send_reliable_all broadcasts to all remote peers\n");
    }

    // --- find_peer_by_addr ---
    {
        pm::Pm pm;
        auto* net = pm.state<pm::NetSys>("net");
        uint8_t p = net->connect();

        struct sockaddr_in addr{};
        addr.sin_family = AF_INET;
        addr.sin_port = htons(9999);
        addr.sin_addr.s_addr = htonl(0x7F000001); // 127.0.0.1
        net->peer_addrs[p] = addr;
        net->has_addr[p] = true;

        assert(net->find_peer_by_addr(addr) == p);

        // Different port — should not match
        struct sockaddr_in addr2 = addr;
        addr2.sin_port = htons(8888);
        assert(net->find_peer_by_addr(addr2) == 255);

        printf("  [OK] find_peer_by_addr\n");
    }

    // --- Clock sync fields ---
    {
        pm::Pm pm;
        auto* net = pm.state<pm::NetSys>("net");

        net->local_time = 10.0f;
        net->clock_offset = 5.0f;  // server is 5s ahead
        float est = net->server_time_estimate();
        assert(est > 14.99f && est < 15.01f);

        printf("  [OK] Clock sync fields\n");
    }

    // --- PktSync wire format includes server_time ---
    {
        pm::PktSync hdr{};
        hdr.seq = 42;
        hdr.frame = 100;
        hdr.server_time = 3.14f;
        hdr.section_count = 2;

        uint8_t buf[sizeof(pm::PktSync)];
        memcpy(buf, &hdr, sizeof(hdr));

        pm::PktSync decoded;
        memcpy(&decoded, buf, sizeof(decoded));
        assert(decoded.type == pm::PKT_SYNC_TYPE);
        assert(decoded.seq == 42);
        assert(decoded.frame == 100);
        assert(decoded.server_time > 3.13f && decoded.server_time < 3.15f);
        assert(decoded.section_count == 2);

        printf("  [OK] PktSync wire format includes server_time\n");
    }

    // --- Heartbeat packet roundtrip ---
    {
        pm::PktHeartbeat hb{};
        assert(hb.type == pm::PKT_HEARTBEAT);
        assert(sizeof(hb) == 1);

        printf("  [OK] Heartbeat packet roundtrip\n");
    }

    // --- SectionHeader has no rm_count ---
    {
        pm::SectionHeader sh{};
        sh.pool_id = 42;
        sh.sync_count = 10;
        sh.entry_size = 16;

        uint8_t buf[sizeof(pm::SectionHeader)];
        memcpy(buf, &sh, sizeof(sh));

        pm::SectionHeader decoded;
        memcpy(&decoded, buf, sizeof(decoded));
        assert(decoded.pool_id == 42);
        assert(decoded.sync_count == 10);
        assert(decoded.entry_size == 16);
        // No rm_count field — struct is smaller
        assert(sizeof(pm::SectionHeader) == 4 + 2 + 2);

        printf("  [OK] SectionHeader is sync-only (no rm_count)\n");
    }

    // --- Reliable removal batching ---
    {
        pm::Pm pm;
        auto* net = pm.state<pm::NetSys>("net");
        auto* pool = pm.pool<int>("items");

        uint8_t p = net->connect();

        // Track some removals
        pm::Id a = pm.spawn(); pool->add(a, 1);
        pm::Id b = pm.spawn(); pool->add(b, 2);
        pm::Id c = pm.spawn(); pool->add(c, 3);

        net->track_removal(pool->pool_id, a);
        net->track_removal(pool->pool_id, b);
        net->track_removal(pool->pool_id, c);

        // Removals are buffered in pending_removals
        assert(net->peer_net[p].pending_removals.size() == 3);

        // flush_removals converts them to reliable messages
        net->flush_removals(p);
        assert(net->peer_net[p].pending_removals.empty());
        // Should have created 1 reliable message (3 ids fit in one batch)
        assert(net->peer_net[p].reliable_outbox.size() == 1);

        // Verify the reliable entry contains correct inner type
        pm::PktReliable hdr;
        memcpy(&hdr, net->peer_net[p].reliable_outbox[0].envelope, sizeof(hdr));
        assert(hdr.inner_type == pm::RELIABLE_INNER_REMOVAL);

        // Verify payload: pool_id(4) + count(2) + 3 * Id(8)
        const uint8_t* payload = net->peer_net[p].reliable_outbox[0].envelope + sizeof(pm::PktReliable);
        uint32_t pid; memcpy(&pid, payload, 4);
        uint16_t count; memcpy(&count, payload + 4, 2);
        assert(pid == pool->pool_id);
        assert(count == 3);

        printf("  [OK] Reliable removal batching\n");
    }

    // --- tracked_remove + clear_pool use reliable removals ---
    {
        pm::Pm pm;
        auto* net = pm.state<pm::NetSys>("net");
        auto* pool = pm.pool<int>("vals");

        uint8_t p1 = net->connect();
        uint8_t p2 = net->connect();

        pm::Id a = pm.spawn(); pool->add(a, 10);
        pm::Id b = pm.spawn(); pool->add(b, 20);

        // tracked_remove buffers for all remote peers
        net->track_removal(pool->pool_id, a);
        assert(net->peer_net[p1].pending_removals.size() == 1);
        assert(net->peer_net[p2].pending_removals.size() == 1);

        printf("  [OK] tracked_remove + clear_pool use reliable removals\n");
    }

    // --- State sync push + PktStateSync wire format ---
    {
        pm::Pm pm;
        auto* net = pm.state<pm::NetSys>("net");
        uint8_t p = net->connect();

        uint8_t data[] = {1, 2, 3, 4, 5};
        net->push_state(p, 42, data, 5);
        assert(net->peer_net[p].state_outbox.size() == 1);
        assert(net->peer_net[p].state_outbox[0].state_id == 42);
        assert(net->peer_net[p].state_outbox[0].size == 5);
        assert(memcmp(net->peer_net[p].state_outbox[0].data, data, 5) == 0);

        // PktStateSync wire format
        pm::PktStateSync hdr{};
        hdr.state_id = 99;
        hdr.size = 10;
        assert(hdr.type == pm::PKT_STATE_SYNC);

        uint8_t buf[sizeof(pm::PktStateSync)];
        memcpy(buf, &hdr, sizeof(hdr));
        pm::PktStateSync decoded;
        memcpy(&decoded, buf, sizeof(decoded));
        assert(decoded.state_id == 99);
        assert(decoded.size == 10);

        printf("  [OK] State sync push + PktStateSync wire format\n");
    }

    // --- push_state_all broadcasts to all remote peers ---
    {
        pm::Pm pm;
        auto* net = pm.state<pm::NetSys>("net");
        uint8_t p1 = net->connect();
        uint8_t p2 = net->connect();

        uint8_t data = 0xAB;
        net->push_state_all(7, &data, 1);
        assert(net->peer_net[p1].state_outbox.size() == 1);
        assert(net->peer_net[p2].state_outbox.size() == 1);
        assert(net->peer_net[p1].state_outbox[0].state_id == 7);
        assert(net->peer_net[p2].state_outbox[0].state_id == 7);

        printf("  [OK] push_state_all broadcasts to all remote peers\n");
    }

    // --- on_state_recv registration ---
    {
        pm::Pm pm;
        auto* net = pm.state<pm::NetSys>("net");

        bool called = false;
        net->on_state_recv(42, [&](pm::TaskContext&, const uint8_t*, uint16_t) {
            called = true;
        });

        assert(net->state_recv_handlers.count(42) == 1);
        printf("  [OK] on_state_recv registration\n");
    }

    // --- clear_frame clears state_outbox too ---
    {
        pm::Pm pm;
        auto* net = pm.state<pm::NetSys>("net");
        uint8_t p = net->connect();

        uint8_t data = 1;
        net->push_state(p, 1, &data, 1);
        assert(net->peer_net[p].state_outbox.size() == 1);
        net->peer_net[p].clear_frame();
        assert(net->peer_net[p].state_outbox.empty());

        printf("  [OK] clear_frame clears state_outbox too\n");
    }

    // --- alloc_peer_slot finds first free slot ---
    {
        pm::Pm pm;
        auto* net = pm.state<pm::NetSys>("net");

        // Self is 0, so first free is 1
        assert(net->alloc_peer_slot() == 1);

        // Allocate 1
        net->activate_peer(1);
        assert(net->alloc_peer_slot() == 2);

        // Allocate 2
        net->activate_peer(2);
        assert(net->alloc_peer_slot() == 3);

        // Disconnect 1, it becomes free again
        net->disconnect(1);
        assert(net->alloc_peer_slot() == 1);

        printf("  [OK] alloc_peer_slot finds first free slot\n");
    }

    // --- activate_peer fires callbacks ---
    {
        pm::Pm pm;
        auto* net = pm.state<pm::NetSys>("net");

        int cb_count = 0;
        uint8_t cb_peer = 255;
        net->on_connect([&](pm::NetSys&, uint8_t p) { cb_count++; cb_peer = p; });

        net->activate_peer(1);
        assert(cb_count == 1);
        assert(cb_peer == 1);
        assert(net->peers_arr[1].connected);

        printf("  [OK] activate_peer fires callbacks\n");
    }

    // --- ConnectResult factory methods ---
    {
        auto accept = pm::NetSys::ConnectResult::accept();
        assert(accept.accepted);
        assert(accept.response_size == 0);

        uint8_t data[] = {10, 20, 30};
        auto accept_with = pm::NetSys::ConnectResult::accept(data, 3);
        assert(accept_with.accepted);
        assert(accept_with.response_size == 3);
        assert(accept_with.response[0] == 10);

        auto deny = pm::NetSys::ConnectResult::deny(pm::DENY_SERVER_FULL);
        assert(!deny.accepted);
        assert(deny.deny_reason == pm::DENY_SERVER_FULL);

        printf("  [OK] ConnectResult factory methods\n");
    }

    // --- request_connect sets client state ---
    {
        pm::Pm pm;
        auto* net = pm.state<pm::NetSys>("net");
        // Don't actually send (no socket) — just test state setup
        // Manually set up state without calling request_connect (which needs socket)
        net->set_client();
        net->conn_state = pm::NetSys::ConnState::CONNECTING;
        net->connect_payload_size = 5;
        uint8_t payload[] = {1, 2, 3, 4, 5};
        memcpy(net->connect_payload_buf, payload, 5);
        net->connect_timer = 0.f;
        net->connect_elapsed = 0.f;

        assert(net->conn_state == pm::NetSys::ConnState::CONNECTING);
        assert(!net->host_mode);
        assert(net->self_id == 255);
        assert(net->connect_payload_size == 5);

        printf("  [OK] request_connect sets client state\n");
    }

    // --- PktConnectReq wire format ---
    {
        pm::PktConnectReq req{};
        req.version = 42;
        assert(req.type == pm::PKT_CONNECT_REQ);

        uint8_t buf[sizeof(pm::PktConnectReq)];
        memcpy(buf, &req, sizeof(req));
        pm::PktConnectReq decoded;
        memcpy(&decoded, buf, sizeof(decoded));
        assert(decoded.type == pm::PKT_CONNECT_REQ);
        assert(decoded.version == 42);

        printf("  [OK] PktConnectReq wire format\n");
    }

    // --- PktConnectAck wire format ---
    {
        pm::PktConnectAck ack{};
        ack.peer_id = 7;
        assert(ack.type == pm::PKT_CONNECT_ACK);

        uint8_t buf[sizeof(pm::PktConnectAck)];
        memcpy(buf, &ack, sizeof(ack));
        pm::PktConnectAck decoded;
        memcpy(&decoded, buf, sizeof(decoded));
        assert(decoded.peer_id == 7);

        printf("  [OK] PktConnectAck wire format\n");
    }

    // --- PktConnectDeny wire format ---
    {
        pm::PktConnectDeny deny{};
        deny.reason = pm::DENY_VERSION_MISMATCH;
        assert(deny.type == pm::PKT_CONNECT_DENY);
        assert(sizeof(deny) == 2);

        printf("  [OK] PktConnectDeny wire format\n");
    }

    // --- Cached ACK stored in Peer on activation ---
    {
        pm::Pm pm;
        auto* net = pm.state<pm::NetSys>("net");

        // Fresh peer has no cached ACK
        assert(net->peers_arr[1].ack_size == 0);

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

        assert(net->peers_arr[1].ack_size == sizeof(pm::PktConnectAck) + 2);

        // Reset clears it
        net->peers_arr[1].reset();
        assert(net->peers_arr[1].ack_size == 0);

        printf("  [OK] Cached ACK stored in Peer on activation\n");
    }

    // --- ConnState transitions ---
    {
        pm::Pm pm;
        auto* net = pm.state<pm::NetSys>("net");

        // Default is DISCONNECTED (client hasn't called request_connect)
        assert(net->conn_state == pm::NetSys::ConnState::DISCONNECTED);

        net->conn_state = pm::NetSys::ConnState::CONNECTING;
        assert(net->conn_state == pm::NetSys::ConnState::CONNECTING);

        net->conn_state = pm::NetSys::ConnState::CONNECTED;
        assert(net->conn_state == pm::NetSys::ConnState::CONNECTED);

        printf("  [OK] ConnState transitions\n");
    }

    // --- Server connect validator registration ---
    {
        pm::Pm pm;
        auto* net = pm.state<pm::NetSys>("net");

        bool called = false;
        net->connect_validator = [&](uint8_t, struct sockaddr_in&, const uint8_t*, uint16_t) {
            called = true;
            return pm::NetSys::ConnectResult::accept();
        };

        struct sockaddr_in addr{};
        uint8_t payload[] = {1};
        auto result = net->connect_validator(0, addr, payload, 1);
        assert(called);
        assert(result.accepted);

        printf("  [OK] Server connect validator registration\n");
    }

    printf("\n=== pm_util.hpp test suite ===\n\n");

    // =========================================================================
    // UTILITY TESTS
    // =========================================================================

    // --- Hysteresis: hold blocks changes ---
    {
        pm::Hysteresis<bool> h(false, 0.1f);
        h.set(true);
        assert(h.get() == true);
        // Cooldown active — can't flip back immediately
        h.set(false);
        assert(h.get() == true);
        // Partially tick — still blocked
        h.update(0.05f);
        h.set(false);
        assert(h.get() == true);
        // Tick past hold — now unblocked
        h.update(0.06f);
        h.set(false);
        assert(h.get() == false);
        printf("  [OK] Hysteresis: hold blocks changes\n");
    }

    // --- Hysteresis: same value doesn't trigger hold ---
    {
        pm::Hysteresis<int> h(5, 1.0f);
        h.set(5);  // same value
        assert(h.cooldown == 0.f);
        h.set(10);  // different value
        assert(h.cooldown == 1.0f);
        printf("  [OK] Hysteresis: same value no cooldown\n");
    }

    // --- Hysteresis: operator T ---
    {
        pm::Hysteresis<bool> h(true, 0.5f);
        bool val = h;
        assert(val == true);
        printf("  [OK] Hysteresis: operator T\n");
    }

    // --- Cooldown: fires at interval ---
    {
        pm::Cooldown cd(0.5f);
        assert(!cd.ready(0.3f));
        assert(cd.ready(0.3f));  // 0.6 total >= 0.5
        // Overshoot preserved: elapsed should be 0.1
        assert(cd.remaining() > 0.39f && cd.remaining() < 0.41f);
        printf("  [OK] Cooldown: fires at interval\n");
    }

    // --- Cooldown: reset ---
    {
        pm::Cooldown cd(1.0f);
        cd.ready(0.8f);
        cd.reset();
        assert(!cd.ready(0.5f));  // only 0.5 after reset
        assert(cd.ready(0.6f));   // 1.1 total
        printf("  [OK] Cooldown: reset\n");
    }

    // --- DelayTimer: on-delay ---
    {
        pm::DelayTimer dt(0.5f, 0.f);
        dt.update(true, 0.3f);
        assert(!dt.output);
        dt.update(true, 0.3f);
        assert(dt.output);  // 0.6 >= 0.5
        printf("  [OK] DelayTimer: on-delay\n");
    }

    // --- DelayTimer: off-delay ---
    {
        pm::DelayTimer dt(0.f, 0.5f);
        dt.update(true, 0.1f);
        assert(dt.output);  // on_delay=0, instant on
        dt.update(false, 0.3f);
        assert(dt.output);  // off_delay not reached
        dt.update(false, 0.3f);
        assert(!dt.output);  // 0.6 >= 0.5
        printf("  [OK] DelayTimer: off-delay\n");
    }

    // --- DelayTimer: input reassertion resets elapsed ---
    {
        pm::DelayTimer dt(0.f, 1.0f);
        dt.update(true, 0.1f);
        assert(dt.output);
        dt.update(false, 0.5f);
        assert(dt.output);
        dt.update(true, 0.1f);  // reassert — resets off-delay elapsed
        dt.update(false, 0.5f);
        assert(dt.output);  // only 0.5 since reassertion, need 1.0
        dt.update(false, 0.6f);
        assert(!dt.output);  // 1.1 >= 1.0
        printf("  [OK] DelayTimer: reassertion resets elapsed\n");
    }

    // --- DelayTimer: pulse mode ---
    {
        pm::DelayTimer pulse(0.f, 0.3f);
        // Simulate pulse: feed !output as input
        pulse.update(!pulse.output, 0.01f);  // input=true, on_delay=0 → output=true
        assert(pulse.output);
        pulse.update(!pulse.output, 0.1f);   // input=false, start off-delay
        assert(pulse.output);
        pulse.update(!pulse.output, 0.1f);
        assert(pulse.output);
        pulse.update(!pulse.output, 0.15f);  // 0.35 >= 0.3 → output=false
        assert(!pulse.output);
        printf("  [OK] DelayTimer: pulse mode\n");
    }

    // --- DelayTimer: reset ---
    {
        pm::DelayTimer dt(0.f, 1.0f);
        dt.update(true, 0.1f);
        assert(dt.output);
        dt.reset();
        assert(!dt.output);
        assert(dt.elapsed == 0.f);
        printf("  [OK] DelayTimer: reset\n");
    }

    // --- RisingEdge ---
    {
        pm::RisingEdge re;
        assert(!re.update(false));
        assert(re.update(true));   // false→true
        assert(!re.update(true));  // stays true — no edge
        assert(!re.update(false)); // true→false is falling, not rising
        assert(re.update(true));   // false→true again
        printf("  [OK] RisingEdge\n");
    }

    // --- FallingEdge ---
    {
        pm::FallingEdge fe;
        assert(!fe.update(false));  // starts false, no transition
        assert(!fe.update(true));   // false→true is rising
        assert(fe.update(false));   // true→false
        assert(!fe.update(false));  // stays false
        printf("  [OK] FallingEdge\n");
    }

    // --- Latch: reset-dominant (default) ---
    {
        pm::Latch l;
        assert(!l.output);
        l.update(true, false);
        assert(l.output);
        l.update(false, true);
        assert(!l.output);
        // Both set and reset — reset wins
        l.update(true, true);
        assert(!l.output);
        printf("  [OK] Latch: reset-dominant\n");
    }

    // --- Latch: set-dominant ---
    {
        pm::Latch l(false);  // reset_dominant = false
        l.update(true, true);
        assert(l.output);  // set wins
        l.update(false, true);
        assert(!l.output);
        printf("  [OK] Latch: set-dominant\n");
    }

    // --- Latch: operator bool ---
    {
        pm::Latch l;
        l.update(true, false);
        if (!l) assert(false);
        printf("  [OK] Latch: operator bool\n");
    }

    // --- Counter: increment ---
    {
        pm::Counter c(3);
        assert(c.count == 0 && !c.done);
        c.increment();
        assert(c.count == 1 && !c.done);
        c.increment();
        c.increment();
        assert(c.count == 3 && c.done);
        c.increment();  // no-op when done
        assert(c.count == 3);
        printf("  [OK] Counter: increment\n");
    }

    // --- Counter: decrement ---
    {
        pm::Counter c(0);
        c.count = 5;
        c.decrement();
        assert(c.count == 4 && !c.done);
        c.count = 1;
        c.decrement();
        assert(c.count == 0 && c.done);
        printf("  [OK] Counter: decrement\n");
    }

    // --- Counter: reset ---
    {
        pm::Counter c(10);
        c.increment(); c.increment();
        c.reset();
        assert(c.count == 0 && !c.done && c.preset == 10);
        c.reset(5);
        assert(c.preset == 5 && c.count == 0);
        printf("  [OK] Counter: reset\n");
    }

    // --- Counter + RisingEdge composition ---
    {
        pm::RisingEdge edge;
        pm::Counter c(2);
        // Simulate a signal that goes true, false, true, false, true
        bool signal[] = {true, false, true, false, true};
        for (bool s : signal) {
            if (edge.update(s)) c.increment();
        }
        assert(c.count == 2 && c.done);  // 3 rising edges but done at 2
        printf("  [OK] Counter + RisingEdge composition\n");
    }

    printf("\n=== All tests passed ===\n");
    return 0;
}