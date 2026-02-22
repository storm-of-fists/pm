// hellfire_client.cpp — Client for hellfire (renders, sends input, spawns local server for hosting)
// Build: g++ -std=c++17 -O3 -o hellfire_client hellfire_client.cpp $(sdl2-config --cflags --libs)
// Run:   ./hellfire_client

#include "pm_core.hpp"
#include "pm_math.hpp"
#include "pm_udp.hpp"
#include "pm_sdl.hpp"
#include "pm_debug.hpp"
#include "hellfire_common.hpp"
#include <cstdio>
#include <cstring>
#include <string>
#include <algorithm>

#ifdef _WIN32
#include <windows.h>
#else
#include <signal.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <unistd.h>
#endif

using namespace pm;


// =============================================================================
// Child process management
// =============================================================================

static std::string exe_dir() {
#ifdef _WIN32
    char buf[MAX_PATH];
    GetModuleFileNameA(nullptr, buf, MAX_PATH);
    std::string s(buf);
    auto pos = s.find_last_of("\\/");
    return (pos != std::string::npos) ? s.substr(0, pos + 1) : "./";
#else
    char buf[1024];
    ssize_t len = readlink("/proc/self/exe", buf, sizeof(buf) - 1);
    if (len <= 0) return "./";
    buf[len] = 0;
    std::string s(buf);
    auto pos = s.rfind('/');
    return (pos != std::string::npos) ? s.substr(0, pos + 1) : "./";
#endif
}

struct ChildProcess {
#ifdef _WIN32
    PROCESS_INFORMATION pi{};
    bool running = false;

    bool spawn(const char* exe, const char* port_str) {
        STARTUPINFOA si{}; si.cb = sizeof(si);
        char buf[512];
        snprintf(buf, sizeof(buf), "%s %s", exe, port_str);
        if (!CreateProcessA(nullptr, buf, nullptr, nullptr, FALSE, 0, nullptr, nullptr, &si, &pi))
            return false;
        running = true;
        return true;
    }
    void kill() {
        if (!running) return;
        TerminateProcess(pi.hProcess, 0);
        CloseHandle(pi.hProcess);
        CloseHandle(pi.hThread);
        running = false;
    }
#else
    pid_t pid = 0;
    bool running = false;

    bool spawn(const char* exe, const char* port_str) {
        pid = fork();
        if (pid < 0) return false;
        if (pid == 0) {
            execlp(exe, exe, port_str, (char*)nullptr);
            _exit(1);
        }
        running = true;
        return true;
    }
    void kill() {
        if (!running || pid <= 0) return;
        ::kill(pid, SIGTERM);
        int status;
        waitpid(pid, &status, 0);
        running = false;
        pid = 0;
    }
#endif
    ~ChildProcess() { kill(); }
};

static ChildProcess g_server_process;

// =============================================================================
// Client game state (receive-only mirror)
// =============================================================================
struct ClientState {
    float time = 0;
    int score = 0, kills = 0;
    int current_level = 0;
    bool game_over = false, win = false, paused = false;
    uint16_t round = 0;
    float level_flash = 0.f;
};

// =============================================================================
// MenuState — title screen, text input, lobby
// =============================================================================
enum class GamePhase { MENU, LOBBY, PLAYING };

struct MenuState {
    GamePhase phase = GamePhase::MENU;
    int menu_sel = 0;     // 0=host, 1=join
    int field = 0;        // 0=username, 1=ip (join only)
    std::string username;
    std::string ip_addr = "127.0.0.1";
    float cursor_blink = 0;
    bool show_players = false;
    bool is_host_client = false;

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

    static void text_feed(std::string& str, int key, int max_len) {
        if (key == SDLK_BACKSPACE) { if (!str.empty()) str.pop_back(); return; }
        if ((int)str.size() >= max_len) return;
        if (key >= SDLK_a && key <= SDLK_z) { str += (char)key; return; }
        if (key >= SDLK_0 && key <= SDLK_9) { str += (char)key; return; }
        if (key == SDLK_PERIOD) { str += '.'; return; }
        if (key == SDLK_MINUS) { str += '-'; return; }
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

    void draw_lobby(DrawQueue* q, bool host) {
        push_str(q, "lobby", W/2 - 40, 60, 4, 255, 220, 80);
        draw_player_list(q, 1.f);
        if (host) {
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
// ClientNetState — pools + network receive state
// =============================================================================
struct ClientNetState {
    ClientState gs{};
    Pool<Player>* players = nullptr;
    Pool<Monster>* monsters = nullptr;
    Pool<Bullet>* bullets = nullptr;
    Id peer_ids[4] = {NULL_ID, NULL_ID, NULL_ID, NULL_ID};

    void add_player(Pm& pm, uint8_t peer) {
        if (peer >= 4 || peer_ids[peer] != NULL_ID) return;
        peer_ids[peer] = pm.spawn();
        players->add(peer_ids[peer], Player{{SPAWN_X[peer], SPAWN_Y[peer]}, PLAYER_HP, 0, 0, true,
                  PCOL[peer][0], PCOL[peer][1], PCOL[peer][2]});
    }
};

// =============================================================================
// menu_init — title screen + lobby input/draw
// =============================================================================
void menu_init(Pm& pm) {
    auto* menu = pm.state<MenuState>("menu");
    auto* net  = pm.state<NetSys>("net");
    auto* keys_q = pm.state<KeyQueue>("keys");

    pm.schedule("menu/input", Phase::INPUT + 2.f, [menu, net, keys_q](TaskContext& ctx) {
        menu->cursor_blink += ctx.dt();

        if (menu->phase == GamePhase::PLAYING) {
            for (auto key : *keys_q) {
                if (key == SDLK_v) menu->show_players = true;
                if (key == -SDLK_v) menu->show_players = false;
            }
            return;
        }

        for (auto key : *keys_q) {
            if (key <= 0) continue;

            if (menu->phase == GamePhase::MENU) {
                if (menu->field == 0) {
                    if (key == SDLK_RETURN && !menu->username.empty()) {
                        if (menu->menu_sel == 0) {
                            menu->is_host_client = true;
                            std::string server_bin = exe_dir() + "hellfire_server";
                            char port_buf[8]; snprintf(port_buf, sizeof(port_buf), "%d", NET_PORT);
                            if (!g_server_process.spawn(server_bin.c_str(), port_buf)) {
                                printf("[client] failed to spawn server!\n");
                                menu->is_host_client = false;
                                return;
                            }
                            printf("[client] spawned local server\n");
                            SDL_Delay(100);
                            menu->ip_addr = "127.0.0.1";
                            net->set_peer_id(255);
                            net->port = 0;
                            net->connect_ip = menu->ip_addr.c_str();
                            net->start_client();
                            menu->phase = GamePhase::LOBBY;
                        } else {
                            menu->field = 1;
                        }
                    }
                    else if (key == SDLK_ESCAPE) {
                        if (!menu->username.empty()) menu->username.clear();
                    }
                    else if ((key == SDLK_TAB || key == SDLK_DOWN || key == SDLK_UP) && menu->username.empty()) {
                        menu->menu_sel = 1 - menu->menu_sel;
                    }
                    else MenuState::text_feed(menu->username, key, MAX_NAME);
                }
                else if (menu->field == 1) {
                    if (key == SDLK_RETURN && !menu->ip_addr.empty()) {
                        net->set_peer_id(255);
                        net->port = 0;
                        net->connect_ip = menu->ip_addr.c_str();
                        net->start_client();
                        menu->phase = GamePhase::LOBBY;
                    }
                    else if (key == SDLK_ESCAPE) { menu->field = 0; }
                    else MenuState::text_feed(menu->ip_addr, key, MAX_IP);
                }
            }
            else if (menu->phase == GamePhase::LOBBY) {
                if (key == SDLK_RETURN && menu->is_host_client && menu->roster_count > 0) {
                    PktStart pkt{PKT_START};
                    net->send_to(0, &pkt, sizeof(pkt));
                }
                else if (key == SDLK_ESCAPE) {
                    net->sock.close_sock();
                    if (menu->is_host_client) { g_server_process.kill(); menu->is_host_client = false; }
                    menu->phase = GamePhase::MENU;
                    menu->field = 0;
                    menu->roster_count = 0;
                }
            }
        }
    });

    auto* draw_q_ref = pm.state<DrawQueue>("draw");
    pm.schedule("menu/draw", Phase::HUD + 1.f, [menu, draw_q_ref](TaskContext& ctx) {
        bool blink = ((int)(menu->cursor_blink * 3.f)) % 2 == 0;

        if (menu->phase == GamePhase::MENU) { menu->draw_menu(draw_q_ref, blink); return; }
        if (menu->phase == GamePhase::LOBBY) { menu->draw_lobby(draw_q_ref, menu->is_host_client); return; }
        if (menu->show_players) menu->draw_player_list(draw_q_ref, 0.8f);
    });
}

// =============================================================================
// client_net_init — send input, receive state
// =============================================================================
void client_net_init(Pm& pm) {
    auto* cn   = pm.state<ClientNetState>("client_net");
    cn->players  = pm.pool<Player>("players");
    cn->monsters = pm.pool<Monster>("monsters");
    cn->bullets  = pm.pool<Bullet>("bullets");
    auto* net  = pm.state<NetSys>("net");
    auto* menu = pm.state<MenuState>("menu");

    // --- Pool sync (recv only — client never sends pool data) ---
    net->bind_recv(cn->monsters, read_monsters);
    net->bind_recv(cn->bullets, read_bullets);

    // --- Connection handshake ---
    net->protocol_version = 1;
    net->on_connected([menu](NetSys& n, uint8_t peer_id, const uint8_t* payload, uint16_t size) {
        printf("[client] assigned peer %d\n", peer_id);
        // ACK payload contains roster
        if (size >= sizeof(PktRoster)) {
            PktRoster pkt; memcpy(&pkt, payload, sizeof(pkt));
            apply_roster(pkt, menu->roster, menu->roster_count);
        }
    });
    net->on_connect_denied([](NetSys&, uint8_t reason) {
        if (reason == 0) printf("[client] connection timed out\n");
        else printf("[client] connection denied (reason %d)\n", reason);
    });

    // --- Packet handlers ---
    net->on_recv(PKT_ROSTER, [menu](TaskContext&, const uint8_t* buf, int n, struct sockaddr_in&) {
        if (n < (int)sizeof(PktRoster)) return;
        PktRoster pkt; memcpy(&pkt, buf, sizeof(pkt));
        apply_roster(pkt, menu->roster, menu->roster_count);
    });

    net->on_recv(PKT_START, [menu](TaskContext&, const uint8_t*, int, struct sockaddr_in&) {
        menu->phase = GamePhase::PLAYING;
    });

    net->on_state_recv(STATE_ID_GAME, [cn](TaskContext& ctx, const uint8_t* buf, uint16_t) {
        PktState pkt; memcpy(&pkt, buf, sizeof(pkt));
        cn->gs.time = pkt.time; cn->gs.score = pkt.score; cn->gs.kills = pkt.kills;
        cn->gs.game_over = pkt.gameover; cn->gs.paused = pkt.paused;
        cn->gs.round = pkt.round;
        int new_level = 0;
        for (int i = NUM_LEVELS - 1; i >= 0; i--) {
            if (cn->gs.score >= LEVELS[i].score_threshold) { new_level = i; break; }
        }
        if (new_level > cn->gs.current_level) cn->gs.level_flash = 3.0f;
        cn->gs.current_level = new_level;
        if (cn->gs.score >= WIN_SCORE) cn->gs.win = true;
        for (int i = 0; i < pkt.pcnt && i < 4; i++) {
            auto* p = cn->players->get(cn->peer_ids[i]);
            if (!p) { cn->add_player(ctx.pm(), i); p = cn->players->get(cn->peer_ids[i]); }
            if (p) {
                Vec2 srv = {pkt.p[i].x, pkt.p[i].y};
                float err = dist(p->pos, srv);
                if (err > 120.f) p->pos = srv;
                else if (err > 1.f) {
                    float a = 0.1f + (err / 120.f) * 0.3f;
                    p->pos.x += (srv.x - p->pos.x) * a;
                    p->pos.y += (srv.y - p->pos.y) * a;
                }
                p->hp = pkt.p[i].hp; p->alive = pkt.p[i].alive;
            }
        }
    });

    // --- Send input to server ---
    pm.schedule("client_send", Phase::NET_SEND, [cn, net, menu](TaskContext& ctx) {
        if (net->sock.sock == INVALID_SOCKET) return;

        // Initiate connection handshake if not yet connecting/connected
        if (net->conn_state == NetSys::ConnState::DISCONNECTED && net->connect_ip) {
            char name_buf[MAX_NAME + 1] = {};
            strncpy(name_buf, menu->username.c_str(), MAX_NAME);
            net->request_connect(net->connect_ip, NET_PORT, name_buf, MAX_NAME + 1);
            return;
        }
        if (net->conn_state != NetSys::ConnState::CONNECTED) return;
        if (!net->should_send || menu->phase != GamePhase::PLAYING) return;
        if (cn->gs.paused) return;

        const uint8_t* k = SDL_GetKeyboardState(nullptr);
        Input in{};
        if (k[SDL_SCANCODE_W] || k[SDL_SCANCODE_UP])    in.dy -= 1;
        if (k[SDL_SCANCODE_S] || k[SDL_SCANCODE_DOWN])  in.dy += 1;
        if (k[SDL_SCANCODE_A] || k[SDL_SCANCODE_LEFT])  in.dx -= 1;
        if (k[SDL_SCANCODE_D] || k[SDL_SCANCODE_RIGHT]) in.dx += 1;
        int mx, my; uint32_t mb = SDL_GetMouseState(&mx, &my);
        in.ax = (float)mx; in.ay = (float)my;
        in.shooting = (mb & SDL_BUTTON_LMASK) != 0;

        auto* p = cn->players->get(cn->peer_ids[net->peer_id()]);
        if (p && p->alive) {
            float dt = ctx.dt();
            Vec2 move = {in.dx, in.dy};
            float ml = len(move);
            if (ml > 0.001f) move = move * (1.f / ml);
            p->pos += move * (PLAYER_SPEED * dt);
            float hs = PLAYER_SIZE * 0.5f;
            p->pos.x = std::clamp(p->pos.x, hs, (float)W - hs);
            p->pos.y = std::clamp(p->pos.y, hs, (float)H - hs);
        }

        PktInput pkt{PKT_INPUT, net->peer_id(), in.dx, in.dy, in.ax, in.ay,
                     (uint8_t)in.shooting};
        net->send_to(0, &pkt, sizeof(pkt));
    });

    // --- Game key handling (pause, restart, back to menu) ---
    auto* keys_q = pm.state<KeyQueue>("keys");
    pm.schedule("game_keys", Phase::INPUT + 1.f, [cn, net, menu, keys_q](TaskContext& ctx) {
        if (menu->phase != GamePhase::PLAYING) return;
        for (auto key : *keys_q) {
            if (key <= 0) continue;
            if (key == SDLK_ESCAPE) {
                if (cn->gs.game_over) {
                    net->sock.close_sock();
                    if (menu->is_host_client) { g_server_process.kill(); menu->is_host_client = false; }
                    menu->phase = GamePhase::MENU;
                    menu->field = 0;
                    menu->username.clear();
                    menu->roster_count = 0;
                    cn->gs = ClientState{};
                    Pm& pm = ctx.pm();
                    for (auto [id, m, _] : cn->monsters->each()) pm.remove_entity(id);
                    for (auto [id, b, _] : cn->bullets->each()) pm.remove_entity(id);
                    for (int i = 0; i < 4; i++) cn->peer_ids[i] = NULL_ID;
                }
                else if (menu->is_host_client) {
                    PktPause pkt{PKT_PAUSE};
                    net->send_to(0, &pkt, sizeof(pkt));
                }
            }
            if (key == SDLK_r && cn->gs.game_over && menu->is_host_client) {
                PktRestart pkt{PKT_RESTART};
                net->send_to(0, &pkt, sizeof(pkt));
                cn->gs = ClientState{};
            }
        }
    });

    // --- Client-side staleness cleanup ---
    pm.schedule("stale_cleanup", Phase::CLEANUP, [cn, menu](TaskContext& ctx) {
        if (menu->phase != GamePhase::PLAYING || cn->gs.paused) return;
        float dt = ctx.dt();
        for (auto [id, b, _] : cn->bullets->each())  { b.lifetime += dt; if (b.lifetime > 2.f) ctx.remove_entity(id); }
        for (auto [id, m, _] : cn->monsters->each()) { m.shoot_timer += dt; if (m.shoot_timer > 2.f) ctx.remove_entity(id); }
    });
}

// =============================================================================
// draw_init — all rendering
// =============================================================================
void draw_init(Pm& pm) {
    auto* cn    = pm.state<ClientNetState>("client_net");
    auto* menu  = pm.state<MenuState>("menu");
    auto* net   = pm.state<NetSys>("net");
    auto* debug = pm.state<DebugOverlay>("debug");

    debug->add_size("players", [cn]{ return cn->players->items.size(); });
    debug->add_size("monsters", [cn]{ return cn->monsters->items.size(); });
    debug->add_size("bullets", [cn]{ return cn->bullets->items.size(); });
    debug->add_stat("net", [net](char* b, int n) {
        snprintf(b, n, "snap:%.1fms", net->snapshot_age(0)*1000.f);
    });
    debug->add_stat("game", [cn](char* b, int n) {
        snprintf(b, n, "%s  lvl:%d  score:%d",
            cn->gs.game_over?"OVER":"running", cn->gs.current_level+1, cn->gs.score);
    });

    auto* draw_q = pm.state<DrawQueue>("draw");

    // --- Draw players ---
    pm.schedule("draw_players", Phase::DRAW - 1.f, [cn, menu, draw_q](TaskContext& ctx) {
        if (menu->phase != GamePhase::PLAYING) return;
        int pi = 0;
        for (auto& p : cn->players->items) {
            if (p.alive) {
                float hs = PLAYER_SIZE * 0.5f;
                if (!(p.invuln > 0 && ((int)(cn->gs.time * 15)) % 2 == 0)) {
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

    // --- Draw monsters ---
    pm.schedule("draw_monsters", Phase::DRAW + 1.f, [cn, net, menu, draw_q](TaskContext& ctx) {
        if (menu->phase != GamePhase::PLAYING) return;
        float age = cn->gs.paused ? 0.f : net->snapshot_age(0);
        for (auto& m : cn->monsters->items) {
            float hs = m.size * 0.5f;
            float rx = m.pos.x + m.vel.x * age;
            float ry = m.pos.y + m.vel.y * age;
            draw_q->push({rx-hs, ry-hs, m.size, m.size, m.r, m.g, m.b, 255});
        }
    });

    // --- Draw bullets ---
    pm.schedule("draw_bullets", Phase::DRAW, [cn, net, menu, draw_q](TaskContext& ctx) {
        if (menu->phase != GamePhase::PLAYING) return;
        float age = cn->gs.paused ? 0.f : net->snapshot_age(0);
        for (auto& b : cn->bullets->items) {
            float hs = b.size * 0.5f;
            float rx = b.pos.x + b.vel.x * age;
            float ry = b.pos.y + b.vel.y * age;
            if (b.player_owned) draw_q->push({rx-hs, ry-hs, b.size, b.size, 255, 255, 255, 255});
            else                draw_q->push({rx-hs, ry-hs, b.size, b.size, 255, 50,  50,  255});
        }
    });

    // --- HUD ---
    pm.schedule("hud", Phase::HUD, [cn, menu, draw_q](TaskContext& ctx) {
        if (menu->phase != GamePhase::PLAYING) return;
        auto* gs = &cn->gs;
        { char b[64]; snprintf(b, sizeof(b), "score: %d  %s", gs->score, LEVELS[gs->current_level].name);
          push_str(draw_q, b, 8, 8, 2, 200, 200, 200); }
        if (gs->level_flash > 0.f) {
            gs->level_flash -= ctx.dt();
            uint8_t a = (uint8_t)(std::min(gs->level_flash, 1.f) * 255);
            draw_q->push({0, (float)H/2-20, (float)W, 36, 0, 0, 0, (uint8_t)(a*0.7f)});
            char lb[32]; snprintf(lb, sizeof(lb), "%s", LEVELS[gs->current_level].name);
            push_str(draw_q, lb, W/2-(int)strlen(lb)*8, H/2-14, 4, 255, 220, 80);
        }
        float inten = (sinf(gs->time*0.4f)*0.5f+0.5f)*0.6f + (sinf(gs->time*0.08f)*0.5f+0.5f)*0.4f;
        draw_q->push({(float)(W/2-60), (float)(H-14), 120, 4, 40, 40, 50, 255});
        draw_q->push({(float)(W/2-60), (float)(H-14), 120*inten, 4, (uint8_t)(255*inten), 50, (uint8_t)(200*(1-inten)), 255});
        if (gs->paused && !gs->game_over) {
            draw_q->push({0, 0, (float)W, (float)H, 0, 0, 0, 100});
            push_str(draw_q, "paused", W/2-48, H/2-20, 4, 200, 200, 220);
            if (menu->is_host_client)
                push_str(draw_q, "esc resume", W/2-80, H/2+20, 2, 150, 150, 160);
        }
        if (gs->game_over) {
            draw_q->push({0, 0, (float)W, (float)H, 0, 0, 0, 160});
            if (gs->win) push_str(draw_q, "you win", W/2-56, H/2-60, 4, 80, 255, 150);
            else         push_str(draw_q, "game over", W/2-72, H/2-60, 4, 255, 80, 80);
            { char b[32]; snprintf(b, sizeof(b), "score: %d", gs->score); push_str(draw_q, b, W/2-60, H/2-10, 3, 255, 100, 100); }
            if (menu->is_host_client)
                push_str(draw_q, "r restart  esc menu", W/2-152, H/2+40, 2, 150, 150, 160);
            else
                push_str(draw_q, "esc menu", W/2-64, H/2+40, 2, 150, 150, 160);
        }
        push_str(draw_q, "v", W-16, H-14, 2, 60, 60, 70);
    });
}

// =============================================================================
// Main
// =============================================================================
int main(int, char**) {
    Pm pm;
    // No set_loop_rate() — SDL vsync paces frames

    auto* sdl = pm.state<SdlSystem>("sdl");
    sdl->open("hellfire", W, H);
    sdl_init(pm, sdl);

    auto* net = pm.state<NetSys>("net");
    net_init(pm, net);

    menu_init(pm);
    client_net_init(pm);
    draw_init(pm);

    auto* debug = pm.state<DebugOverlay>("debug");
    debug_init(pm, debug);

    pm.run();

    g_server_process.kill();

    return 0;
}