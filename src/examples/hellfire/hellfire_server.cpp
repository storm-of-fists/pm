// hellfire_server.cpp — Dedicated server for hellfire
// Build: g++ -std=c++17 -O3 -o hellfire_server hellfire_server.cpp
// Run:   ./hellfire_server [port]

#include "pm_core.hpp"
#include "pm_math.hpp"
#include "pm_udp.hpp"
#include "pm_spatial_grid.hpp"
#include "hellfire_common.hpp"
#include <cstdio>
#include <cstring>
#include <algorithm>

using namespace pm;

// =============================================================================
// Server state — all game data
// =============================================================================
struct ServerState {
    bool game_over = false, win = false;
    float time = 0, spawn_accum = 0;
    int score = 0, kills = 0;
    int current_level = 0;
    float level_flash = 0.f, level_hold = 0.f;
    uint16_t round = 0;
    Rng rng{42};
    bool started = false;

    // Roster
    PlayerInfo roster[4]{};
    int roster_count = 0;

    // Input from each client
    Input axes[4]{};

    // Player entities
    Pool<Player>* players = nullptr;
    Id peer_ids[4] = {NULL_ID, NULL_ID, NULL_ID, NULL_ID};

    // Spatial grid for monster broad-phase — rebuilt each collision tick.
    // Cell size 64px; at max load (400 monsters, 900×700 world) averages ~2–3
    // monsters per cell so each bullet query touches only a handful of entities.
    SpatialGrid monster_grid{W, H, 64};

    void add_player(Pm& pm, uint8_t peer) {
        if (peer >= 4 || peer_ids[peer] != NULL_ID) return;
        peer_ids[peer] = pm.spawn();
        players->add(peer_ids[peer], Player{{SPAWN_X[peer], SPAWN_Y[peer]}, PLAYER_HP, 0, 0, true,
                  PCOL[peer][0], PCOL[peer][1], PCOL[peer][2]});
    }

    void set_roster_name(uint8_t peer, const char* name) {
        for (int i = 0; i < roster_count; i++) {
            if (roster[i].peer_id == peer) {
                snprintf(roster[i].name, MAX_NAME + 1, "%s", name);
                return;
            }
        }
        if (roster_count < 4) {
            roster[roster_count].peer_id = peer;
            roster[roster_count].connected = true;
            snprintf(roster[roster_count].name, MAX_NAME + 1, "%s", name);
            roster_count++;
        }
    }

    void reset_game(Pm& pm, NetSys* net, Pool<Monster>* mp, Pool<Bullet>* bp) {
        game_over = false; win = false; time = 0; score = 0; kills = 0;
        spawn_accum = 0; current_level = 0; level_flash = 0.f; level_hold = 0.f; rng = Rng{42};
        round++;
        net->clear_pool(pm, mp);
        net->clear_pool(pm, bp);
        for (int i = 0; i < 4; i++) {
            auto* p = players->get(peer_ids[i]);
            if (p) { p->hp = PLAYER_HP; p->alive = true; p->pos = {SPAWN_X[i], SPAWN_Y[i]}; p->cooldown = 0; p->invuln = 0; }
        }
    }
};

// =============================================================================
// Monster spawning helper
// =============================================================================
static void spawn_monster(Pm& pm, Pool<Monster>* pool, ServerState* gs) {
    Monster m;
    const LevelDef& lvl = LEVELS[gs->current_level];
    float intensity = (sinf(gs->time * 0.4f) * 0.5f + 0.5f) * 0.6f
                    + (sinf(gs->time * 0.08f) * 0.5f + 0.5f) * 0.4f;
    float speed = MONSTER_SPEED * (0.8f + intensity * 0.6f) * std::min(lvl.speed_mult * lvl.size_mult, 3.f);
    m.size = gs->rng.rfr(MONSTER_MIN_SZ * lvl.size_mult, MONSTER_MAX_SZ * lvl.size_mult);
    switch (gs->rng.next() % 4) {
        case 0: m.pos = {gs->rng.rfr(0, W), -30}; break;
        case 1: m.pos = {gs->rng.rfr(0, W), H+30}; break;
        case 2: m.pos = {-30, gs->rng.rfr(0, H)}; break;
        case 3: m.pos = {W+30, gs->rng.rfr(0, H)}; break;
    }
    Vec2 tgt = {W*0.5f + gs->rng.rfr(-200, 200), H*0.5f + gs->rng.rfr(-200, 200)};
    m.vel = norm(tgt - m.pos) * speed;
    m.shoot_timer = gs->rng.rfr(1.5f, 4.f);
    float hue = gs->rng.rf();
    if      (hue < 0.3f) { m.r = 255; m.g = 80;  m.b = 60;  }
    else if (hue < 0.5f) { m.r = 255; m.g = 140; m.b = 40;  }
    else if (hue < 0.7f) { m.r = 255; m.g = 60;  m.b = 120; }
    else if (hue < 0.85f){ m.r = 200; m.g = 50;  m.b = 200; }
    else                 { m.r = 255; m.g = 200; m.b = 50;  }
    pool->add(pm.spawn(), m);
}

// =============================================================================
// server_init — register all server tasks
// =============================================================================
void server_init(Pm& pm) {
    auto* gs  = pm.state<ServerState>("server");
    gs->players = pm.pool<Player>("players");
    auto* mp  = pm.pool<Monster>("monsters");
    auto* bp  = pm.pool<Bullet>("bullets");
    auto* net = pm.state<NetSys>("net");

    // --- Sync bindings (send only — server never receives pool sync) ---
    // Monsters + bullets move every frame → full-sync mode (dense iteration, no change hook)
    net->bind_send(pm, mp, "monster_sync", Phase::NET_SEND, write_monster,
        [gs](Pm&, uint8_t peer, Id, const Monster& m, float margin) -> bool {
            if (peer >= 4) return true;
            auto* p = gs->players->get(gs->peer_ids[peer]);
            if (!p || !p->alive) return true;
            return dist(m.pos, p->pos) <= INTEREST_RADIUS * (1.f + margin);
        }, 0.3f);

    net->bind_send(pm, bp, "bullet_sync", Phase::NET_SEND, write_bullet);

    // --- Connection handshake ---
    net->protocol_version = 1;
    net->connect_validator = [gs](uint8_t peer_id, struct sockaddr_in&,
                                  const uint8_t* payload, uint16_t size) -> NetSys::ConnectResult {
        if (gs->roster_count >= 4)
            return NetSys::ConnectResult::deny(DENY_SERVER_FULL);
        // Payload is a PktJoin (name only, no type byte needed)
        char name[MAX_NAME + 1] = {};
        if (size > 0) {
            memcpy(name, payload, std::min((int)size, MAX_NAME));
            name[MAX_NAME] = 0;
        }
        gs->set_roster_name(peer_id, name[0] ? name : "peer");
        printf("[server] peer %d joined: %s\n", peer_id, name[0] ? name : "peer");
        // ACK payload: roster so the new client gets current state
        PktRoster roster_pkt;
        build_roster(gs->roster, gs->roster_count, roster_pkt);
        return NetSys::ConnectResult::accept(&roster_pkt, sizeof(roster_pkt));
    };

    // Broadcast updated roster to all existing peers after each connect
    net->on_connect([gs, net](NetSys&, uint8_t) {
        PktRoster roster_pkt;
        build_roster(gs->roster, gs->roster_count, roster_pkt);
        net->broadcast(net->remote_peers(), &roster_pkt, sizeof(roster_pkt));
    });

    // --- Packet handlers ---
    net->on_recv(PKT_INPUT, [gs, net](Pm&, const uint8_t* buf, int n, struct sockaddr_in&) {
        if (n < (int)sizeof(PktInput)) return;
        PktInput p; memcpy(&p, buf, sizeof(p));
        if (p.peer < 4) gs->axes[p.peer] = {p.dx, p.dy, p.ax, p.ay, p.shooting != 0};
    });

    net->on_recv(PKT_START, [gs, net](Pm& pm, const uint8_t* /*buf*/, int, struct sockaddr_in&) {
        if (gs->started) return;
        gs->started = true;
        for (int i = 0; i < gs->roster_count; i++) {
            uint8_t p = gs->roster[i].peer_id;
            if (p < 4) gs->add_player(pm, p);
        }
        PktStart start{PKT_START};
        net->broadcast(net->remote_peers(), &start, sizeof(start));
        printf("[server] game started with %d players\n", gs->roster_count);
    });

    net->on_recv(PKT_PAUSE, [](Pm& pm, const uint8_t*, int, struct sockaddr_in&) {
        pm.toggle_pause();
    });

    net->on_recv(PKT_RESTART, [gs, net, mp, bp](Pm& pm, const uint8_t*, int, struct sockaddr_in&) {
        if (!gs->game_over) return;
        gs->reset_game(pm, net, mp, bp);
        pm.resume();
        printf("[server] game restarted\n");
    });

    // --- Player movement + shooting ---
    pm.schedule("player_move", Phase::SIMULATE - 1.f, [gs, bp](Pm& pm) {
        if (!gs->started || gs->game_over) return;
        float dt = pm.dt();
        for (int pi = 0; pi < 4; pi++) {
            auto* p = gs->players->get(gs->peer_ids[pi]);
            if (!p || !p->alive) continue;
            auto& in = gs->axes[pi];
            Vec2 move = {in.dx, in.dy};
            float ml = len(move);
            if (ml > 0.001f) move = move * (1.f / ml);
            p->pos += move * (PLAYER_SPEED * dt);
            float hs = PLAYER_SIZE * 0.5f;
            p->pos.x = std::clamp(p->pos.x, hs, (float)W - hs);
            p->pos.y = std::clamp(p->pos.y, hs, (float)H - hs);
            if (p->cooldown > 0) p->cooldown -= dt;
            if (p->invuln > 0) p->invuln -= dt;
            if (in.shooting && p->cooldown <= 0) {
                p->cooldown = PLAYER_COOLDOWN;
                Vec2 aim = norm(Vec2{in.ax, in.ay} - p->pos);
                if (len(aim) < 0.001f) aim = {1, 0};
                bp->add(pm.spawn(), Bullet{p->pos, aim*PBULLET_SPEED, PBULLET_LIFE, PBULLET_SIZE, true});
            }
        }
    }, true);

    // --- Spawning ---
    pm.schedule("spawn", Phase::SIMULATE - 2.f, [gs, net, mp, bp](Pm& pm) {
        if (!gs->started || gs->game_over) return;
        float dt = pm.dt();
        gs->time += dt;
        const LevelDef& lvl = LEVELS[gs->current_level];
        if (gs->level_hold > 0.f) { gs->level_hold -= dt; return; }
        int next = gs->current_level + 1;
        if (next < NUM_LEVELS && gs->score >= LEVELS[next].score_threshold) {
            gs->current_level = next;
            gs->round++;
            gs->spawn_accum = 0.f; gs->level_flash = 3.0f; gs->level_hold = 3.0f;
            for (int i = 0; i < 4; i++) {
                auto* p = gs->players->get(gs->peer_ids[i]);
                if (p && p->alive) {
                    p->pos.x = SPAWN_X[i] + gs->rng.rfr(-80, 80);
                    p->pos.y = SPAWN_Y[i] + gs->rng.rfr(-60, 60);
                    p->invuln = 2.0f;
                }
            }
            net->clear_pool(pm, mp);
            net->clear_pool(pm, bp);
            printf("[server] level %d\n", gs->current_level + 1);
            return;
        }
        if (gs->score >= WIN_SCORE && !gs->win) {
            gs->win = true; gs->game_over = true;
            net->clear_pool(pm, mp);
            net->clear_pool(pm, bp);
            return;
        }
        float intensity = (sinf(gs->time * 0.4f) * 0.5f + 0.5f) * 0.6f
                        + (sinf(gs->time * 0.08f) * 0.5f + 0.5f) * 0.4f;
        if ((int)mp->items.size() >= lvl.max_monsters) return;
        gs->spawn_accum += (1.f + 8.f * intensity) * lvl.spawn_mult * dt;
        while (gs->spawn_accum >= 1.f && (int)mp->items.size() < lvl.max_monsters) {
            gs->spawn_accum -= 1.f;
            spawn_monster(pm, mp, gs);
        }
    }, true);

    // --- Monster AI ---
    pm.schedule("monster_ai", Phase::SIMULATE + 1.f, [gs, mp, bp](Pm& pm) {
        if (!gs->started || gs->game_over) return;
        float dt = pm.dt();
        mp->each_mut([&](Monster& m) {
            Vec2 tgt = m.pos; float best = 1e9f;
            for (auto& p : gs->players->items)
                if (p.alive && dist(m.pos, p.pos) < best) { best = dist(m.pos, p.pos); tgt = p.pos; }
            Vec2 desired = norm(tgt - m.pos) * len(m.vel);
            m.vel.x += (desired.x - m.vel.x) * 0.5f * dt;
            m.vel.y += (desired.y - m.vel.y) * 0.5f * dt;
            m.shoot_timer -= dt;
            if (m.shoot_timer <= 0 && best < 500.f) {
                m.shoot_timer = gs->rng.rfr(2.f, 5.f);
                Vec2 dir = norm(tgt - m.pos);
                float sp = gs->rng.rfr(-0.15f, 0.15f);
                float cs = cosf(sp), sn = sinf(sp);
                Vec2 aim = {dir.x*cs - dir.y*sn, dir.x*sn + dir.y*cs};
                bp->add(pm.spawn(), Bullet{m.pos, aim*MBULLET_SPEED, MBULLET_LIFE, MBULLET_SIZE, false});
            }
            m.pos += m.vel * dt;
        }, Parallel::Off);
    }, true);

    // --- Bullet physics ---
    pm.schedule("bullet_physics", Phase::SIMULATE, [gs, net, bp](Pm& pm) {
        if (!gs->started || gs->game_over) return;
        float dt = pm.dt();
        bp->each_mut([&](Id id, Bullet& b) {
            b.pos += b.vel * dt;
            b.lifetime -= dt;
            if (b.lifetime <= 0) { net->tracked_remove(pm, bp, id); }
        }, Parallel::Off);
    }, true);

    // --- Collision ---
    // Conservative query radius: max possible monster collision threshold.
    // Bullet-vs-monster exact check is done inside the query callback.
    static constexpr float MON_QUERY_R = PBULLET_SIZE + MONSTER_MAX_SZ * 0.65f;

    pm.schedule("collision", Phase::COLLIDE, [gs, mp, bp, net](Pm& pm) {
        if (!gs->started || gs->game_over) return;
        float pr = PLAYER_SIZE * 0.5f;

        // Build monster grid for this frame (O(monsters)).
        gs->monster_grid.clear();
        mp->each([&](Id mid, const Monster& m) {
            gs->monster_grid.insert(mid, m.pos);
        }, Parallel::Off);

        // Player bullets vs monsters — O(bullets × few) instead of O(b × m).
        bp->each([&](Id bid, const Bullet& b) {
            if (!b.player_owned) return;
            bool hit = false;
            gs->monster_grid.query(b.pos, MON_QUERY_R, [&](Id mid, Vec2) {
                if (hit) return;
                const Monster* m = mp->get(mid);
                if (!m || dist(b.pos, m->pos) >= b.size + m->size * 0.5f) return;
                net->tracked_remove(pm, mp, mid);
                net->tracked_remove(pm, bp, bid);
                gs->score += 10; gs->kills++;
                hit = true;
            });
        }, Parallel::Off);

        // Players vs enemy bullets and monster contact — O(p × b/m), already cheap.
        for (int i = 0; i < 4; i++) {
            auto* p = gs->players->get(gs->peer_ids[i]);
            if (!p || !p->alive || p->invuln > 0) continue;
            bp->each([&](Id bid, const Bullet& b) {
                if (!b.player_owned && dist(b.pos, p->pos) < b.size + pr) {
                    p->hp -= BULLET_DMG; p->invuln = PLAYER_INVULN;
                    net->tracked_remove(pm, bp, bid);
                }
            }, Parallel::Off);
            mp->each([&](const Monster& m) {
                if (dist(m.pos, p->pos) < m.size*0.5f + pr) { p->hp -= CONTACT_DMG; p->invuln = PLAYER_INVULN*0.5f; }
            }, Parallel::Off);
            if (p->hp <= 0) { p->hp = 0; p->alive = false; }
        }

        bool any = false;
        for (auto& p : gs->players->items) if (p.alive) any = true;
        if (!any && !gs->players->items.empty()) {
            gs->game_over = true;
            net->clear_pool(pm, mp);
            net->clear_pool(pm, bp);
        }
    }, true);

    // --- Cleanup OOB ---
    pm.schedule("cleanup", Phase::CLEANUP, [gs, mp, bp, net](Pm& pm) {
        if (!gs->started) return;
        mp->each([&](Id id, const Monster& m) {
            if (m.pos.x<-100||m.pos.x>W+100||m.pos.y<-100||m.pos.y>H+100) { net->tracked_remove(pm, mp, id); }
        }, Parallel::Off);
        bp->each([&](Id id, const Bullet& b) {
            if (b.pos.x<-50||b.pos.x>W+50||b.pos.y<-50||b.pos.y>H+50) { net->tracked_remove(pm, bp, id); }
        }, Parallel::Off);
    }, true);

    // --- Broadcast game state via state sync ---
    net->bind_state_send(pm, STATE_ID_GAME, "state_send", Phase::NET_SEND, [gs, net](Pm& pm, uint8_t* buf) -> uint16_t {
        if (!gs->started) return 0;
        PktState pkt{PKT_STATE, net->net_frame, gs->time, gs->score, gs->kills, (uint8_t)pm.is_paused(), gs->game_over, 0, gs->round, {}};
        for (int i = 0; i < 4; i++) {
            auto* p = gs->players->get(gs->peer_ids[i]);
            if (p) { pkt.p[pkt.pcnt++] = {p->pos.x, p->pos.y, p->hp, (uint8_t)p->alive}; }
        }
        memcpy(buf, &pkt, sizeof(pkt));
        return sizeof(pkt);
    });

    // --- Debug info broadcast — once per second to all connected clients ---
    pm.schedule("debug_send", Phase::HUD, [mp, bp, net](Pm& pm) {
        static float timer = 0.f;
        timer += pm.dt();
        if (timer < 1.f) return;
        timer -= 1.f;
        PktDbg pkt{};
        pkt.monsters    = (uint16_t)mp->items.size();
        pkt.bullets     = (uint16_t)bp->items.size();
        pkt.ms_per_tick = pm.dt() * 1000.f;
        for (uint8_t p : net->remote_peers())
            net->send_to(p, &pkt, sizeof(pkt));
    });

    // --- Status print ---
    pm.schedule("status", Phase::HUD, [gs, mp, bp](Pm& pm) {
        static float accum = 0;
        accum += pm.dt();
        if (accum < 5.f) return;
        accum = 0;
        if (gs->started) {
            printf("[server] t=%.0f  score=%d  lvl=%d  m=%zu  b=%zu  players=%zu\n",
                   gs->time, gs->score, gs->current_level+1, mp->items.size(), bp->items.size(), gs->players->items.size());
        } else {
            printf("[server] lobby — %d player(s) waiting\n", gs->roster_count);
        }
    });
}

// =============================================================================
// Main
// =============================================================================
int main(int argc, char** argv) {
    int port = NET_PORT;
    if (argc > 1) port = atoi(argv[1]);

    printf("[server] starting on port %d\n", port);

    Pm pm;
    pm.set_loop_rate(60);

    auto* net = pm.state<NetSys>("net");
    net->set_dedicated();
    net->port = port;
    net->start();
    net_init(pm, net, Phase::NET_RECV, Phase::NET_SEND);

    server_init(pm);

    pm.run();
    return 0;
}