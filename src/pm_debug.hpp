// pm_debug.hpp — Debug overlay system
//
// Tab       — toggle overlay (sampling only runs while visible)
// Ctrl+R    — reset stats (clears timing rings + task max_us)
// F10       — single-step one frame (while paused)
//
// Shows:
//   - FPS / frame time / tick / pause+step state
//   - Task table: name | step | median | max (us, heat-colored)
//   - Entity stats: live count, +spawns/-removes, pending queue
//   - Pool sizes and custom stats from game code
//
// API:
//   debug->add_stat("label", [](char* buf, int n) { snprintf(...); });
//   debug->add_size("pool_name", []{ return pool->items.size(); });
//
// Register: pm.sys<DebugOverlay>("debug");
// Depends: pm_core.hpp, pm_sdl.hpp

#pragma once
#include "pm_core.hpp"
#include "pm_sdl.hpp"
#include <functional>
#include <vector>
#include <string>
#include <cstdio>
#include <cstring>
#include <unordered_map>

namespace pm {

class DebugOverlay : public System {
public:
    bool visible = false;

    struct StatEntry {
        std::string label;
        std::function<void(char*, int)> fn;
    };
    struct SizeEntry {
        std::string label;
        std::function<size_t()> fn;
    };

    std::vector<StatEntry> stats;
    std::vector<SizeEntry> sizes;

    void add_stat(const char* label, std::function<void(char*, int)> fn) {
        stats.push_back({label, std::move(fn)});
    }

    void add_size(const char* label, std::function<size_t()> fn) {
        sizes.push_back({label, std::move(fn)});
    }

    void initialize(Pm& pm) override {
        pm.schedule(tname("input").c_str(), Phase::INPUT + 3.f, [this](Ctx& ctx) {
            auto keys = ctx.queue<int>("keys");
            for (auto key : *keys) {
                if (key <= 0) continue;

                if (key == SDLK_TAB) {
                    visible = !visible;
                    if (visible) reset(ctx.pm());
                }

                if (!visible) continue;

                if (key == SDLK_r && (SDL_GetModState() & KMOD_CTRL)) {
                    reset(ctx.pm());
                }

                if (key == SDLK_F10) {
                    ctx.pm().request_step();
                }
            }
        });

        pm.schedule(tname("sample").c_str(), Phase::HUD + 4.f, [this](Ctx& ctx) {
            if (!visible) return;
            Pm& pm = ctx.pm();

            // FPS
            fps_frames++;
            fps_accum += ctx.dt();
            if (fps_accum >= 0.5f) {
                fps = (float)fps_frames / fps_accum;
                fps_frames = 0;
                fps_accum = 0;
            }
            frame_ms = ctx.dt() * 1000.f;

            // Sample task timings into our ring buffers
            for (auto& t : pm.tasks()) {
                if (!t.active) continue;
                rings[t.name].push(t.last_us);
            }
        });

        pm.schedule(tname("draw").c_str(), Phase::HUD + 5.f, [this](Ctx& ctx) {
            if (!visible) return;
            auto q = ctx.queue<DrawRect>("draw");
            Pm& pm = ctx.pm();
            int sc = 2;
            int cw = 4 * sc;
            int rh = 6 * sc + 2;

            int pad = 8;
            int x0 = pad;
            int y = pad;

            int col_ord  = x0;
            int col_name = x0 + 4 * cw;
            int col_step = x0 + 30 * cw;
            int col_med  = col_step + 9 * cw;
            int col_max  = col_med + 9 * cw;
            int panel_w  = col_max + 9 * cw + pad;

            int task_rows = 0;
            auto& order = pm.task_order();
            auto& all_tasks = pm.tasks();
            for (auto ti : order)
                if (all_tasks[ti].active) task_rows++;

            int total_rows = 1                   // fps + state
                + 1                              // controls hint
                + 1 + 1 + task_rows              // header + sep + tasks
                + 1 + 1                          // total sep + total row
                + 1                              // entities
                + (int)sizes.size()
                + (int)stats.size()
                + 1;
            int panel_h = pad + total_rows * rh + pad;

            // Background
            q->push({0, 0, (float)panel_w, (float)panel_h, 8, 8, 14, 220});

            // FPS + state
            {
                char buf[96];
                const char* state = "";
                if (ctx.is_paused() && ctx.stepping()) state = "  |step|";
                else if (ctx.is_paused())              state = "  |paused|";
                snprintf(buf, sizeof(buf), "fps:%.0f  dt:%.1f ms  tick:%llu%s",
                    fps, frame_ms, (unsigned long long)pm.tick_count(), state);
                push_str(q, buf, x0, y, sc, 140, 220, 140);
                y += rh;
            }

            // Controls hint
            push_str(q, "ctrl+r reset  f10 step", x0, y, sc, 70, 70, 90);
            y += rh;

            // Table header
            push_str(q, "#", col_ord, y, sc, 120, 120, 140);
            push_str(q, "task", col_name, y, sc, 120, 120, 140);
            rpad(q, "step", col_step, 8 * cw, y, sc, 120, 120, 140);
            rpad(q, "median", col_med, 8 * cw, y, sc, 120, 120, 140);
            rpad(q, "max", col_max, 8 * cw, y, sc, 120, 120, 140);
            y += rh;

            // Separator
            q->push({(float)x0, (float)y, (float)(panel_w - 2 * pad), 1, 60, 60, 80, 200});
            y += 4;

            // Task rows (in execution order)
            uint64_t total_step = 0, total_med = 0;
            int task_num = 0;
            for (auto ti : order) {
                auto& t = all_tasks[ti];
                if (!t.active) continue;
                const char* name = pm.name_str(t.name);

                char nbuf[8];
                snprintf(nbuf, sizeof(nbuf), "%d", task_num++);
                push_str(q, nbuf, col_ord, y, sc, 80, 80, 100);
                push_str(q, name, col_name, y, sc, 180, 180, 200, 25);

                char buf[16];
                uint8_t cr, cg, cb;

                fmt_us(buf, sizeof(buf), t.last_us);
                heat(t.last_us, cr, cg, cb);
                rpad(q, buf, col_step, 8 * cw, y, sc, cr, cg, cb);
                total_step += t.last_us;

                auto it = rings.find(t.name);
                uint64_t med = (it != rings.end()) ? it->second.median() : 0;
                fmt_us(buf, sizeof(buf), med);
                heat(med, cr, cg, cb);
                rpad(q, buf, col_med, 8 * cw, y, sc, cr, cg, cb);
                total_med += med;

                fmt_us(buf, sizeof(buf), t.max_us);
                heat(t.max_us, cr, cg, cb);
                rpad(q, buf, col_max, 8 * cw, y, sc, cr, cg, cb);

                y += rh;
            }

            // Total row
            q->push({(float)x0, (float)y, (float)(panel_w - 2 * pad), 1, 60, 60, 80, 200});
            y += 4;
            {
                push_str(q, "total", col_name, y, sc, 150, 150, 180);
                char buf[16];
                uint8_t cr, cg, cb;
                fmt_us(buf, sizeof(buf), total_step);
                heat(total_step, cr, cg, cb);
                rpad(q, buf, col_step, 8 * cw, y, sc, cr, cg, cb);
                fmt_us(buf, sizeof(buf), total_med);
                heat(total_med, cr, cg, cb);
                rpad(q, buf, col_med, 8 * cw, y, sc, cr, cg, cb);
                y += rh;
            }
            y += 4;

            // Entities + draw stats
            {
                size_t draw_count = q->size();
                char buf[128];
                snprintf(buf, sizeof(buf), "entities:%u  +%u/-%u  pending:%zu  draws:%zu",
                    pm.entity_count(), pm.frame_spawns(), pm.frame_removes(),
                    pm.remove_pending(), draw_count);
                push_str(q, buf, x0, y, sc, 180, 200, 255);
                y += rh;
            }

            // Pool sizes
            for (auto& s : sizes) {
                char buf[48];
                snprintf(buf, sizeof(buf), "%s:%zu", s.label.c_str(), s.fn());
                push_str(q, buf, x0, y, sc, 160, 160, 220);
                y += rh;
            }

            // Custom stats
            for (auto& s : stats) {
                char val[96];
                s.fn(val, sizeof(val));
                char buf[128];
                snprintf(buf, sizeof(buf), "%s: %s", s.label.c_str(), val);
                push_str(q, buf, x0, y, sc, 220, 200, 140);
                y += rh;
            }
        });
    }

private:
    float fps = 0, frame_ms = 0, fps_accum = 0;
    int fps_frames = 0;

    static constexpr size_t RING_SIZE = 64;

    struct TimingRing {
        uint64_t samples[RING_SIZE] = {};
        size_t write = 0;
        size_t count = 0;

        void push(uint64_t us) {
            samples[write % RING_SIZE] = us;
            write++;
            if (count < RING_SIZE) count++;
        }

        uint64_t median() const {
            if (count == 0) return 0;
            size_t n = count;
            uint64_t tmp[RING_SIZE];
            for (size_t i = 0; i < n; i++)
                tmp[i] = samples[(write - n + i) % RING_SIZE];
            for (size_t i = 1; i < n; i++) {
                uint64_t key = tmp[i];
                size_t j = i;
                while (j > 0 && tmp[j - 1] > key) { tmp[j] = tmp[j - 1]; j--; }
                tmp[j] = key;
            }
            return tmp[n / 2];
        }
    };

    std::unordered_map<NameId, TimingRing> rings;

    void reset(Pm& pm) {
        rings.clear();
        pm.reset_task_stats();
        fps = 0; frame_ms = 0;
        fps_accum = 0; fps_frames = 0;
    }

    static void fmt_us(char* buf, int n, uint64_t us) {
        if (us >= 1000)
            snprintf(buf, n, "%llu.%01llu ms",
                (unsigned long long)(us / 1000),
                (unsigned long long)((us % 1000) / 100));
        else
            snprintf(buf, n, "%llu us", (unsigned long long)us);
    }

    static void heat(uint64_t us, uint8_t& r, uint8_t& g, uint8_t& b) {
        if      (us < 50)  { r = 120; g = 200; b = 120; }
        else if (us < 200) { r = 200; g = 200; b = 100; }
        else if (us < 500) { r = 240; g = 160; b = 60;  }
        else               { r = 255; g = 80;  b = 80;  }
    }

    static void rpad(DrawQueue* q, const char* text, int col_x, int col_w,
                     int y, int sc, uint8_t r, uint8_t g, uint8_t b) {
        int text_len = (int)strlen(text);
        int cw = 4 * sc;
        int max_chars = col_w / cw;
        int pad_chars = max_chars - text_len;
        if (pad_chars < 0) pad_chars = 0;
        int x = col_x + pad_chars * cw;
        push_str(q, text, x, y, sc, r, g, b, max_chars);
    }
};

} // namespace pm