// hellfire.cpp — Bullet Hell with pm kernel + SDL2 + UDP co-op
// Build: g++ -std=c++17 -O3 -o hellfire hellfire.cpp $(sdl2-config --cflags --libs)
// Run:   ./hellfire

#include "pm_core.hpp"
#include "pm_math.hpp"
#include "pm_net.hpp"
#include "pm_sync.hpp"
#include "pm_sdl.hpp"
#include "pm_stats.hpp"
#include "pm_debug.hpp"
#include <cstdio>
#include <cstring>
#include <string>
#include <algorithm>

using namespace pm;

// =============================================================================
// Constants
// =============================================================================
static constexpr int W = 900, H = 700;
static constexpr float PLAYER_SPEED = 280, PLAYER_SIZE = 14, PLAYER_HP = 100;
static constexpr float PLAYER_COOLDOWN = 0.12f, PLAYER_INVULN = 0.4f;
static constexpr float PBULLET_SPEED = 750, PBULLET_SIZE = 4, PBULLET_LIFE = 1.5f;
static constexpr float MBULLET_SPEED = 220, MBULLET_SIZE = 5, MBULLET_LIFE = 4.0f;
static constexpr float CONTACT_DMG = 1, BULLET_DMG = 3;
static constexpr float MONSTER_MIN_SZ = 8, MONSTER_MAX_SZ = 16, MONSTER_SPEED = 60;
static constexpr int MAX_MONSTERS = 400, MAX_BULLETS = 600, NET_PORT = 9998;
static constexpr float INTEREST_RADIUS = 1200.f;
static constexpr int MAX_NAME = 12;
static constexpr int MAX_IP = 15;

static const uint8_t PCOL[4][3] = {{0,220,255}, {0,255,120}, {255,160,40}, {255,80,200}};
static const float SPAWN_X[4] = {W*.25f, W*.75f, W*.25f, W*.75f};
static const float SPAWN_Y[4] = {H*.4f, H*.4f, H*.6f, H*.6f};

struct LevelDef {
    int score_threshold;
    float speed_mult, spawn_mult;
    int max_monsters;
    float size_mult;
    const char* name;
};
static constexpr int NUM_LEVELS = 5;
static const LevelDef LEVELS[NUM_LEVELS] = {
    {    0, 0.6f, 0.4f,  60, 0.8f, "level 1"},
    {  500, 0.8f, 0.7f, 120, 0.9f, "level 2"},
    { 1500, 1.0f, 1.2f, 200, 1.0f, "level 3"},
    { 3000, 1.3f, 2.0f, 300, 1.1f, "level 4"},
    { 5500, 1.6f, 3.0f, 400, 1.2f, "level 5"},
};
static constexpr int WIN_SCORE = 8000;

// =============================================================================
// Components
// =============================================================================
struct Monster { Vec2 pos, vel; float shoot_timer=0, size=0; uint8_t r=255, g=255, b=255; };
struct Bullet  { Vec2 pos, vel; float lifetime=0, size=0; bool player_owned=false; };
struct Player  { Vec2 pos; float hp=PLAYER_HP, cooldown=0, invuln=0; bool alive=true; uint8_t r=0, g=220, b=255; };

// =============================================================================
// Game phases + player roster
// =============================================================================
enum class Phase { MENU, LOBBY, PLAYING };

struct PlayerInfo {
    char name[MAX_NAME + 1] = {};
    uint8_t peer_id = 255;
    bool connected = false;
};

// =============================================================================
// Wire format
// =============================================================================
#pragma pack(push, 1)
enum PktType : uint8_t { PKT_INPUT=0, PKT_STATE=1, PKT_JOIN=2, PKT_WELCOME=3,
                         PKT_ROSTER=4, PKT_START=5 };

struct PktInput   { uint8_t type=PKT_INPUT; uint8_t peer; float dx,dy,ax,ay; uint8_t shooting; uint16_t ack_seq=0; };
struct PktJoin    { uint8_t type=PKT_JOIN; char name[MAX_NAME+1]={}; };
struct PktWelcome { uint8_t type=PKT_WELCOME; uint8_t peer_id, pcnt; };
struct PktState {
    uint8_t type=PKT_STATE; uint32_t frame; float time;
    int score, kills; uint8_t paused, gameover, pcnt;
    struct { float x,y,hp; uint8_t alive; } p[4];
};
struct PktRoster {
    uint8_t type=PKT_ROSTER; uint8_t count;
    struct Entry { uint8_t peer_id; char name[MAX_NAME+1]; } entries[4];
};
struct PktStart { uint8_t type=PKT_START; };

struct MonSync { Id id; int16_t x,y; uint8_t sz,r,g,b; };
struct BulSync { Id id; int16_t x,y; uint8_t sz,po; };
#pragma pack(pop)

struct Input { float dx=0, dy=0, ax=0, ay=0; bool shooting=false; };
struct GameState; struct BulletSys; struct PlayerSys; struct MonsterSys; struct HellfireNet;

// =============================================================================
// InputHub
// =============================================================================
struct InputHub : public System {
    Input axes[4]{};
    void initialize(Pm& pm) override {
        pm.schedule(tname("poll").c_str(), Pm::Phase::INPUT + 1.f, [this](Ctx& ctx) {
            const uint8_t* k = SDL_GetKeyboardState(nullptr);
            auto& in = axes[ctx.peer_id()];
            in.dx = in.dy = 0;
            if (k[SDL_SCANCODE_W] || k[SDL_SCANCODE_UP])    in.dy -= 1;
            if (k[SDL_SCANCODE_S] || k[SDL_SCANCODE_DOWN])  in.dy += 1;
            if (k[SDL_SCANCODE_A] || k[SDL_SCANCODE_LEFT])  in.dx -= 1;
            if (k[SDL_SCANCODE_D] || k[SDL_SCANCODE_RIGHT]) in.dx += 1;
            int mx, my; uint32_t mb = SDL_GetMouseState(&mx, &my);
            in.ax = (float)mx; in.ay = (float)my;
            in.shooting = (mb & SDL_BUTTON_LMASK) != 0;
        });
    }
};

// =============================================================================
// MenuSys — title screen, text input, lobby, player list overlay
// =============================================================================
struct MenuSys : public System {
    Phase phase = Phase::MENU;
    int menu_sel = 0;     // 0=host, 1=join
    int field = 0;        // 0=username, 1=ip (join only)
    std::string username;
    std::string ip_addr = "127.0.0.1";
    float cursor_blink = 0;
    bool show_players = false; // V key overlay in-game

    // Roster
    PlayerInfo roster[4]{};
    int roster_count = 0;

    void set_roster_name(uint8_t peer, const char* name) {
        for (int i = 0; i < roster_count; i++) {
            if (roster[i].peer_id == peer) {
                strncpy(roster[i].name, name, MAX_NAME);
                roster[i].name[MAX_NAME] = 0;
                return;
            }
        }
        if (roster_count < 4) {
            roster[roster_count].peer_id = peer;
            roster[roster_count].connected = true;
            strncpy(roster[roster_count].name, name, MAX_NAME);
            roster[roster_count].name[MAX_NAME] = 0;
            roster_count++;
        }
    }

    void build_roster_packet(PktRoster& pkt) const {
        pkt.type = PKT_ROSTER;
        pkt.count = (uint8_t)roster_count;
        for (int i = 0; i < roster_count && i < 4; i++) {
            pkt.entries[i].peer_id = roster[i].peer_id;
            memcpy(pkt.entries[i].name, roster[i].name, MAX_NAME + 1);
        }
    }

    void apply_roster_packet(const PktRoster& pkt) {
        roster_count = std::min((int)pkt.count, 4);
        for (int i = 0; i < roster_count; i++) {
            roster[i].peer_id = pkt.entries[i].peer_id;
            roster[i].connected = true;
            memcpy(roster[i].name, pkt.entries[i].name, MAX_NAME + 1);
        }
    }

    static void text_feed(std::string& str, int key, int max_len) {
        if (key == SDLK_BACKSPACE) { if (!str.empty()) str.pop_back(); return; }
        if ((int)str.size() >= max_len) return;
        if (key >= SDLK_a && key <= SDLK_z) { str += (char)key; return; }
        if (key >= SDLK_0 && key <= SDLK_9) { str += (char)key; return; }
        if (key == SDLK_PERIOD) { str += '.'; return; }
        if (key == SDLK_MINUS) { str += '-'; return; }
    }

    void initialize(Pm& pm) override {
        auto* net = pm.sys<NetSys>("net");
        auto* game = pm.sys<GameState>("game");
        auto* player_sys = pm.sys<PlayerSys>("player_sys");

        pm.schedule(tname("input").c_str(), Pm::Phase::INPUT + 2.f, [this, net, game, player_sys](Ctx& ctx) {
            cursor_blink += ctx.dt();
            auto keys = ctx.queue<int>("keys");

            if (phase == Phase::PLAYING) {
                for (auto key : *keys) {
                    if (key == SDLK_v) show_players = true;
                    if (key == -SDLK_v) show_players = false;
                }
                return;
            }

            for (auto key : *keys) {
                if (key <= 0) continue;

                if (phase == Phase::MENU) {
                    if (field == 0) {
                        if (key == SDLK_RETURN && !username.empty()) {
                            if (menu_sel == 0) {
                                set_roster_name(0, username.c_str());
                                net->port = NET_PORT;
                                net->start();
                                phase = Phase::LOBBY;
                            } else {
                                field = 1;
                            }
                        }
                        else if (key == SDLK_ESCAPE) {
                            if (!username.empty()) username.clear();
                        }
                        else if ((key == SDLK_TAB || key == SDLK_DOWN || key == SDLK_UP) && username.empty()) {
                            menu_sel = 1 - menu_sel;
                        }
                        else text_feed(username, key, MAX_NAME);
                    }
                    else if (field == 1) {
                        if (key == SDLK_RETURN && !ip_addr.empty()) {
                            Pm& pm = ctx.pm();
                            pm.set_peer_id(255);
                            net->port = 0;
                            net->connect_ip = ip_addr.c_str();
                            net->start_client();
                            phase = Phase::LOBBY;
                        }
                        else if (key == SDLK_ESCAPE) { field = 0; }
                        else text_feed(ip_addr, key, MAX_IP);
                    }
                }
                else if (phase == Phase::LOBBY) {
                    if (key == SDLK_RETURN && ctx.is_host() && roster_count > 0) {
                        PktStart pkt{PKT_START};
                        net->broadcast(ctx.remote_peers(), &pkt, sizeof(pkt));
                        phase = Phase::PLAYING;
                        for (int i = 0; i < roster_count; i++) {
                            uint8_t p = roster[i].peer_id;
                            if (p < 4) player_sys->add_player(ctx.pm(), p);
                        }
                    }
                    else if (key == SDLK_ESCAPE) {
                        net->sock.close_sock();
                        phase = Phase::MENU;
                        field = 0;
                        roster_count = 0;
                    }
                }
            }
        });

        pm.schedule(tname("draw").c_str(), Pm::Phase::HUD + 1.f, [this](Ctx& ctx) {
            auto q = ctx.queue<DrawRect>("draw");
            bool blink = ((int)(cursor_blink * 3.f)) % 2 == 0;

            if (phase == Phase::MENU) { draw_menu(q, blink); return; }
            if (phase == Phase::LOBBY) { draw_lobby(q, ctx.is_host()); return; }
            if (show_players) draw_player_list(q, 0.8f);
        });
    }

    void draw_menu(DrawQueue* q, bool blink) {
        push_str(q, "hellfire", W/2 - 128, 120, 8, 255, 80, 60);

        const char* opts[] = {"host game", "join game"};
        for (int i = 0; i < 2; i++) {
            bool sel = (i == menu_sel);
            push_str(q, opts[i], W/2 - 72, 280 + i*40, 3,
                     sel ? 255 : 100, sel ? 220 : 100, sel ? 80 : 100);
        }
        push_str(q, ">", W/2 - 96, 280 + menu_sel*40, 3, 255, 220, 80);

        if (field == 0) {
            push_str(q, "name:", W/2 - 120, 400, 2, 150, 150, 160);
            int tx = W/2 - 40;
            if (!username.empty()) push_str(q, username.c_str(), tx, 400, 2, 255, 255, 255);
            if (blink) q->push({(float)(tx + (int)username.size()*8), 400, 2, 12, 255, 255, 255, 255});
            push_str(q, "type name - arrows to select - enter", W/2 - 280, H - 40, 2, 80, 80, 90);
        }
        else if (field == 1) {
            push_str(q, "name:", W/2 - 120, 390, 2, 100, 100, 110);
            push_str(q, username.c_str(), W/2 - 40, 390, 2, 180, 180, 180);
            push_str(q, "ip:", W/2 - 120, 420, 2, 150, 150, 160);
            int tx = W/2 - 40;
            if (!ip_addr.empty()) push_str(q, ip_addr.c_str(), tx, 420, 2, 255, 255, 255);
            if (blink) q->push({(float)(tx + (int)ip_addr.size()*8), 420, 2, 12, 255, 255, 255, 255});
            push_str(q, "type ip - enter to connect - esc back", W/2 - 296, H - 40, 2, 80, 80, 90);
        }
    }

    void draw_lobby(DrawQueue* q, bool is_host) {
        push_str(q, "lobby", W/2 - 40, 60, 4, 255, 220, 80);
        draw_player_list(q, 1.f);
        if (is_host) {
            push_str(q, "waiting for players...", W/2 - 168, H - 100, 2, 100, 100, 120);
            if (roster_count > 0)
                push_str(q, "press enter to start", W/2 - 160, H - 60, 3, 200, 200, 200);
        } else {
            push_str(q, "waiting for host...", W/2 - 152, H - 60, 3, 150, 150, 160);
        }
        push_str(q, "esc to go back", W/2 - 112, H - 30, 2, 80, 80, 90);
    }

    void draw_player_list(DrawQueue* q, float alpha) {
        int bw = 280, bh = 30 + roster_count * 30 + 10;
        int bx = W/2 - bw/2, by = 160;
        uint8_t a = (uint8_t)(alpha * 200);
        uint8_t at = (uint8_t)(alpha * 255);
        q->push({(float)bx, (float)by, (float)bw, (float)bh, 20, 20, 30, a});
        push_str(q, "players", bx + 10, by + 8, 2, 200, 200, 210);
        for (int i = 0; i < roster_count; i++) {
            int py = by + 32 + i * 30;
            uint8_t pi = roster[i].peer_id;
            uint8_t cr = (pi < 4) ? PCOL[pi][0] : 180;
            uint8_t cg = (pi < 4) ? PCOL[pi][1] : 180;
            uint8_t cb = (pi < 4) ? PCOL[pi][2] : 180;
            q->push({(float)(bx + 16), (float)(py + 2), 10, 10, cr, cg, cb, at});
            push_str(q, roster[i].name, bx + 34, py, 2, cr, cg, cb);
            if (pi == 0) push_str(q, "host", bx + bw - 50, py, 2, 120, 120, 130);
        }
    }
};

// =============================================================================
// HellfireNet — game-specific networking
// =============================================================================
struct HellfireNet : public System {
    void initialize(Pm& pm) override {
        auto* net = pm.sys<NetSys>("net");
        auto* input_hub = pm.sys<InputHub>("input");
        auto* game_state = pm.sys<GameState>("game");
        auto* player_sys = pm.sys<PlayerSys>("player_sys");
        auto* menu = pm.sys<MenuSys>("menu");

        net->on_recv(PKT_JOIN, [net, menu](Ctx& ctx, const uint8_t* buf, int n, struct sockaddr_in& src) {
            if (!ctx.is_host()) return;
            Pm& pm = ctx.pm();
            PktJoin pkt;
            memcpy(&pkt, buf, std::min(n, (int)sizeof(pkt)));
            pkt.name[MAX_NAME] = 0;

            int id = -1;
            for (uint8_t i : ctx.remote_peers())
                if (net->has_addr[i] && src.sin_port == net->peer_addrs[i].sin_port) id = i;
            if (id < 0) {
                id = pm.connect();
                if (id == 255) return;
                net->peer_addrs[id] = src;
                net->has_addr[id] = true;
                menu->set_roster_name(id, pkt.name[0] ? pkt.name : "peer");
            }
            PktWelcome w{PKT_WELCOME, (uint8_t)id, ctx.peer_count()};
            net->send_to(id, &w, sizeof(w));
            // Send updated roster to everyone
            PktRoster roster;
            menu->build_roster_packet(roster);
            net->broadcast(ctx.remote_peers(), &roster, sizeof(roster));
        });

        net->on_recv(PKT_WELCOME, [](Ctx& ctx, const uint8_t* buf, int, struct sockaddr_in&) {
            PktWelcome w; memcpy(&w, buf, sizeof(w));
            ctx.pm().set_peer_id(w.peer_id);
        });

        net->on_recv(PKT_ROSTER, [menu](Ctx&, const uint8_t* buf, int n, struct sockaddr_in&) {
            if (n < (int)sizeof(PktRoster)) return;
            PktRoster pkt; memcpy(&pkt, buf, sizeof(pkt));
            menu->apply_roster_packet(pkt);
        });

        net->on_recv(PKT_START, [menu](Ctx&, const uint8_t*, int, struct sockaddr_in&) {
            menu->phase = Phase::PLAYING;
        });

        net->on_recv(PKT_INPUT, [net, input_hub](Ctx&, const uint8_t* buf, int n, struct sockaddr_in&) {
            if (n < (int)sizeof(PktInput)) return;
            PktInput p; memcpy(&p, buf, sizeof(p));
            if (p.peer < 4) input_hub->axes[p.peer] = {p.dx, p.dy, p.ax, p.ay, p.shooting != 0};
            if (p.ack_seq != 0 && p.peer < 64) net->on_ack(p.peer, p.ack_seq);
        });

        net->on_recv(PKT_STATE, [game_state, player_sys](Ctx& ctx, const uint8_t* buf, int, struct sockaddr_in&) {
            PktState gs; memcpy(&gs, buf, sizeof(gs));
            game_state->apply_state(ctx, gs, player_sys);
        });

        pm.schedule(tname("client_send").c_str(), Pm::Phase::NET_SEND, [net, input_hub, menu](Ctx& ctx) {
            if (net->sock.sock == INVALID_SOCKET || ctx.is_host()) return;
            if (!net->has_addr[0] && net->connect_ip) {
                net->peer_addrs[0].sin_family = AF_INET;
                net->peer_addrs[0].sin_addr.s_addr = inet_addr(net->connect_ip);
                net->peer_addrs[0].sin_port = htons(NET_PORT);
                net->has_addr[0] = true;
            }
            if (ctx.peer_id() == 255) {
                if (net->should_send) {
                    PktJoin j;
                    strncpy(j.name, menu->username.c_str(), MAX_NAME);
                    net->send_to(0, &j, sizeof(j));
                }
                return;
            }
            if (!net->should_send || menu->phase != Phase::PLAYING) return;
            auto& in = input_hub->axes[ctx.peer_id()];
            PktInput p{PKT_INPUT, ctx.peer_id(), in.dx, in.dy, in.ax, in.ay,
                       (uint8_t)in.shooting, net->client_last_recv_seq};
            net->send_to(0, &p, sizeof(p));
        });
    }
};

// =============================================================================
// BulletSys
// =============================================================================
struct BulletSys : public System {
    Pool<Bullet>* pool = nullptr;

    void initialize(Pm& pm) override {
        pool = pm.pool<Bullet>("bullets");
        auto* game = pm.sys<GameState>("game");
        auto* net = pm.sys<NetSys>("net");
        auto* menu = pm.sys<MenuSys>("menu");
        auto* debug = pm.sys<DebugOverlay>("debug");
        debug->add_size("bullets", [this]{ return pool->items.size(); });

        net->bind(pm, pool, tname("sync").c_str(),
            [](Id id, const Bullet& b, uint8_t* out) -> uint16_t {
                BulSync bs{id, (int16_t)b.pos.x, (int16_t)b.pos.y, (uint8_t)b.size, (uint8_t)(b.player_owned?1:0)};
                memcpy(out, &bs, sizeof(bs));
                return sizeof(bs);
            },
            [](Ctx& ctx, Pool<Bullet>* pool, const uint8_t* data, uint16_t count) {
                for (uint16_t i = 0; i < count; i++) {
                    BulSync bs; memcpy(&bs, data + i * sizeof(BulSync), sizeof(bs));
                    ctx.sync_id(bs.id);
                    pool->add(bs.id, Bullet{{(float)bs.x, (float)bs.y}, {0,0}, 0, (float)bs.sz, bs.po!=0});
                }
            });

        pm.schedule(tname("physics").c_str(), Pm::Phase::SIMULATE, [this, game, menu](Ctx& ctx) {
            if (menu->phase != Phase::PLAYING || game->game_over) return;
            float dt = ctx.dt();
            for (auto [id, b, _] : pool->each()) {
                b.pos += b.vel * dt;
                b.lifetime -= dt;
                pool->unsync(id);
                if (b.lifetime <= 0) ctx.remove(id);
            }
        }, 0.f, RunOn::Host, true);

        pm.schedule(tname("draw").c_str(), Pm::Phase::DRAW, [this, net, menu](Ctx& ctx) {
            if (menu->phase != Phase::PLAYING) return;
            auto draw_q = ctx.queue<DrawRect>("draw");
            float age = ctx.is_host() ? 0.f : net->snapshot_age;
            for (auto& b : pool->items) {
                float hs = b.size * 0.5f;
                float rx = b.pos.x + b.vel.x * age;
                float ry = b.pos.y + b.vel.y * age;
                if (b.player_owned) draw_q->push({rx-hs, ry-hs, b.size, b.size, 255, 255, 255, 255});
                else                draw_q->push({rx-hs, ry-hs, b.size, b.size, 255, 50,  50,  255});
            }
        });
    }
};

// =============================================================================
// PlayerSys
// =============================================================================
struct PlayerSys : public System {
    Pool<Player>* pool = nullptr;
    Id peer_ids[4] = {NULL_ID, NULL_ID, NULL_ID, NULL_ID};

    void add_player(Pm& pm, uint8_t peer) {
        if (peer >= 4 || peer_ids[peer] != NULL_ID) return;
        peer_ids[peer] = pm.spawn();
        pool->add(peer_ids[peer], Player{{SPAWN_X[peer], SPAWN_Y[peer]}, PLAYER_HP, 0, 0, true,
                  PCOL[peer][0], PCOL[peer][1], PCOL[peer][2]});
    }

    void initialize(Pm& pm) override {
        pool = pm.pool<Player>("players");
        auto* game = pm.sys<GameState>("game");
        auto* input = pm.sys<InputHub>("input");
        auto* bullet_pool = pm.pool<Bullet>("bullets");
        auto* menu = pm.sys<MenuSys>("menu");
        auto* debug = pm.sys<DebugOverlay>("debug");
        debug->add_size("players", [this]{ return pool->items.size(); });

        pm.schedule(tname("move").c_str(), Pm::Phase::SIMULATE - 1.f, [this, game, input, bullet_pool, menu](Ctx& ctx) {
            if (menu->phase != Phase::PLAYING || game->game_over) return;
            float dt = ctx.dt();
            for (int pi = 0; pi < 4; pi++) {
                if (!ctx.is_host() && pi != ctx.peer_id()) continue;
                auto* p = pool->get(peer_ids[pi]);
                if (!p || !p->alive) continue;
                auto& in = input->axes[pi];
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
                    bullet_pool->add(ctx.spawn(), Bullet{p->pos, aim*PBULLET_SPEED, PBULLET_LIFE, PBULLET_SIZE, true});
                }
            }
        }, 0.f, RunOn::All, true);

        pm.schedule(tname("draw").c_str(), Pm::Phase::DRAW - 1.f, [this, game, menu](Ctx& ctx) {
            if (menu->phase != Phase::PLAYING) return;
            auto draw_q = ctx.queue<DrawRect>("draw");
            int pi = 0;
            for (auto& p : pool->items) {
                if (p.alive) {
                    float hs = PLAYER_SIZE * 0.5f;
                    if (!(p.invuln > 0 && ((int)(game->time * 15)) % 2 == 0)) {
                        draw_q->push({p.pos.x-hs-1, p.pos.y-hs-1, PLAYER_SIZE+2, PLAYER_SIZE+2, 255, 255, 255, 255});
                        draw_q->push({p.pos.x-hs, p.pos.y-hs, PLAYER_SIZE, PLAYER_SIZE, p.r, p.g, p.b, 255});
                    }
                }
                float bx = (pi%2==0) ? 10.f : (float)(W-170); float by = (pi<2) ? 10.f : 28.f;
                float pct = p.hp / PLAYER_HP;
                draw_q->push({bx, by, 160, 14, 40, 40, 50, 255});
                draw_q->push({bx+1, by+1, 158*pct, 12, (uint8_t)(p.r*pct+255*(1-pct)), (uint8_t)(p.g*pct), (uint8_t)(p.b*pct), 255});
                pi++;
            }
        });
    }
};

// =============================================================================
// MonsterSys
// =============================================================================
struct MonsterSys : public System {
    Pool<Monster>* pool = nullptr;
    void spawn_one(Pm& pm, GameState* game, float intensity, float speed_scale);

    void initialize(Pm& pm) override {
        pool = pm.pool<Monster>("monsters");
        auto* game = pm.sys<GameState>("game");
        auto* player_sys = pm.sys<PlayerSys>("player_sys");
        auto* bullet_pool = pm.pool<Bullet>("bullets");
        auto* net = pm.sys<NetSys>("net");
        auto* menu = pm.sys<MenuSys>("menu");
        auto* debug = pm.sys<DebugOverlay>("debug");
        debug->add_size("monsters", [this]{ return pool->items.size(); });

        net->bind(pm, pool, tname("sync").c_str(),
            [](Id id, const Monster& m, uint8_t* out) -> uint16_t {
                MonSync ms{id, (int16_t)m.pos.x, (int16_t)m.pos.y, (uint8_t)m.size, m.r, m.g, m.b};
                memcpy(out, &ms, sizeof(ms));
                return sizeof(ms);
            },
            [](Ctx& ctx, Pool<Monster>* pool, const uint8_t* data, uint16_t count) {
                for (uint16_t i = 0; i < count; i++) {
                    MonSync ms; memcpy(&ms, data + i * sizeof(MonSync), sizeof(ms));
                    ctx.sync_id(ms.id);
                    pool->add(ms.id, Monster{{(float)ms.x, (float)ms.y}, {0,0}, 0, (float)ms.sz, ms.r, ms.g, ms.b});
                }
            },
            [player_sys](Ctx&, uint8_t peer, Id, const Monster& m, float margin) -> bool {
                if (peer >= 4) return true;
                auto* p = player_sys->pool->get(player_sys->peer_ids[peer]);
                if (!p || !p->alive) return true;
                return dist(m.pos, p->pos) <= INTEREST_RADIUS * (1.f + margin);
            },
            0.3f);

        pm.schedule(tname("spawn").c_str(), Pm::Phase::SIMULATE - 2.f, [this, game, player_sys, bullet_pool, menu](Ctx& ctx) {
            if (menu->phase != Phase::PLAYING || game->game_over) return;
            float dt = ctx.dt();
            game->time += dt;
            const LevelDef& lvl = LEVELS[game->current_level];
            if (game->level_hold > 0.f) { game->level_hold -= dt; return; }
            int next = game->current_level + 1;
            if (next < NUM_LEVELS && game->score >= LEVELS[next].score_threshold) {
                Pm& pm = ctx.pm();
                game->current_level = next;
                game->spawn_accum = 0.f; game->level_flash = 3.0f; game->level_hold = 3.0f;
                for (int i = 0; i < 4; i++) {
                    auto* p = player_sys->pool->get(player_sys->peer_ids[i]);
                    if (p && p->alive) {
                        p->pos.x = SPAWN_X[i] + game->rng.rfr(-80, 80);
                        p->pos.y = SPAWN_Y[i] + game->rng.rfr(-60, 60);
                        p->invuln = 2.0f;
                    }
                }
                for (auto [id, m, _] : pool->each()) pm.remove(id);
                for (auto [id, b, _] : bullet_pool->each()) pm.remove(id);
                return;
            }
            if (game->score >= WIN_SCORE && !game->win) { game->win = true; game->game_over = true; return; }
            float intensity = (sinf(game->time * 0.4f) * 0.5f + 0.5f) * 0.6f
                            + (sinf(game->time * 0.08f) * 0.5f + 0.5f) * 0.4f;
            if ((int)pool->items.size() >= lvl.max_monsters) return;
            game->spawn_accum += (1.f + 8.f * intensity) * lvl.spawn_mult * dt;
            Pm& pm = ctx.pm();
            while (game->spawn_accum >= 1.f && (int)pool->items.size() < lvl.max_monsters) {
                game->spawn_accum -= 1.f;
                spawn_one(pm, game, intensity, lvl.speed_mult * lvl.size_mult);
            }
        }, 0.f, RunOn::Host, true);

        pm.schedule(tname("ai").c_str(), Pm::Phase::SIMULATE + 1.f, [this, game, player_sys, bullet_pool, menu](Ctx& ctx) {
            if (menu->phase != Phase::PLAYING || game->game_over) return;
            float dt = ctx.dt();
            for (auto [id, m, _] : pool->each()) {
                Vec2 tgt = m.pos; float best = 1e9f;
                for (auto& p : player_sys->pool->items)
                    if (p.alive && dist(m.pos, p.pos) < best) { best = dist(m.pos, p.pos); tgt = p.pos; }
                Vec2 desired = norm(tgt - m.pos) * len(m.vel);
                m.vel.x += (desired.x - m.vel.x) * 0.5f * dt;
                m.vel.y += (desired.y - m.vel.y) * 0.5f * dt;
                m.shoot_timer -= dt;
                if (m.shoot_timer <= 0 && best < 500.f) {
                    m.shoot_timer = game->rng.rfr(2.f, 5.f);
                    Vec2 dir = norm(tgt - m.pos);
                    float sp = game->rng.rfr(-0.15f, 0.15f);
                    float cs = cosf(sp), sn = sinf(sp);
                    Vec2 aim = {dir.x*cs - dir.y*sn, dir.x*sn + dir.y*cs};
                    bullet_pool->add(ctx.spawn(), Bullet{m.pos, aim*MBULLET_SPEED, MBULLET_LIFE, MBULLET_SIZE, false});
                }
                m.pos += m.vel * dt;
                pool->unsync(id);
            }
        }, 0.f, RunOn::Host, true);

        pm.schedule(tname("draw").c_str(), Pm::Phase::DRAW + 1.f, [this, net, menu](Ctx& ctx) {
            if (menu->phase != Phase::PLAYING) return;
            auto draw_q = ctx.queue<DrawRect>("draw");
            float age = ctx.is_host() ? 0.f : net->snapshot_age;
            for (auto& m : pool->items) {
                float hs = m.size * 0.5f;
                float rx = m.pos.x + m.vel.x * age;
                float ry = m.pos.y + m.vel.y * age;
                draw_q->push({rx-hs, ry-hs, m.size, m.size, m.r, m.g, m.b, 255});
            }
        });
    }
};

void MonsterSys::spawn_one(Pm& pm, GameState* game, float intensity, float speed_scale) {
    Monster m;
    const LevelDef& lvl = LEVELS[game->current_level];
    float speed = MONSTER_SPEED * (0.8f + intensity * 0.6f) * std::min(speed_scale, 3.f);
    m.size = game->rng.rfr(MONSTER_MIN_SZ * lvl.size_mult, MONSTER_MAX_SZ * lvl.size_mult);
    switch (game->rng.next() % 4) {
        case 0: m.pos = {game->rng.rfr(0, W), -30}; break;
        case 1: m.pos = {game->rng.rfr(0, W), H+30}; break;
        case 2: m.pos = {-30, game->rng.rfr(0, H)}; break;
        case 3: m.pos = {W+30, game->rng.rfr(0, H)}; break;
    }
    Vec2 tgt = {W*0.5f + game->rng.rfr(-200, 200), H*0.5f + game->rng.rfr(-200, 200)};
    m.vel = norm(tgt - m.pos) * speed;
    m.shoot_timer = game->rng.rfr(1.5f, 4.f);
    float hue = game->rng.rf();
    if      (hue < 0.3f) { m.r = 255; m.g = 80;  m.b = 60;  }
    else if (hue < 0.5f) { m.r = 255; m.g = 140; m.b = 40;  }
    else if (hue < 0.7f) { m.r = 255; m.g = 60;  m.b = 120; }
    else if (hue < 0.85f){ m.r = 200; m.g = 50;  m.b = 200; }
    else                 { m.r = 255; m.g = 200; m.b = 50;  }
    pool->add(pm.spawn(), m);
}

// =============================================================================
// GameState
// =============================================================================
struct GameState : public System {
    bool game_over = false, win = false;
    float time = 0, spawn_accum = 0;
    int score = 0, kills = 0;
    int current_level = 0;
    float level_flash = 0.f, level_hold = 0.f;
    Rng rng{42};

    void apply_state(Ctx& ctx, const PktState& gs, PlayerSys* ps) {
        time = gs.time; score = gs.score; kills = gs.kills; game_over = gs.gameover;
        if (gs.paused) ctx.pause(); else ctx.resume();
        for (int i = 0; i < gs.pcnt && i < 4; i++) {
            auto* p = ps->pool->get(ps->peer_ids[i]);
            if (!p) { ps->add_player(ctx.pm(), i); p = ps->pool->get(ps->peer_ids[i]); }
            if (p) {
                Vec2 srv = {gs.p[i].x, gs.p[i].y};
                float err = dist(p->pos, srv);
                if (err > 120.f) p->pos = srv;
                else if (err > 1.f) {
                    float a = 0.1f + (err / 120.f) * 0.3f;
                    p->pos.x += (srv.x - p->pos.x) * a;
                    p->pos.y += (srv.y - p->pos.y) * a;
                }
                p->hp = gs.p[i].hp; p->alive = gs.p[i].alive;
            }
        }
    }

    void reset() {
        game_over = false; win = false; time = 0; score = 0; kills = 0;
        spawn_accum = 0; current_level = 0; level_flash = 0.f; level_hold = 0.f; rng = Rng{42};
    }

    void initialize(Pm& pm) override {
        auto* ps   = pm.sys<PlayerSys>("player_sys");
        auto* mp   = pm.pool<Monster>("monsters");
        auto* bp   = pm.pool<Bullet>("bullets");
        auto* net  = pm.sys<NetSys>("net");
        auto* menu = pm.sys<MenuSys>("menu");
        auto* debug = pm.sys<DebugOverlay>("debug");

        debug->add_stat("net", [net, mp, bp](char* b, int n) {
            snprintf(b, n, "snap:%.1fms  pend_m:%zu  pend_b:%zu",
                net->snapshot_age*1000.f, mp->pending_count(), bp->pending_count());
        });
        debug->add_stat("game", [this](char* b, int n) {
            snprintf(b, n, "%s  lvl:%d  score:%d", game_over?"OVER":"running", current_level+1, score);
        });

        pm.schedule(tname("keys").c_str(), Pm::Phase::INPUT + 1.f, [this, menu, ps, mp, bp](Ctx& ctx) {
            if (menu->phase != Phase::PLAYING) return;
            auto keys = ctx.queue<int>("keys");
            for (auto key : *keys) {
                if (key <= 0) continue;
                if (key == SDLK_ESCAPE) {
                    if (game_over) {
                        menu->phase = Phase::MENU;
                        menu->field = 0;
                        menu->username.clear();
                        menu->roster_count = 0;
                        reset();
                        Pm& pm = ctx.pm();
                        for (auto [id, m, _] : mp->each()) pm.remove(id);
                        for (auto [id, b, _] : bp->each()) pm.remove(id);
                        for (int i = 0; i < 4; i++) ps->peer_ids[i] = NULL_ID;
                    }
                    else if (ctx.is_host()) ctx.toggle_pause();
                }
                if (key == SDLK_r && game_over && ctx.is_host()) {
                    Pm& pm = ctx.pm();
                    reset(); ctx.resume();
                    for (auto [id, m, _] : mp->each()) pm.remove(id);
                    for (auto [id, b, _] : bp->each()) pm.remove(id);
                    for (int i = 0; i < 4; i++) {
                        auto* p = ps->pool->get(ps->peer_ids[i]);
                        if (p) { p->hp = PLAYER_HP; p->alive = true; p->pos = {SPAWN_X[i], SPAWN_Y[i]}; p->cooldown = 0; p->invuln = 0; }
                    }
                }
            }
        });

        pm.schedule(tname("collision").c_str(), Pm::Phase::COLLIDE, [this, menu, ps, mp, bp, net](Ctx& ctx) {
            if (menu->phase != Phase::PLAYING || game_over) return;
            float pr = PLAYER_SIZE * 0.5f;
            for (auto [bid, b, _] : bp->each()) {
                if (!b.player_owned || ctx.is_removing(bid)) continue;
                for (auto [mid, m, _2] : mp->each()) {
                    if (ctx.is_removing(mid)) continue;
                    if (dist(b.pos, m.pos) < b.size + m.size*0.5f) {
                        net->track_removal(ctx, mp->pool_id, mid);
                        net->track_removal(ctx, bp->pool_id, bid);
                        ctx.remove(mid); ctx.remove(bid);
                        score += 10; kills++; break;
                    }
                }
            }
            for (int i = 0; i < 4; i++) {
                auto* p = ps->pool->get(ps->peer_ids[i]);
                if (!p || !p->alive || p->invuln > 0) continue;
                for (auto [bid, b, _] : bp->each()) {
                    if (!b.player_owned && !ctx.is_removing(bid) && dist(b.pos, p->pos) < b.size + pr) {
                        p->hp -= BULLET_DMG; p->invuln = PLAYER_INVULN;
                        net->track_removal(ctx, bp->pool_id, bid);
                        ctx.remove(bid);
                    }
                }
                for (auto [mid, m, _] : mp->each())
                    if (dist(m.pos, p->pos) < m.size*0.5f + pr) { p->hp -= CONTACT_DMG; p->invuln = PLAYER_INVULN*0.5f; }
                if (p->hp <= 0) { p->hp = 0; p->alive = false; }
            }
            bool any = false;
            for (auto& p : ps->pool->items) if (p.alive) any = true;
            if (!any && !ps->pool->items.empty()) game_over = true;
        }, 0.f, RunOn::Host, true);

        pm.schedule(tname("cleanup").c_str(), Pm::Phase::CLEANUP, [menu, mp, bp, net](Ctx& ctx) {
            if (menu->phase != Phase::PLAYING) return;
            for (auto [id, m, _] : mp->each())
                if (m.pos.x<-100||m.pos.x>W+100||m.pos.y<-100||m.pos.y>H+100) { net->track_removal(ctx, mp->pool_id, id); ctx.remove(id); }
            for (auto [id, b, _] : bp->each())
                if (b.pos.x<-50||b.pos.x>W+50||b.pos.y<-50||b.pos.y>H+50) { net->track_removal(ctx, bp->pool_id, id); ctx.remove(id); }
        }, 0.f, RunOn::Host, true);

        pm.schedule(tname("net_send").c_str(), Pm::Phase::NET_SEND, [this, net, ps, menu](Ctx& ctx) {
            if (!net->should_send || !ctx.is_host() || menu->phase != Phase::PLAYING) return;
            PktState gs{PKT_STATE, net->net_frame, time, score, kills, (uint8_t)ctx.is_paused(), game_over, 0};
            for (int i = 0; i < 4; i++) {
                auto* p = ps->pool->get(ps->peer_ids[i]);
                if (p) { gs.p[gs.pcnt++] = {p->pos.x, p->pos.y, p->hp, (uint8_t)p->alive}; }
            }
            net->broadcast(ctx.remote_peers(), &gs, sizeof(gs));
        }, 0.f, RunOn::Host);

        pm.schedule(tname("hud").c_str(), Pm::Phase::HUD, [this, menu](Ctx& ctx) {
            if (menu->phase != Phase::PLAYING) return;
            auto q = ctx.queue<DrawRect>("draw");
            { char b[64]; snprintf(b, sizeof(b), "score: %d  %s", score, LEVELS[current_level].name);
              push_str(q, b, 8, 8, 2, 200, 200, 200); }
            if (level_flash > 0.f) {
                level_flash -= ctx.dt();
                uint8_t a = (uint8_t)(std::min(level_flash, 1.f) * 255);
                q->push({0, (float)H/2-20, (float)W, 36, 0, 0, 0, (uint8_t)(a*0.7f)});
                char lb[32]; snprintf(lb, sizeof(lb), "%s", LEVELS[current_level].name);
                push_str(q, lb, W/2-(int)strlen(lb)*8, H/2-14, 4, 255, 220, 80);
            }
            float inten = (sinf(time*0.4f)*0.5f+0.5f)*0.6f + (sinf(time*0.08f)*0.5f+0.5f)*0.4f;
            q->push({(float)(W/2-60), (float)(H-14), 120, 4, 40, 40, 50, 255});
            q->push({(float)(W/2-60), (float)(H-14), 120*inten, 4, (uint8_t)(255*inten), 50, (uint8_t)(200*(1-inten)), 255});
            if (ctx.is_paused() && !game_over) {
                q->push({0, 0, (float)W, (float)H, 0, 0, 0, 140});
                q->push({(float)(W/2-20), (float)(H/2-25), 12, 50, 200, 200, 200, 255});
                q->push({(float)(W/2+8),  (float)(H/2-25), 12, 50, 200, 200, 200, 255});
            }
            if (game_over) {
                q->push({0, 0, (float)W, (float)H, 0, 0, 0, 160});
                if (win) push_str(q, "you win", W/2-56, H/2-60, 4, 80, 255, 150);
                else     push_str(q, "game over", W/2-72, H/2-60, 4, 255, 80, 80);
                { char b[32]; snprintf(b, sizeof(b), "score: %d", score); push_str(q, b, W/2-60, H/2-10, 3, 255, 100, 100); }
                push_str(q, "r restart  esc menu", W/2-152, H/2+40, 2, 150, 150, 160);
            }
            push_str(q, "v", W-16, H-14, 2, 60, 60, 70);
        });
    }
};

// =============================================================================
// Main
// =============================================================================
int main(int, char**) {
    Pm pm;
    pm.set_loop_rate(60.f);

    pm.sys<SdlSystem>("sdl")->open("hellfire", W, H);
    pm.sys<InputHub>("input");
    pm.sys<NetSys>("net");
    pm.sys<MenuSys>("menu");
    pm.sys<GameState>("game");
    pm.sys<BulletSys>("bullet_sys");
    pm.sys<PlayerSys>("player_sys");
    pm.sys<MonsterSys>("monster_sys");
    pm.sys<HellfireNet>("hellfire_net");
    pm.sys<StatsSystem>("stats");
    pm.sys<DebugOverlay>("debug");

    pm.run_loop();
    return 0;
}