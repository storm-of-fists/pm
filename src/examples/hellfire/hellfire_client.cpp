// hellfire_client.cpp — Client for hellfire (renders, sends input, spawns local server for hosting)
// Build: cmake --preset debug && cmake --build build --target hellfire_client
// Run:   ./build/hellfire_client

#include "pm_core.hpp"
#include "pm_math.hpp"
#include "pm_udp.hpp"
#include "pm_sdl.hpp"
#include "pm_sprite.hpp"
#include "pm_util.hpp"
#include "pm_debug.hpp"
#include "pm_mod.hpp"
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
// SpriteStore — loaded sprites (one per shared SDL renderer lifetime)
// =============================================================================
struct SpriteStore {
    Sprite player_front;
    Sprite player_back;
    // Per-player facing: true = front sprite (toward camera), false = back sprite (away).
    Hysteresis<bool> facing[4];
    float prev_y[4] = {};
};

// =============================================================================
// Camera — world-to-screen transform with smooth zoom + player follow
// =============================================================================
struct Camera {
    Vec2 center = {W * 0.5f, H * 0.5f};
    float zoom = 1.0f;
    float target_zoom = 1.0f;

    Vec2 world_to_screen(Vec2 world) const {
        return (world - center) * zoom + Vec2{W * 0.5f, H * 0.5f};
    }
    Vec2 screen_to_world(Vec2 screen) const {
        return (screen - Vec2{W * 0.5f, H * 0.5f}) / zoom + center;
    }
    float scale(float size) const { return size * zoom; }
};

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
    // Server-side diagnostic info (updated via PKT_DBG once/sec)
    uint16_t srv_monsters = 0, srv_bullets = 0;
    float srv_ms = 0.f;
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
    bool disconnected = false;          // show "server disconnected" banner on menu
    bool needs_disconnect_cleanup = false; // signal stale_cleanup to remove entities

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
        if (key >= SDLK_A && key <= SDLK_Z) { str += (char)key; return; }
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
                if (key == SDLK_V) menu->show_players = true;
                if (key == -SDLK_V) menu->show_players = false;
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

        if (menu->phase == GamePhase::MENU) {
            menu->draw_menu(draw_q_ref, blink);
            if (menu->disconnected) {
                // "disconnected" = 12 chars × 4px × scale 2 = 96px wide; center at W/2-48
                int bx = W/2 - 52, by = H - 64;
                draw_q_ref->push({(float)bx, (float)by, 104.f, 16.f, 60, 10, 10, 220});
                push_str(draw_q_ref, "disconnected", bx + 4, by + 4, 2, 255, 100, 80);
            }
            return;
        }
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
        menu->disconnected = false;
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
    net->on_disconnect([menu, cn](NetSys& n, uint8_t peer_id) {
        if (peer_id != 0) return;  // only care about server (slot 0)
        if (menu->phase == GamePhase::MENU) return;
        printf("[client] lost connection to server\n");
        n.conn_state = NetSys::ConnState::DISCONNECTED;
        n.connect_ip = nullptr;
        if (menu->is_host_client) { g_server_process.kill(); menu->is_host_client = false; }
        menu->phase = GamePhase::MENU;
        menu->field = 0;
        menu->roster_count = 0;
        menu->disconnected = true;
        menu->needs_disconnect_cleanup = true;
        cn->gs = ClientState{};
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

    net->on_recv(PKT_DBG, [cn](TaskContext&, const uint8_t* buf, int n, struct sockaddr_in&) {
        if (n < (int)sizeof(PktDbg)) return;
        PktDbg pkt; memcpy(&pkt, buf, sizeof(pkt));
        cn->gs.srv_monsters = pkt.monsters;
        cn->gs.srv_bullets  = pkt.bullets;
        cn->gs.srv_ms       = pkt.ms_per_tick;
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
    auto* cam = pm.state<Camera>("camera");
    pm.schedule("client_send", Phase::NET_SEND, [cn, net, menu, cam](TaskContext& ctx) {
        if (net->sock.sock == INVALID_SOCKET) return;

        // Initiate connection handshake if not yet connecting/connected
        if (net->conn_state == NetSys::ConnState::DISCONNECTED && net->connect_ip) {
            char name_buf[MAX_NAME + 1] = {};
            strncpy(name_buf, menu->username.c_str(), MAX_NAME);
            net->request_connect(net->connect_ip, NET_PORT, name_buf, MAX_NAME + 1);
            return;
        }
        if (net->conn_state != NetSys::ConnState::CONNECTED) return;

        // Keep server connection alive in lobby — remote_peers() excludes self (slot 0 = server),
        // so the framework heartbeat never fires on a client. Without this the server times out the
        // host after peer_timeout seconds and reallocates slot 0 to the next joiner.
        if (net->should_send && menu->phase == GamePhase::LOBBY) {
            PktHeartbeat hb{};
            net->send_to(0, &hb, sizeof(hb));
        }

        if (!net->should_send || menu->phase != GamePhase::PLAYING) return;
        if (cn->gs.paused) {
            PktHeartbeat hb{};
            net->send_to(0, &hb, sizeof(hb));
            return;
        }

        const bool* k = SDL_GetKeyboardState(nullptr);
        Input in{};
        if (k[SDL_SCANCODE_W] || k[SDL_SCANCODE_UP])    in.dy -= 1;
        if (k[SDL_SCANCODE_S] || k[SDL_SCANCODE_DOWN])  in.dy += 1;
        if (k[SDL_SCANCODE_A] || k[SDL_SCANCODE_LEFT])  in.dx -= 1;
        if (k[SDL_SCANCODE_D] || k[SDL_SCANCODE_RIGHT]) in.dx += 1;
        float mx, my; SDL_MouseButtonFlags mb = SDL_GetMouseState(&mx, &my);
        Vec2 aim = cam->screen_to_world({mx, my});
        in.ax = aim.x; in.ay = aim.y;
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
                    cn->monsters->each([&](Id id, const Monster&) { pm.remove_entity(id); }, Parallel::Off);
                    cn->bullets->each([&](Id id, const Bullet&) { pm.remove_entity(id); }, Parallel::Off);
                    for (int i = 0; i < 4; i++) cn->peer_ids[i] = NULL_ID;
                }
                else if (menu->is_host_client) {
                    PktPause pkt{PKT_PAUSE};
                    net->send_to(0, &pkt, sizeof(pkt));
                }
            }
            if (key == SDLK_R && cn->gs.game_over && menu->is_host_client) {
                PktRestart pkt{PKT_RESTART};
                net->send_to(0, &pkt, sizeof(pkt));
                cn->gs = ClientState{};
            }
        }
    });

    // --- Client-side staleness cleanup ---
    pm.schedule("stale_cleanup", Phase::CLEANUP, [cn, menu](TaskContext& ctx) {
        if (menu->needs_disconnect_cleanup) {
            cn->monsters->each([&](Id id, const Monster&) { ctx.remove_entity(id); }, Parallel::Off);
            cn->bullets->each([&](Id id, const Bullet&) { ctx.remove_entity(id); }, Parallel::Off);
            for (int i = 0; i < 4; i++) cn->peer_ids[i] = NULL_ID;
            menu->needs_disconnect_cleanup = false;
            return;
        }
        if (menu->phase != GamePhase::PLAYING || cn->gs.paused) return;
        float dt = ctx.dt();
        cn->bullets->each_mut([&](Id id, Bullet& b)  { b.lifetime += dt; if (b.lifetime > 2.f) ctx.remove_entity(id); }, Parallel::Off);
        cn->monsters->each_mut([&](Id id, Monster& m) { m.shoot_timer += dt; if (m.shoot_timer > 2.f) ctx.remove_entity(id); }, Parallel::Off);
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
    debug->add_stat("server", [cn](char* b, int n) {
        snprintf(b, n, "srv mon:%d bul:%d %.1fms",
            cn->gs.srv_monsters, cn->gs.srv_bullets, cn->gs.srv_ms);
    });

    auto* draw_q  = pm.state<DrawQueue>("draw");
    auto* sdl     = pm.state<SdlSystem>("sdl");
    auto* sprites = pm.state<SpriteStore>("sprites");

    // --- Load player sprites ---
    {
        std::string base = exe_dir() + "resources/";
        sprites->player_front.load(sdl->renderer, base + "connor-front.png");
        sprites->player_back.load(sdl->renderer,  base + "connor-back.png");
        for (auto& f : sprites->facing) f = Hysteresis<bool>(false, 0.1f);
    }

    // --- Hot-reload sprites if files change on disk (runs ~1Hz) ---
    pm.schedule("sprite/hotreload", Phase::HUD, [sdl, sprites](TaskContext& ctx) {
        static float timer = 0.f;
        timer += ctx.dt();
        if (timer < 1.f) return;
        timer = 0.f;
        if (sprites->player_front.changed()) { printf("[sprite] reloading player_front\n"); sprites->player_front.reload(sdl->renderer); }
        if (sprites->player_back.changed())  { printf("[sprite] reloading player_back\n");  sprites->player_back.reload(sdl->renderer); }
    });

    // --- Camera: zoom input + follow local player ---
    auto* cam   = pm.state<Camera>("camera");
    auto* wheel = pm.state<float>("wheel");
    auto* keys_q2 = pm.state<KeyQueue>("keys");
    pm.schedule("camera/update", Phase::INPUT + 3.f, [cam, wheel, keys_q2, cn, net, menu](TaskContext& ctx) {
        if (menu->phase != GamePhase::PLAYING) return;
        float dt = ctx.dt();

        // Zoom input: mouse wheel + PgUp/PgDown
        float zoom_ticks = *wheel;
        for (auto key : *keys_q2) {
            if (key == SDLK_PAGEUP)   zoom_ticks += 1.f;
            if (key == SDLK_PAGEDOWN) zoom_ticks -= 1.f;
        }
        if (zoom_ticks != 0.f) {
            cam->target_zoom *= std::pow(1.1f, zoom_ticks);
            cam->target_zoom = std::clamp(cam->target_zoom, 0.5f, 3.0f);
        }

        // Smooth zoom
        cam->zoom += (cam->target_zoom - cam->zoom) * 8.0f * dt;

        // Follow local player
        if (net->peer_id() < 4) {
            auto* p = cn->players->get(cn->peer_ids[net->peer_id()]);
            if (p && p->alive)
                cam->center += (p->pos - cam->center) * 6.0f * dt;
        }
    });

    // --- Draw player HP bars (DrawQueue rects — rendered at Phase::RENDER behind sprites) ---
    pm.schedule("draw_players", Phase::DRAW - 1.f, [cn, menu, draw_q](TaskContext& ctx) {
        if (menu->phase != GamePhase::PLAYING) return;
        int pi = 0;
        for (auto& p : cn->players->items) {
            float bx = (pi%2==0) ? 10.f : (float)(W-170); float by = (pi<2) ? 10.f : 28.f;
            float pct = p.hp / PLAYER_HP;
            draw_q->push({bx, by, 160, 14, 40, 40, 50, 255});
            draw_q->push({bx+1, by+1, 158*pct, 12, (uint8_t)(p.r*pct+255*(1-pct)), (uint8_t)(p.g*pct), (uint8_t)(p.b*pct), 255});
            pi++;
        }
    });

    // --- Draw player sprites (after DrawQueue flush, before present) ---
    pm.schedule("sprite/players", Phase::RENDER + 0.5f, [cn, menu, sdl, sprites, cam](TaskContext& ctx) {
        if (menu->phase != GamePhase::PLAYING) return;
        int pi = 0;
        for (auto& p : cn->players->items) {
            // Facing: front = moving down (toward camera), back = moving up (away).
            float dy = p.pos.y - sprites->prev_y[pi];
            sprites->facing[pi].update(ctx.dt());
            if      (dy >  0.5f) sprites->facing[pi].set(true);
            else if (dy < -0.5f) sprites->facing[pi].set(false);
            sprites->prev_y[pi] = p.pos.y;

            if (p.alive && !(p.invuln > 0 && ((int)(cn->gs.time * 15)) % 2 == 0)) {
                Vec2 sp = cam->world_to_screen(p.pos);
                float sz = cam->scale(PLAYER_SIZE);
                Sprite& spr = sprites->facing[pi] ? sprites->player_front : sprites->player_back;
                if (spr) {
                    spr.draw_centered(sdl->renderer, sp.x, sp.y, sz);
                } else {
                    SDL_FRect dst = {sp.x - sz*0.5f, sp.y - sz*0.5f, sz, sz};
                    SDL_SetRenderDrawBlendMode(sdl->renderer, SDL_BLENDMODE_NONE);
                    SDL_SetRenderDrawColor(sdl->renderer, p.r, p.g, p.b, 255);
                    SDL_RenderFillRect(sdl->renderer, &dst);
                }
            }
            pi++;
        }
    });

    // --- Draw monsters ---
    pm.schedule("draw_monsters", Phase::DRAW + 1.f, [cn, net, menu, draw_q, cam](TaskContext& ctx) {
        if (menu->phase != GamePhase::PLAYING) return;
        float age = cn->gs.paused ? 0.f : net->snapshot_age(0);
        for (auto& m : cn->monsters->items) {
            float rx = m.pos.x + m.vel.x * age;
            float ry = m.pos.y + m.vel.y * age;
            Vec2 sp = cam->world_to_screen({rx, ry});
            float sz = cam->scale(m.size);
            float hs = sz * 0.5f;
            draw_q->push({sp.x-hs, sp.y-hs, sz, sz, m.r, m.g, m.b, 255});
        }
    });

    // --- Draw bullets ---
    pm.schedule("draw_bullets", Phase::DRAW, [cn, net, menu, draw_q, cam](TaskContext& ctx) {
        if (menu->phase != GamePhase::PLAYING) return;
        float age = cn->gs.paused ? 0.f : net->snapshot_age(0);
        for (auto& b : cn->bullets->items) {
            float rx = b.pos.x + b.vel.x * age;
            float ry = b.pos.y + b.vel.y * age;
            Vec2 sp = cam->world_to_screen({rx, ry});
            float sz = cam->scale(b.size);
            float hs = sz * 0.5f;
            if (b.player_owned) draw_q->push({sp.x-hs, sp.y-hs, sz, sz, 255, 255, 255, 255});
            else                draw_q->push({sp.x-hs, sp.y-hs, sz, sz, 255, 50,  50,  255});
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
    sdl_init(pm, sdl, Phase::INPUT, Phase::RENDER);

    auto* net = pm.state<NetSys>("net");
    net_init(pm, net, Phase::NET_RECV, Phase::NET_SEND);

    menu_init(pm);
    client_net_init(pm);
    draw_init(pm);

    auto* debug = pm.state<DebugOverlay>("debug");
    debug_init(pm, debug, Phase::INPUT, Phase::HUD);

    ModLoader mods;
    mods.watch(exe_dir() + "mods/example_mod.so");
    mods.load_all(pm);
    pm.schedule("mods/poll", Phase::INPUT - 5.f, [&mods](TaskContext& ctx) {
        mods.poll(ctx.pm());
    });

    pm.run();

    mods.unload_all(pm);
    g_server_process.kill();

    return 0;
}