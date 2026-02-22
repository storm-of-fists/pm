// hellfire_common.hpp — Shared types for hellfire server + client
#pragma once
#include "pm_core.hpp"
#include "pm_math.hpp"
#include <cstring>
#include <cstdint>
#include <algorithm>

using namespace pm;

// =============================================================================
// EventBuf<T> — Simple push/clear container (replaces kernel Queue)
//
// Use inside pm.state<EventBuf<T>>("name") for named event channels.
// User is responsible for clearing — typically at start of producer or
// end of consumer each frame.
// =============================================================================
template<typename T>
struct EventBuf {
    std::vector<T> items;
    void push(T val) { items.push_back(std::move(val)); }
    void clear() { items.clear(); }
    size_t size() const { return items.size(); }
    auto begin() { return items.begin(); }
    auto end() { return items.end(); }
    auto begin() const { return items.begin(); }
    auto end() const { return items.end(); }
};

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
static constexpr int WIN_SCORE = 8000;
static constexpr uint32_t STATE_ID_GAME = 1;  // state sync id for game state

static const uint8_t PCOL[4][3] = {{0,220,255}, {0,255,120}, {255,160,40}, {255,80,200}};
static const float SPAWN_X[4] = {W*.25f, W*.75f, W*.25f, W*.75f};
static const float SPAWN_Y[4] = {H*.4f, H*.4f, H*.6f, H*.6f};

// =============================================================================
// Levels
// =============================================================================
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

// =============================================================================
// Components
// =============================================================================
struct Monster { Vec2 pos, vel; float shoot_timer=0, size=0; uint8_t r=255, g=255, b=255; };
struct Bullet  { Vec2 pos, vel; float lifetime=0, size=0; bool player_owned=false; };
struct Player  { Vec2 pos; float hp=PLAYER_HP, cooldown=0, invuln=0; bool alive=true; uint8_t r=0, g=220, b=255; };
struct Input   { float dx=0, dy=0, ax=0, ay=0; bool shooting=false; };

struct PlayerInfo {
    char name[MAX_NAME + 1] = {};
    uint8_t peer_id = 255;
    bool connected = false;
};

// =============================================================================
// Wire format
// =============================================================================
#pragma pack(push, 1)
enum PktType : uint8_t {
    PKT_INPUT=0, PKT_STATE=1, PKT_JOIN=2, PKT_WELCOME=3,
    PKT_ROSTER=4, PKT_START=5, PKT_PAUSE=6, PKT_RESTART=7
};

struct PktInput   { uint8_t type=PKT_INPUT; uint8_t peer; float dx,dy,ax,ay; uint8_t shooting; };
struct PktJoin    { uint8_t type=PKT_JOIN; char name[MAX_NAME+1]={}; };
struct PktWelcome { uint8_t type=PKT_WELCOME; uint8_t peer_id, pcnt; };
struct PktState {
    uint8_t type=PKT_STATE; uint32_t frame; float time;
    int score, kills; uint8_t paused, gameover, pcnt; uint16_t round;
    struct { float x,y,hp; uint8_t alive; } p[4];
};
struct PktRoster {
    uint8_t type=PKT_ROSTER; uint8_t count;
    struct Entry { uint8_t peer_id; char name[MAX_NAME+1]; } entries[4];
};
struct PktStart   { uint8_t type=PKT_START; };
struct PktPause   { uint8_t type=PKT_PAUSE; };
struct PktRestart { uint8_t type=PKT_RESTART; };

struct MonSync { Id id; int16_t x,y,vx,vy; uint8_t sz,r,g,b; };
struct BulSync { Id id; int16_t x,y,vx,vy; uint8_t sz,po; };
#pragma pack(pop)

// =============================================================================
// Sync helpers — used by server (write) and client (read)
// =============================================================================
static inline uint16_t write_monster(Id id, const Monster& m, uint8_t* out) {
    MonSync ms{id, (int16_t)m.pos.x, (int16_t)m.pos.y, (int16_t)m.vel.x, (int16_t)m.vel.y, (uint8_t)m.size, m.r, m.g, m.b};
    memcpy(out, &ms, sizeof(ms)); return sizeof(ms);
}
static inline void read_monsters(TaskContext& ctx, Pool<Monster>* pool, const uint8_t* data, uint16_t count) {
    for (uint16_t i = 0; i < count; i++) {
        MonSync ms; memcpy(&ms, data + i * sizeof(MonSync), sizeof(ms));
        if (!ctx.sync_id(ms.id)) continue;
        pool->add(ms.id, Monster{{(float)ms.x, (float)ms.y}, {(float)ms.vx, (float)ms.vy}, 0, (float)ms.sz, ms.r, ms.g, ms.b});
    }
}
static inline uint16_t write_bullet(Id id, const Bullet& b, uint8_t* out) {
    BulSync bs{id, (int16_t)b.pos.x, (int16_t)b.pos.y, (int16_t)b.vel.x, (int16_t)b.vel.y, (uint8_t)b.size, (uint8_t)(b.player_owned?1:0)};
    memcpy(out, &bs, sizeof(bs)); return sizeof(bs);
}
static inline void read_bullets(TaskContext& ctx, Pool<Bullet>* pool, const uint8_t* data, uint16_t count) {
    for (uint16_t i = 0; i < count; i++) {
        BulSync bs; memcpy(&bs, data + i * sizeof(BulSync), sizeof(bs));
        if (!ctx.sync_id(bs.id)) continue;
        pool->add(bs.id, Bullet{{(float)bs.x, (float)bs.y}, {(float)bs.vx, (float)bs.vy}, 0, (float)bs.sz, bs.po!=0});
    }
}

// =============================================================================
// Roster helpers
// =============================================================================
static inline void build_roster(const PlayerInfo* roster, int count, PktRoster& pkt) {
    pkt.type = PKT_ROSTER; pkt.count = (uint8_t)count;
    for (int i = 0; i < count && i < 4; i++) {
        pkt.entries[i].peer_id = roster[i].peer_id;
        memcpy(pkt.entries[i].name, roster[i].name, MAX_NAME + 1);
    }
}
static inline void apply_roster(const PktRoster& pkt, PlayerInfo* roster, int& count) {
    count = std::min((int)pkt.count, 4);
    for (int i = 0; i < count; i++) {
        roster[i].peer_id = pkt.entries[i].peer_id;
        roster[i].connected = true;
        memcpy(roster[i].name, pkt.entries[i].name, MAX_NAME + 1);
    }
}