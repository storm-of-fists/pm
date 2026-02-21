#include "pm_core.hpp"
#include <cassert>
#include <cstdio>
#include <cstring>

struct Pos { float x = 0, y = 0; };
struct Vel { float dx = 0, dy = 0; };
struct Health { int hp = 100; };

// Test System
class PhysicsSystem : public pm::System
{
public:
    float gravity = -9.8f;
    void initialize(pm::Pm &pm) override
    {
        auto *pos_pool = pm.pool<Pos>("pos");
        auto *vel_pool = pm.pool<Vel>("vel");
        pm.schedule("physics/tick", pm::Pm::Phase::SIMULATE, [pos_pool, vel_pool](pm::Ctx &ctx) {
            for (auto [id, pos, pool] : pos_pool->each())
            {
                auto *vel = vel_pool->get(id);
                if (vel)
                {
                    pos.x += vel->dx * ctx.dt();
                    pos.y += vel->dy * ctx.dt();
                }
            }
        });
    }
};

int main()
{
    printf("=== pm_core.hpp test suite ===\n");

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

        // find() is const and never inserts
        assert(names.find("hello") == a);
        assert(names.find("nonexistent") == pm::NULL_NAME);
        assert(names.find(nullptr) == pm::NULL_NAME);
        printf("  [OK] Name interning\n");
    }

    // --- Id system (remove is always deferred) ---
    {
        pm::Pm pm;
        pm::Id a = pm.spawn("player");
        pm::Id b = pm.spawn("enemy");
        pm::Id c = pm.spawn(); // anonymous
        (void)c;
        assert(a != pm::NULL_ID);
        assert(b != pm::NULL_ID);
        assert(a != b);
        assert(pm.find("player") == a);
        assert(pm.find("enemy") == b);
        assert(pm.find("nonexistent") == pm::NULL_ID);

        pm.remove(a);
        assert(pm.is_removing(a)); // queued but not yet dead
        assert(pm.find("player") == a); // still findable this frame
        pm.run(); // processes deferred removes
        assert(pm.find("player") == pm::NULL_ID);
        printf("  [OK] Id spawn/find/remove\n");
    }

    // --- Deferred removal ---
    {
        pm::Pm pm;
        pm::Id a = pm.spawn("target");
        assert(!pm.is_removing(a));
        pm.remove(a); // always deferred now
        assert(pm.is_removing(a));
        assert(pm.find("target") == a); // still alive this frame
        pm.run();
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
        printf("  [OK] Pool add/get/has\n");
    }

    // --- Entity→pool bitset remove ---
    {
        pm::Pm pm;
        auto *pos = pm.pool<Pos>("pos");
        auto *vel = pm.pool<Vel>("vel");
        auto *hp = pm.pool<Health>("health");

        pm::Id e = pm.spawn();
        pos->add(e, {1, 2});
        vel->add(e, {3, 4});
        hp->add(e, {50});

        assert(pos->has(e));
        assert(vel->has(e));
        assert(hp->has(e));

        pm.remove(e);
        pm.run(); // deferred removal processes here

        assert(!pos->has(e));
        assert(!vel->has(e));
        assert(!hp->has(e));
        printf("  [OK] Entity→pool bitset remove\n");
    }

    // --- Sync bitmask tracking ---
    {
        pm::Pm pm;
        auto *pos = pm.pool<Pos>("pos");

        pm::Id e = pm.spawn();
        pos->add(e, {1.f, 2.f});

        // New entry starts all-unsynced
        assert(pos->synced[0] == 0);

        // Sync for peer 0 sets that bit
        pos->sync(0, 0);
        assert((pos->synced[0] & 1ULL) != 0);
        assert((pos->synced[0] & (1ULL << 1)) == 0); // peer 1 still unsynced

        // Sync for peer 1
        pos->sync(1, 0);
        assert((pos->synced[0] & (1ULL << 1)) != 0);

        // unsync() clears for all
        pos->unsync(e);
        assert(pos->synced[0] == 0);

        // Remove doesn't crash
        pos->remove(e);
        assert(pos->items.empty());
        assert(pos->synced.empty());

        printf("  [OK] Sync bitmask tracking\n");
    }

    // --- Per-peer sync and sync_all ---
    {
        pm::Pm pm;
        auto *pos = pm.pool<Pos>("pos");

        pm::Id ids[5];
        for (int i = 0; i < 5; i++) {
            ids[i] = pm.spawn();
            pos->add(ids[i], {(float)i, 0});
        }

        // All 5 entries unsynced for peer 3
        int unsynced_count = 0;
        pos->each_unsynced(3, [&](pm::Id, Pos&, size_t) { unsynced_count++; });
        assert(unsynced_count == 5);

        // Sync all for peer 3
        pos->sync_all(3);
        unsynced_count = 0;
        pos->each_unsynced(3, [&](pm::Id, Pos&, size_t) { unsynced_count++; });
        assert(unsynced_count == 0);

        // Peer 5 still sees all unsynced
        unsynced_count = 0;
        pos->each_unsynced(5, [&](pm::Id, Pos&, size_t) { unsynced_count++; });
        assert(unsynced_count == 5);

        printf("  [OK] Per-peer sync and sync_all\n");
    }

    // --- each_unsynced iteration with selective sync ---
    {
        pm::Pm pm;
        auto *pos = pm.pool<Pos>("pos");

        for (int i = 0; i < 10; i++) {
            pm::Id e = pm.spawn();
            pos->add(e, {(float)i, 0});
        }

        // Sync only even-indexed entries for peer 2
        for (size_t i = 0; i < pos->items.size(); i += 2)
            pos->sync(2, i);

        int unsynced_count = 0;
        pos->each_unsynced(2, [&](pm::Id, Pos&, size_t) { unsynced_count++; });
        assert(unsynced_count == 5); // only odd indices

        printf("  [OK] each_unsynced selective sync\n");
    }

    // --- New peer sees everything unsynced ---
    {
        pm::Pm pm;
        auto *pos = pm.pool<Pos>("pos");

        pm::Id a = pm.spawn();
        pm::Id b = pm.spawn();
        pos->add(a, {1, 2});
        pos->add(b, {3, 4});

        // Connect peer 1, sync everything, drain pending
        uint8_t p1 = pm.connect();
        pos->sync_all(p1);
        pos->each_unsynced(p1, [](pm::Id, Pos&, size_t) {});
        assert(pos->pending_count() == 0);

        // New peer connects — repend_all ensures they see everything
        uint8_t p2 = pm.connect();
        int unsynced_count = 0;
        pos->each_unsynced(p2, [&](pm::Id, Pos&, size_t) { unsynced_count++; });
        assert(unsynced_count == 2);

        printf("  [OK] New peer sees all unsynced\n");
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

    // --- Handle.modify() marks unsynced ---
    {
        pm::Pm pm;
        auto *pos = pm.pool<Pos>("pos");

        pm::Id e = pm.spawn();
        pos->add(e, {1.f, 2.f});

        // Sync peers 0 and 1
        pos->sync(0, 0);
        pos->sync(1, 0);
        assert(pos->synced[0] == 3ULL); // bits 0 and 1 set

        pm::Handle<Pos> h = pos->handle(e);
        Pos *p = h.modify();
        p->x = 99.f;

        // modify() should unsync for all
        assert(pos->synced[0] == 0);
        printf("  [OK] Handle.modify()\n");
    }

    // --- Entry.modify() marks unsynced ---
    {
        pm::Pm pm;
        auto *pos = pm.pool<Pos>("pos");

        pm::Id e1 = pm.spawn();
        pm::Id e2 = pm.spawn();
        pos->add(e1, {1.f, 0.f});
        pos->add(e2, {2.f, 0.f});

        // Sync both entries for peer 0
        pos->sync(0, 0);
        pos->sync(0, 1);
        assert(pos->synced[0] == 1ULL);
        assert(pos->synced[1] == 1ULL);

        // Use entry.modify() during iteration
        for (auto entry : pos->each())
        {
            Pos *m = entry.modify();
            m->x += 10.f;
        }

        // Both entries should be unsynced again
        assert(pos->synced[0] == 0);
        assert(pos->synced[1] == 0);
        printf("  [OK] Entry.modify()\n");
    }

    // --- Send/ack round-trip ---
    {
        pm::Pm pm;
        auto *pos = pm.pool<Pos>("pos");

        uint8_t peer = pm.connect();
        assert(peer == 1);

        // Spawn some entities
        pm::Id a = pm.spawn();
        pm::Id b = pm.spawn();
        pos->add(a, {1, 2});
        pos->add(b, {3, 4});

        // Both unsynced for peer
        int count = 0;
        pos->each_unsynced(peer, [&](pm::Id, Pos&, size_t) { count++; });
        assert(count == 2);

        // Build a packet — track both entries
        uint16_t seq = pm.send_begin(peer);
        assert(seq == 1);
        pos->each_unsynced(peer, [&](pm::Id, Pos&, size_t di) {
            pm.send_track(peer, pos, di);
        });
        pm.send_end(peer);

        // Still unsynced (packet not acked yet)
        count = 0;
        pos->each_unsynced(peer, [&](pm::Id, Pos&, size_t) { count++; });
        assert(count == 2);

        // Peer acks the packet
        pm.recv_ack(peer, seq);

        // Now synced
        count = 0;
        pos->each_unsynced(peer, [&](pm::Id, Pos&, size_t) { count++; });
        assert(count == 0);

        // Modify one entry — becomes unsynced again
        pos->unsync(a);
        count = 0;
        pos->each_unsynced(peer, [&](pm::Id, Pos&, size_t) { count++; });
        assert(count == 1);

        // Build and ack another packet
        uint16_t seq2 = pm.send_begin(peer);
        assert(seq2 == 2);
        pos->each_unsynced(peer, [&](pm::Id, Pos&, size_t di) {
            pm.send_track(peer, pos, di);
        });
        pm.send_end(peer);
        pm.recv_ack(peer, seq2);

        count = 0;
        pos->each_unsynced(peer, [&](pm::Id, Pos&, size_t) { count++; });
        assert(count == 0);

        printf("  [OK] Send/ack round-trip\n");
    }

    // --- Lost packet self-heals ---
    {
        pm::Pm pm;
        auto *pos = pm.pool<Pos>("pos");

        uint8_t peer = pm.connect();
        pm::Id e = pm.spawn();
        pos->add(e, {1, 2});

        // Send packet 1 — never acked (simulates loss)
        uint16_t lost_seq = pm.send_begin(peer);
        pos->each_unsynced(peer, [&](pm::Id, Pos&, size_t di) {
            pm.send_track(peer, pos, di);
        });
        pm.send_end(peer);

        // Entry still unsynced since no ack
        int count = 0;
        pos->each_unsynced(peer, [&](pm::Id, Pos&, size_t) { count++; });
        assert(count == 1);

        // Send packet 2 — picks it up again
        uint16_t seq2 = pm.send_begin(peer);
        pos->each_unsynced(peer, [&](pm::Id, Pos&, size_t di) {
            pm.send_track(peer, pos, di);
        });
        pm.send_end(peer);

        // Ack only packet 2
        pm.recv_ack(peer, seq2);

        count = 0;
        pos->each_unsynced(peer, [&](pm::Id, Pos&, size_t) { count++; });
        assert(count == 0);

        // Late ack of lost packet is harmless
        pm.recv_ack(peer, lost_seq);

        printf("  [OK] Lost packet self-heals\n");
    }

    // --- recv_ack_range cumulative ack ---
    {
        pm::Pm pm;
        auto *pos = pm.pool<Pos>("pos");

        uint8_t peer = pm.connect();
        pm::Id a = pm.spawn();
        pm::Id b = pm.spawn();
        pm::Id c = pm.spawn();
        pos->add(a, {1, 0});
        pos->add(b, {2, 0});
        pos->add(c, {3, 0});

        // Send 3 packets, one entry each
        uint16_t s1 = pm.send_begin(peer);
        pm.send_track(peer, pos, 0);
        pm.send_end(peer);

        pm.send_begin(peer);
        pm.send_track(peer, pos, 1);
        pm.send_end(peer);

        uint16_t s3 = pm.send_begin(peer);
        pm.send_track(peer, pos, 2);
        pm.send_end(peer);

        // Cumulative ack through s3 syncs all three
        pm.recv_ack_range(peer, s1, s3);

        int count = 0;
        pos->each_unsynced(peer, [&](pm::Id, Pos&, size_t) { count++; });
        assert(count == 0);

        printf("  [OK] recv_ack_range cumulative ack\n");
    }

    // --- Peer connect/disconnect ---
    {
        pm::Pm pm;
        assert(pm.is_host());
        assert(pm.peer_count() == 1); // host only
        assert(pm.peer_slots() == 1); // bit 0

        uint8_t p1 = pm.connect();
        uint8_t p2 = pm.connect();
        uint8_t p3 = pm.connect();
        assert(p1 == 1);
        assert(p2 == 2);
        assert(p3 == 3);
        assert(pm.peer_count() == 4); // host + 3

        pm.disconnect(p2);
        assert(pm.peer_count() == 3);
        assert(!(pm.peer_slots() & (1ULL << 2))); // slot 2 freed

        // Next connect reuses slot 2
        uint8_t p4 = pm.connect();
        assert(p4 == 2);
        assert(pm.peer_count() == 4);

        // Can't disconnect host
        pm.disconnect(0);
        assert(pm.peer_count() == 4);

        printf("  [OK] Peer connect/disconnect\n");
    }

    // --- Queue ---
    {
        pm::Pm pm;
        auto *keys = pm.queue<int>("keys");
        keys->push(42);
        keys->push(99);
        assert(keys->size() == 2);
        pm.run();
        assert(keys->size() == 0);
        printf("  [OK] Queue\n");
    }

    // --- System ---
    {
        pm::Pm pm;
        auto *phys = pm.sys<PhysicsSystem>("physics");
        assert(phys->gravity == -9.8f);
        assert(phys->m_name_str == "physics");
        assert(phys->tname("update") == "physics/update");
        assert(phys->tname("draw") == "physics/draw");
        printf("  [OK] System\n");
    }

    // --- tname: auto-prefixed task names ---
    {
        pm::Pm pm;
        struct TestSys : pm::System {
            std::string got_name;
            void initialize(pm::Pm& pm) override {
                pm.schedule(tname("tick").c_str(), pm::Pm::Phase::SIMULATE, [this](pm::Ctx&) {});
                got_name = tname("tick");
            }
        };
        auto* sys = pm.sys<TestSys>("my_sys");
        assert(sys->got_name == "my_sys/tick");
        // Verify the task was actually registered with the prefixed name
        assert(pm.task("my_sys/tick") != nullptr);
        printf("  [OK] tname: auto-prefixed task names\n");
    }

    // --- Scheduler ---
    {
        pm::Pm pm;
        int counter = 0;
        pm.schedule("test/count", 50.f, [&counter](pm::Ctx &) { counter++; });
        pm.run();
        assert(counter == 1);
        pm.run();
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

    // --- Drop (entity-only) + Pool::reset ---
    {
        pm::Pm pm;
        auto *pos = pm.pool<Pos>("pos");
        pm::Id e = pm.spawn("thing");
        pos->add(e, {1, 2});
        assert(pos->has(e));

        // drop() removes named entity + stops tasks, but pool survives
        pm.drop("thing");
        pm.run();
        assert(!pos->has(e)); // entity removed from pool via linear scan

        // Pool::reset() clears all data and releases memory
        pm::Id e2 = pm.spawn();
        pos->add(e2, {3, 4});
        assert(pos->items.size() == 1);
        pos->reset();
        assert(pos->items.size() == 0);
        assert(!pos->has(e2));

        // Pool is still alive and usable after reset
        pm::Id e3 = pm.spawn();
        pos->add(e3, {5, 6});
        assert(pos->has(e3));
        assert(pos->get(e3)->x == 5.f);

        printf("  [OK] Drop + Pool::reset\n");
    }

    // --- add() re-unsyncing on overwrite ---
    {
        pm::Pm pm;
        auto *pos = pm.pool<Pos>("pos");

        pm::Id e = pm.spawn();
        pos->add(e, {1, 2});
        pos->sync_all(0); // simulate fully synced for peer 0
        assert(pos->synced[0] != 0);

        // add() on existing entry should unsync for all
        pos->add(e, {5, 6});
        assert(pos->synced[0] == 0);
        assert(pos->get(e)->x == 5.f);

        printf("  [OK] add() re-unsyncing on overwrite\n");
    }

    // --- Synced survives swap-remove ---
    {
        pm::Pm pm;
        auto *pos = pm.pool<Pos>("pos");

        pm::Id a = pm.spawn();
        pm::Id b = pm.spawn();
        pm::Id c = pm.spawn();
        pos->add(a, {1, 0});
        pos->add(b, {2, 0});
        pos->add(c, {3, 0});

        // Sync 'a' (dense 0) for peer 0, leave b,c unsynced
        pos->sync(0, 0);
        uint64_t c_synced = pos->synced[2];

        // Remove 'a' — 'c' swaps into dense 0
        pos->remove(a);
        assert(pos->items.size() == 2);
        // c's synced mask should have followed the swap
        assert(pos->synced[0] == c_synced);

        printf("  [OK] Synced survives swap-remove\n");
    }

    // --- Pending list compaction ---
    {
        pm::Pm pm;
        auto *pos = pm.pool<Pos>("pos");

        // No remote peers — pending grows but doesn't compact
        pm::Id ids[100];
        for (int i = 0; i < 100; i++) {
            ids[i] = pm.spawn();
            pos->add(ids[i], {(float)i, 0});
        }
        assert(pos->pending_count() == 100);

        // Connect a peer — repend_all
        uint8_t peer = pm.connect();

        // each_unsynced only iterates pending, not all items
        int count = 0;
        pos->each_unsynced(peer, [&](pm::Id, Pos&, size_t) { count++; });
        assert(count == 100);

        // Sync all for that peer
        pos->sync_all(peer);

        // Drain pending — should compact to 0 (all synced for all remote peers)
        pos->each_unsynced(peer, [](pm::Id, Pos&, size_t) {});
        assert(pos->pending_count() == 0);

        // Unsync just 3 entries
        pos->unsync(ids[10]);
        pos->unsync(ids[50]);
        pos->unsync(ids[90]);
        assert(pos->pending_count() == 3);

        count = 0;
        pos->each_unsynced(peer, [&](pm::Id, Pos&, size_t) { count++; });
        assert(count == 3);

        // Remove an entity — stale entry compacted on next each_unsynced
        pos->remove(ids[50]);
        count = 0;
        pos->each_unsynced(peer, [&](pm::Id, Pos&, size_t) { count++; });
        assert(count == 2);
        assert(pos->pending_count() == 2); // stale entry compacted

        printf("  [OK] Pending list compaction\n");
    }

    // --- const char* convenience overloads ---
    {
        pm::Pm pm;
        pm.spawn("foo");
        assert(pm.find("foo") != pm::NULL_ID);

        auto *pos = pm.pool<Pos>("mypool");
        assert(pos != nullptr);

        auto *q = pm.queue<int>("myqueue");
        assert(q != nullptr);

        pm.schedule("mytask", 50.f, [](pm::Ctx &) {});
        assert(pm.task("mytask") != nullptr);

        printf("  [OK] const char* overloads\n");
    }

    // --- Type safety: pool/queue/system type mismatch asserts ---
    {
        // Pool type safety
        pm::Pm pm1;
        pm1.pool<Pos>("pos"); // creates as Pos
        // pm1.pool<Health>("pos"); // would assert: "Pool type mismatch!"
        // (can't test assert death easily, just verify same-type works)
        auto *pos2 = pm1.pool<Pos>("pos");
        assert(pos2 != nullptr);

        // Queue type safety
        pm::Pm pm2;
        pm2.queue<int>("events");
        auto *q2 = pm2.queue<int>("events");
        assert(q2 != nullptr);

        // System type safety
        pm::Pm pm3;
        pm3.sys<PhysicsSystem>("phys");
        auto *s2 = pm3.sys<PhysicsSystem>("phys");
        assert(s2 != nullptr);

        printf("  [OK] Type safety (same-type re-fetch)\n");
    }

    // --- Deferred task stopping ---
    {
        pm::Pm pm;
        int counter_a = 0, counter_b = 0;
        pm.schedule("task_a", 50.f, [&counter_a](pm::Ctx &) { counter_a++; });
        pm.schedule("task_b", 60.f, [&counter_b](pm::Ctx &) { counter_b++; });

        pm.run();
        assert(counter_a == 1);
        assert(counter_b == 1);

        pm.stop_task_deferred("task_a");
        pm.run();
        assert(counter_a == 2);
        assert(counter_b == 2);

        pm.run();
        assert(counter_a == 2);
        assert(counter_b == 3);

        printf("  [OK] Deferred task stopping\n");
    }

    // --- stop_task immediate ---
    {
        pm::Pm pm;
        int counter = 0;
        pm.schedule("victim", 50.f, [&counter](pm::Ctx &) { counter++; });
        pm.run();
        assert(counter == 1);

        pm.stop_task("victim");
        pm.run();
        assert(counter == 1);
        printf("  [OK] Immediate task stopping\n");
    }

    // --- drop() defers task stop ---
    {
        pm::Pm pm;
        int counter = 0;

        auto *pos = pm.pool<Pos>("mysys");
        (void)pos;
        pm.schedule("mysys", 50.f, [&counter](pm::Ctx &) { counter++; });

        pm.run();
        assert(counter == 1);

        pm.drop("mysys");
        pm.run();
        assert(counter == 2);

        pm.run();
        assert(counter == 2);
        printf("  [OK] drop() defers task stop\n");
    }

    // --- Deterministic shutdown order ---
    {
        static std::vector<int> shutdown_order;
        shutdown_order.clear();

        struct SysA : pm::System {
            void shutdown(pm::Pm &) override { shutdown_order.push_back(1); }
        };
        struct SysB : pm::System {
            void shutdown(pm::Pm &) override { shutdown_order.push_back(2); }
        };
        struct SysC : pm::System {
            void shutdown(pm::Pm &) override { shutdown_order.push_back(3); }
        };

        {
            pm::Pm pm;
            pm.sys<SysA>("a");
            pm.sys<SysB>("b");
            pm.sys<SysC>("c");
        }

        assert(shutdown_order.size() == 3);
        assert(shutdown_order[0] == 3);
        assert(shutdown_order[1] == 2);
        assert(shutdown_order[2] == 1);
        printf("  [OK] Deterministic shutdown order\n");
    }

    // --- Remove during iteration is safe (deferred) ---
    {
        pm::Pm pm;
        auto *pos = pm.pool<Pos>("pos");

        pm::Id e1 = pm.spawn();
        pm::Id e2 = pm.spawn();
        pm::Id e3 = pm.spawn();
        pos->add(e1, {1, 0});
        pos->add(e2, {2, 0});
        pos->add(e3, {3, 0});

        // Remove during iteration — should not crash or skip
        int visited = 0;
        for (auto [id, val, pool] : pos->each())
        {
            visited++;
            if (val.x == 2.f)
                pm.remove(id); // deferred, so iteration continues safely
        }
        assert(visited == 3);

        // Still alive until end of frame
        assert(pos->has(e2));
        pm.run();
        assert(!pos->has(e2));
        assert(pos->has(e1));
        assert(pos->has(e3));
        printf("  [OK] Remove during iteration (deferred)\n");
    }

    // --- Fault handling disables task ---
    {
        pm::Pm pm;
        int counter = 0;
        pm.schedule("faulty", 50.f, [&counter](pm::Ctx &) -> pm::Result {
            counter++;
            if (counter >= 2)
                return pm::Result::err("boom");
            return pm::Result::ok();
        });

        pm.run(); // counter=1, ok
        assert(counter == 1);
        pm.run(); // counter=2, faults
        assert(counter == 2);
        assert(pm.faults().size() == 1);
        pm.run(); // task disabled, shouldn't run
        assert(counter == 2);
        printf("  [OK] Fault handling disables task\n");
    }

    // --- const find() doesn't intern ---
    {
        pm::Pm pm;
        pm.spawn("exists");

        // find() on a name that was never interned should return NULL_ID
        // without adding it to the name table
        assert(pm.find("ghost") == pm::NULL_ID);
        assert(pm.find("exists") != pm::NULL_ID);
        printf("  [OK] const find() doesn't intern\n");
    }

    // --- Pause skips pauseable tasks, step() runs one frame ---
    {
        pm::Pm pm;
        int always_count = 0, game_count = 0;

        pm.schedule("input", pm::Pm::Phase::INPUT, [&](pm::Ctx&) { always_count++; });
        pm.schedule("physics", pm::Pm::Phase::SIMULATE, [&](pm::Ctx&) { game_count++; },
                    0.f, pm::RunOn::All, true); // pauseable

        pm.run(); // normal frame
        assert(always_count == 1 && game_count == 1);

        pm.pause();
        assert(pm.is_paused());
        pm.run(); // paused — only non-pauseable runs
        assert(always_count == 2 && game_count == 1);
        pm.run(); // still paused
        assert(always_count == 3 && game_count == 1);

        pm.request_step(); // request: next run() includes pauseable tasks
        assert(!pm.stepping()); // not stepping yet
        pm.run(); // this frame is a step frame
        assert(always_count == 4 && game_count == 2);
        assert(!pm.stepping()); // cleared after run()
        assert(pm.is_paused()); // still paused after step

        pm.run(); // back to paused
        assert(always_count == 5 && game_count == 2);

        pm.resume();
        pm.run(); // normal again
        assert(always_count == 6 && game_count == 3);

        // Verify stepping() is visible inside tasks during step frame
        bool saw_stepping = false;
        pm.schedule("spy", pm::Pm::Phase::SIMULATE + 1.f, [&](pm::Ctx& p) { saw_stepping = p.stepping(); },
                    0.f, pm::RunOn::All, true);

        pm.request_step();
        pm.run();
        assert(saw_stepping); // task observed stepping=true during the step frame
        saw_stepping = false;
        pm.run(); // normal paused frame — spy doesn't run (pauseable)
        assert(!saw_stepping);

        // toggle_pause
        pm.toggle_pause();
        assert(pm.is_paused());
        pm.toggle_pause();
        assert(!pm.is_paused());

        printf("  [OK] Pause/step skips pauseable tasks\n");
    }

    // --- Remove budget rate-limits by time ---
    {
        pm::Pm pm;
        auto* pool = pm.pool<int>("stuff");

        // Spawn 10 entities
        pm::Id ids[10];
        for (int i = 0; i < 10; i++) {
            ids[i] = pm.spawn();
            pool->add(ids[i], i);
        }
        assert(pool->items.size() == 10);

        // Budget=0 (unlimited) — all removed in one frame
        for (int i = 0; i < 5; i++) pm.remove(ids[i]);
        assert(pm.remove_pending() == 5);
        pm.run();
        assert(pool->items.size() == 5);
        assert(pm.remove_pending() == 0);

        // Tiny budget with many entities — should spread across frames
        // Clock checked every 16 removes, so need >16 to observe throttling
        pm::Id batch[200];
        for (int i = 0; i < 200; i++) {
            batch[i] = pm.spawn();
            pool->add(batch[i], i);
        }
        for (int i = 0; i < 200; i++) pm.remove(batch[i]);
        assert(pm.remove_pending() == 200);
        pm.set_remove_budget_us(0.001f); // 1 nanosecond — absurdly small

        // Run until all removed (must converge — at least 16 per frame)
        size_t before = pool->items.size(); // 200 + 5 leftover from first batch
        int safety = 0;
        while (pm.remove_pending() > 0 && safety < 100) {
            pm.run();
            safety++;
        }
        assert(pool->items.size() == before - 200); // only the 200 removed
        assert(pm.remove_pending() == 0);
        assert(safety >= 1);

        // Reset to unlimited — verify it works
        pm.set_remove_budget_us(0.f);
        size_t base = pool->items.size();
        pm::Id a = pm.spawn(); pool->add(a, 99);
        pm::Id b = pm.spawn(); pool->add(b, 100);
        pm.remove(a); pm.remove(b);
        pm.run();
        assert(pool->items.size() == base); // back to where we were

        printf("  [OK] Remove budget rate-limits by time\n");
    }

    // --- PeerRange iteration ---
    {
        pm::Pm pm;
        uint8_t p1 = pm.connect();
        uint8_t p2 = pm.connect();
        uint8_t p3 = pm.connect();

        // peers() includes self (host = 0)
        std::vector<uint8_t> all;
        for (uint8_t p : pm.peers()) all.push_back(p);
        assert(all.size() == 4);
        assert(all[0] == 0); // host
        assert(all[1] == p1);
        assert(all[2] == p2);
        assert(all[3] == p3);

        // remote_peers() excludes self
        std::vector<uint8_t> remotes;
        for (uint8_t p : pm.remote_peers()) remotes.push_back(p);
        assert(remotes.size() == 3);
        assert(remotes[0] == p1);
        assert(remotes[1] == p2);
        assert(remotes[2] == p3);

        // count/empty/has
        assert(pm.peers().count() == 4);
        assert(pm.remote_peers().count() == 3);
        assert(!pm.peers().empty());
        assert(pm.peers().has(0));
        assert(pm.peers().has(p2));
        assert(!pm.peers().has(55));

        // After disconnect
        pm.disconnect(p2);
        remotes.clear();
        for (uint8_t p : pm.remote_peers()) remotes.push_back(p);
        assert(remotes.size() == 2);
        assert(remotes[0] == p1);
        assert(remotes[1] == p3);

        // Client perspective: self is not 0
        pm::Pm client_pm;
        client_pm.set_peer_id(3);
        uint8_t server = client_pm.connect(); // gets slot 0 (first non-self)
        assert(server == 0);
        assert(client_pm.peers().has(0));
        assert(client_pm.peers().has(3));
        assert(client_pm.peers().count() == 2);
        assert(client_pm.remote_peers().count() == 1);
        std::vector<uint8_t> client_remotes;
        for (uint8_t p : client_pm.remote_peers()) client_remotes.push_back(p);
        assert(client_remotes.size() == 1);
        assert(client_remotes[0] == 0); // server is the remote

        printf("  [OK] PeerRange iteration\n");
    }

    // --- Peer metadata ---
    {
        pm::Pm pm;
        // Host peer is set up at construction
        assert(pm.peer(0).id == 0);
        assert(pm.peer(0).connected == true);
        assert(pm.peer(0).connected_tick == 0);

        pm.run(); pm.run(); // tick a couple frames
        uint8_t p1 = pm.connect();
        assert(pm.peer(p1).id == p1);
        assert(pm.peer(p1).connected == true);
        assert(pm.peer(p1).connected_tick == pm.tick_count());

        // user_data
        int my_data = 42;
        pm.peer(p1).user_data = &my_data;
        assert(*(int*)pm.peer(p1).user_data == 42);

        // Disconnect clears metadata
        pm.disconnect(p1);
        assert(pm.peer(p1).connected == false);
        assert(pm.peer(p1).id == 255);
        assert(pm.peer(p1).user_data == nullptr);

        printf("  [OK] Peer metadata\n");
    }

    // --- Peer lifecycle hooks ---
    {
        pm::Pm pm;
        std::vector<std::pair<char, uint8_t>> events;

        pm.on_connect([&](pm::Pm&, uint8_t id) { events.push_back({'C', id}); });
        pm.on_disconnect([&](pm::Pm&, uint8_t id) { events.push_back({'D', id}); });

        uint8_t p1 = pm.connect();
        uint8_t p2 = pm.connect();
        assert(events.size() == 2);
        assert(events[0] == std::make_pair('C', p1));
        assert(events[1] == std::make_pair('C', p2));

        pm.disconnect(p1);
        assert(events.size() == 3);
        assert(events[2] == std::make_pair('D', p1));

        // Disconnecting already-disconnected does nothing
        pm.disconnect(p1);
        assert(events.size() == 3);

        // Can't disconnect self
        pm.disconnect(0);
        assert(events.size() == 3);

        printf("  [OK] Peer lifecycle hooks\n");
    }

    // --- set_peer_id manages self bit ---
    {
        pm::Pm pm;
        assert(pm.peer_slots() & 1); // host at bit 0

        // Simulate client: set to 255 (unassigned)
        pm.set_peer_id(255);
        assert(pm.peer_slots() == 0); // no self bit (255 >= 64)
        assert(pm.peer_id() == 255);

        // Server assigns us slot 3
        pm.set_peer_id(3);
        assert(pm.peer_slots() == (1ULL << 3));
        assert(pm.peer_id() == 3);
        assert(pm.peer(3).connected == true);

        // remote_peers excludes self at slot 3
        uint8_t s = pm.connect(); // server at slot 0
        assert(pm.remote_peers().count() == 1);
        bool found_self = false;
        for (uint8_t p : pm.remote_peers())
            if (p == 3) found_self = true;
        assert(!found_self);
        (void)s;

        printf("  [OK] set_peer_id manages self bit\n");
    }

    // --- Ctx peers() in task ---
    {
        pm::Pm pm;
        uint8_t p1 = pm.connect();
        uint8_t p2 = pm.connect();

        uint8_t ctx_remote_count = 0;
        bool ctx_has_self = false;
        pm.schedule("test/ctx_peers", 50.f, [&](pm::Ctx& ctx) {
            ctx_remote_count = 0;
            for (uint8_t p : ctx.remote_peers()) {
                ctx_remote_count++;
                (void)p;
            }
            ctx_has_self = ctx.peers().has(ctx.peer_id());
        });
        pm.run();
        assert(ctx_remote_count == 2);
        assert(ctx_has_self);
        (void)p1; (void)p2;

        printf("  [OK] Ctx peers() in task\n");
    }

    // --- Interest management: is_synced_to / unsync_for ---
    {
        pm::Pm pm;
        auto* pool = pm.pool<int>("items");
        uint8_t p1 = pm.connect();
        uint8_t p2 = pm.connect();

        pm::Id a = pm.spawn(); pool->add(a, 10);
        pm::Id b = pm.spawn(); pool->add(b, 20);
        pm::Id c = pm.spawn(); pool->add(c, 30);

        // Initially nothing is synced
        size_t da = pool->sparse_indices[pm::id_index(a)];
        size_t db = pool->sparse_indices[pm::id_index(b)];
        size_t dc = pool->sparse_indices[pm::id_index(c)];
        assert(!pool->is_synced_to(p1, da));
        assert(!pool->is_synced_to(p2, da));

        // Sync a and b to p1
        pool->sync(p1, da);
        pool->sync(p1, db);
        assert(pool->is_synced_to(p1, da));
        assert(pool->is_synced_to(p1, db));
        assert(!pool->is_synced_to(p1, dc));
        assert(!pool->is_synced_to(p2, da)); // p2 still unsynced

        // unsync_for: revoke a from p1 (simulate leaving interest)
        pool->unsync_for(p1, da);
        assert(!pool->is_synced_to(p1, da)); // cleared
        assert(pool->is_synced_to(p1, db));  // b untouched

        // a should be back in pending list — iterate unsynced for p1
        std::vector<pm::Id> unsynced;
        pool->each_unsynced(p1, [&](pm::Id id, int&, size_t) {
            unsynced.push_back(id);
        });
        // a and c were never synced to p1 (a was unsynced, c never was)
        assert(std::find(unsynced.begin(), unsynced.end(), a) != unsynced.end());
        assert(std::find(unsynced.begin(), unsynced.end(), c) != unsynced.end());
        // b IS synced to p1, should not appear
        assert(std::find(unsynced.begin(), unsynced.end(), b) == unsynced.end());

        printf("  [OK] Interest: is_synced_to / unsync_for\n");
    }

    // --- Interest filtering pattern (simulated) ---
    {
        pm::Pm pm;
        auto* pool = pm.pool<float>("positions"); // float = x position
        uint8_t p1 = pm.connect();

        // Create entities at various positions
        pm::Id near1 = pm.spawn(); pool->add(near1, 50.f);   // close
        pm::Id near2 = pm.spawn(); pool->add(near2, 100.f);  // close
        pm::Id far1  = pm.spawn(); pool->add(far1, 900.f);   // far
        pm::Id far2  = pm.spawn(); pool->add(far2, 1500.f);  // very far

        float interest_radius = 200.f;
        float peer_pos = 80.f; // peer is at x=80

        // First pass: sync only in-interest entities
        std::vector<pm::Id> synced_ids;
        pool->each_unsynced(p1, [&](pm::Id id, float& x, size_t di) {
            if (std::abs(x - peer_pos) <= interest_radius) {
                synced_ids.push_back(id);
                pool->sync(p1, di); // mark synced
            }
            // else: skip, stays unsynced
        });
        assert(synced_ids.size() == 2); // near1, near2
        assert(std::find(synced_ids.begin(), synced_ids.end(), near1) != synced_ids.end());
        assert(std::find(synced_ids.begin(), synced_ids.end(), near2) != synced_ids.end());

        // Verify far entities still unsynced
        size_t dfar1 = pool->sparse_indices[pm::id_index(far1)];
        assert(!pool->is_synced_to(p1, dfar1));

        // Simulate entity leaving interest: near2 moves far
        size_t dnear2 = pool->sparse_indices[pm::id_index(near2)];
        pool->items[dnear2] = 800.f; // moved to x=800

        // Reconciliation: check all synced entities for interest
        for (size_t di = 0; di < pool->items.size(); di++) {
            if (!pool->is_synced_to(p1, di)) continue;
            if (std::abs(pool->items[di] - peer_pos) > interest_radius) {
                pool->unsync_for(p1, di); // left interest
            }
        }

        // near2 should now be unsynced
        dnear2 = pool->sparse_indices[pm::id_index(near2)]; // may have changed after swap
        assert(!pool->is_synced_to(p1, dnear2));

        // near1 should still be synced
        size_t dnear1 = pool->sparse_indices[pm::id_index(near1)];
        assert(pool->is_synced_to(p1, dnear1));

        // Second pass: near2 shows up as unsynced again (along with far entities)
        synced_ids.clear();
        pool->each_unsynced(p1, [&](pm::Id id, float& x, size_t di) {
            if (std::abs(x - peer_pos) <= interest_radius) {
                synced_ids.push_back(id);
                pool->sync(p1, di);
            }
        });
        // Only far1, far2, near2 are unsynced — but none are within interest now
        assert(synced_ids.empty());

        // Move near2 back into range
        dnear2 = pool->sparse_indices[pm::id_index(near2)];
        pool->items[dnear2] = 90.f;
        pool->unsync(near2); // entity moved, needs re-check

        synced_ids.clear();
        pool->each_unsynced(p1, [&](pm::Id id, float& x, size_t di) {
            if (std::abs(x - peer_pos) <= interest_radius) {
                synced_ids.push_back(id);
                pool->sync(p1, di);
            }
        });
        assert(synced_ids.size() == 1);
        assert(synced_ids[0] == near2); // re-entered interest

        printf("  [OK] Interest: spatial filtering pattern\n");
    }

    // --- Interest hysteresis ---
    {
        pm::Pm pm;
        auto* pool = pm.pool<float>("pos");
        uint8_t p1 = pm.connect();

        float radius = 100.f;
        float hysteresis = 0.3f; // leave at 130
        float peer_pos = 0.f;

        pm::Id a = pm.spawn(); pool->add(a, 90.f);  // inside enter radius
        pm::Id b = pm.spawn(); pool->add(b, 110.f); // outside enter, inside leave
        pm::Id c = pm.spawn(); pool->add(c, 140.f); // outside both

        auto enter_check = [&](pm::Id, float& x) -> bool {
            return std::abs(x - peer_pos) <= radius;
        };
        auto leave_check = [&](pm::Id, float& x) -> bool {
            return std::abs(x - peer_pos) <= radius * (1.f + hysteresis);
        };

        // First sync pass: only 'a' enters (within enter radius)
        std::vector<pm::Id> entered;
        pool->each_unsynced(p1, [&](pm::Id id, float& x, size_t di) {
            if (enter_check(id, x)) {
                entered.push_back(id);
                pool->sync(p1, di);
            }
        });
        assert(entered.size() == 1);
        assert(entered[0] == a);

        // Now move 'a' to 115 — outside enter radius, but inside leave radius
        size_t da = pool->sparse_indices[pm::id_index(a)];
        pool->items[da] = 115.f;

        // Reconciliation with leave check: 'a' should STAY synced (within leave radius)
        bool a_left = false;
        for (size_t di = 0; di < pool->items.size(); di++) {
            if (!pool->is_synced_to(p1, di)) continue;
            if (!leave_check(pool->dense_ids[di], pool->items[di])) {
                pool->unsync_for(p1, di);
                a_left = true;
            }
        }
        assert(!a_left); // hysteresis kept it synced!
        assert(pool->is_synced_to(p1, pool->sparse_indices[pm::id_index(a)]));

        // Move 'a' to 135 — outside BOTH radii
        da = pool->sparse_indices[pm::id_index(a)];
        pool->items[da] = 135.f;

        // NOW reconciliation drops it
        for (size_t di = 0; di < pool->items.size(); di++) {
            if (!pool->is_synced_to(p1, di)) continue;
            if (!leave_check(pool->dense_ids[di], pool->items[di])) {
                pool->unsync_for(p1, di);
                a_left = true;
            }
        }
        assert(a_left);
        assert(!pool->is_synced_to(p1, pool->sparse_indices[pm::id_index(a)]));

        printf("  [OK] Interest: hysteresis prevents churn\n");
    }

    printf("\n=== All tests passed ===\n");
    return 0;
}