// hellfire_diag.hpp — Diagnostic report for hellfire server + client
//
// Collects per-frame timing, entity peaks, network stats, events, and
// per-second timeline samples. Writes a JSON report on game completion.
//
// Usage:
//   struct MyState { DiagReport diag; ... };
//   gs->diag.sample_frame(pm.loop_dt());
//   gs->diag.push_event(time, "peer %d joined", id);
//   gs->diag.write_json("/tmp/report.json");

#pragma once
#include <vector>
#include <cstdio>
#include <cstring>
#include <cstdarg>

struct DiagEvent {
    float time;
    char msg[80];
};

struct DiagSample {
    float time;
    int monsters, bullets, players_alive;
    int score, kills;
    float frame_ms;
    float snap_age_ms;
};

struct DiagPeer {
    int id = -1;
    char name[16] = {};
    float connected_at = 0;
    bool alive_at_end = false;
};

struct DiagReport {
    // --- Identity ---
    const char* role = "unknown";
    char name[16] = {};
    int peer_id = -1;

    // --- Session ---
    float duration = 0;
    uint32_t frames = 0;

    // --- Game outcome ---
    int score = 0, kills = 0, level = 0;
    bool game_over = false, win = false;

    // --- Frame timing ---
    float frame_ms_min = 1e9f, frame_ms_max = 0, frame_ms_sum = 0;
    uint32_t timing_count = 0;

    // --- Entity peaks ---
    int peak_monsters = 0, peak_bullets = 0, peak_players = 0;
    uint32_t total_spawns = 0, total_removes = 0;

    // --- Network (client-side) ---
    float snap_age_sum = 0, snap_age_max = 0;
    uint32_t snap_age_samples = 0;

    // --- Peers ---
    static constexpr int MAX_DIAG_PEERS = 8;
    DiagPeer peers[MAX_DIAG_PEERS];
    int peer_count = 0;
    float peer_connect_time[MAX_DIAG_PEERS] = {};

    // --- Events + timeline ---
    std::vector<DiagEvent> events;
    std::vector<DiagSample> timeline;

    // ── Helpers ──────────────────────────────────────────────────────────────

    void sample_frame(float dt) {
        float ms = dt * 1000.f;
        if (ms > 0.f && ms < 1000.f) {
            if (ms < frame_ms_min) frame_ms_min = ms;
            if (ms > frame_ms_max) frame_ms_max = ms;
            frame_ms_sum += ms;
            timing_count++;
        }
        frames++;
    }

    void track_entities(int m, int b, int p) {
        if (m > peak_monsters) peak_monsters = m;
        if (b > peak_bullets)  peak_bullets = b;
        if (p > peak_players)  peak_players = p;
    }

    void track_snapshot_age(float age_s) {
        float ms = age_s * 1000.f;
        snap_age_sum += ms;
        if (ms > snap_age_max) snap_age_max = ms;
        snap_age_samples++;
    }

    void push_event(float time, const char* fmt, ...) {
        DiagEvent e;
        e.time = time;
        va_list ap;
        va_start(ap, fmt);
        vsnprintf(e.msg, sizeof(e.msg), fmt, ap);
        va_end(ap);
        events.push_back(e);
    }

    // ── JSON output ─────────────────────────────────────────────────────────

    static void esc(FILE* f, const char* s) {
        while (*s) {
            if (*s == '"') fputs("\\\"", f);
            else if (*s == '\\') fputs("\\\\", f);
            else fputc(*s, f);
            s++;
        }
    }

    void write_json(const char* path) const {
        FILE* f = fopen(path, "w");
        if (!f) { printf("[diag] ERROR: cannot write %s\n", path); return; }

        float avg_ms = timing_count > 0 ? frame_ms_sum / (float)timing_count : 0;
        float safe_min = frame_ms_min < 1e8f ? frame_ms_min : 0;
        bool is_client = (strcmp(role, "client") == 0);

        fputs("{\n", f);
        fprintf(f, "  \"role\": \"%s\",\n", role);
        fprintf(f, "  \"name\": \""); esc(f, name); fputs("\",\n", f);
        fprintf(f, "  \"peer_id\": %d,\n", peer_id);
        fprintf(f, "  \"duration\": %.2f,\n", duration);
        fprintf(f, "  \"frames\": %u,\n", frames);

        fputs("  \"outcome\": {\n", f);
        fprintf(f, "    \"score\": %d,\n", score);
        fprintf(f, "    \"kills\": %d,\n", kills);
        fprintf(f, "    \"level\": %d,\n", level);
        fprintf(f, "    \"game_over\": %s,\n", game_over ? "true" : "false");
        fprintf(f, "    \"win\": %s\n", win ? "true" : "false");
        fputs("  },\n", f);

        fputs("  \"timing\": {\n", f);
        fprintf(f, "    \"min_ms\": %.3f,\n", safe_min);
        fprintf(f, "    \"max_ms\": %.3f,\n", frame_ms_max);
        fprintf(f, "    \"avg_ms\": %.3f\n", avg_ms);
        fputs("  },\n", f);

        fputs("  \"entities\": {\n", f);
        fprintf(f, "    \"peak_monsters\": %d,\n", peak_monsters);
        fprintf(f, "    \"peak_bullets\": %d,\n", peak_bullets);
        fprintf(f, "    \"peak_players\": %d,\n", peak_players);
        fprintf(f, "    \"total_spawns\": %u,\n", total_spawns);
        fprintf(f, "    \"total_removes\": %u\n", total_removes);
        fputs("  },\n", f);

        if (is_client) {
            float snap_avg = snap_age_samples > 0 ? snap_age_sum / (float)snap_age_samples : 0;
            fputs("  \"network\": {\n", f);
            fprintf(f, "    \"snapshot_age_avg_ms\": %.2f,\n", snap_avg);
            fprintf(f, "    \"snapshot_age_max_ms\": %.2f\n", snap_age_max);
            fputs("  },\n", f);
        }

        fputs("  \"peers\": [", f);
        for (int i = 0; i < peer_count; i++) {
            fprintf(f, "%s\n    {\"id\": %d, \"name\": \"", i > 0 ? "," : "", peers[i].id);
            esc(f, peers[i].name);
            fprintf(f, "\", \"connected_at\": %.2f, \"alive_at_end\": %s}",
                    peers[i].connected_at, peers[i].alive_at_end ? "true" : "false");
        }
        fputs("\n  ],\n", f);

        fputs("  \"events\": [", f);
        for (size_t i = 0; i < events.size(); i++) {
            fprintf(f, "%s\n    {\"t\": %.2f, \"msg\": \"", i > 0 ? "," : "", events[i].time);
            esc(f, events[i].msg);
            fputs("\"}", f);
        }
        fputs("\n  ],\n", f);

        fputs("  \"timeline\": [", f);
        for (size_t i = 0; i < timeline.size(); i++) {
            auto& s = timeline[i];
            fprintf(f, "%s\n    {\"t\": %.1f, \"monsters\": %d, \"bullets\": %d, \"players\": %d, "
                    "\"score\": %d, \"kills\": %d, \"frame_ms\": %.2f",
                    i > 0 ? "," : "",
                    s.time, s.monsters, s.bullets, s.players_alive,
                    s.score, s.kills, s.frame_ms);
            if (is_client)
                fprintf(f, ", \"snap_age_ms\": %.2f", s.snap_age_ms);
            fputc('}', f);
        }
        fputs("\n  ]\n", f);

        fputs("}\n", f);
        fclose(f);
        printf("[diag] wrote %s\n", path);
    }
};
