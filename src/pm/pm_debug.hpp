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
// Usage:
//   auto* debug = pm.state<DebugOverlay>("debug");
//   debug_init(pm, debug, Phase::INPUT, Phase::HUD);
//
// Depends: pm_core.hpp, pm_sdl.hpp

#pragma once
#include "pm_core.hpp"
#include "pm_sdl.hpp"
#include <algorithm>
#include <functional>
#include <vector>
#include <string>
#include <cstdio>
#include <cstring>
#include <unordered_map>

namespace pm {

struct DebugOverlay {
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

	// Internal timing state — not for external use.
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
};

// ─── Helpers (file-local) ────────────────────────────────────────────────────

namespace detail {

inline void fmt_us(char* buf, int n, uint64_t us) {
	if (us >= 1000)
		snprintf(buf, n, "%llu.%01llu ms",
			(unsigned long long)(us / 1000),
			(unsigned long long)((us % 1000) / 100));
	else
		snprintf(buf, n, "%llu us", (unsigned long long)us);
}

inline void heat(uint64_t us, uint8_t& r, uint8_t& g, uint8_t& b) {
	if      (us < 50)  { r = 120; g = 200; b = 120; }
	else if (us < 200) { r = 200; g = 200; b = 100; }
	else if (us < 500) { r = 240; g = 160; b = 60;  }
	else               { r = 255; g = 80;  b = 80;  }
}

inline void rpad(DrawQueue* q, const char* text, int col_x, int col_w,
				 int y, int sc, uint8_t r, uint8_t g, uint8_t b) {
	int text_len = (int)strlen(text);
	int cw = 4 * sc;
	int max_chars = col_w / cw;
	int pad_chars = max_chars - text_len;
	if (pad_chars < 0) pad_chars = 0;
	int x = col_x + pad_chars * cw;
	push_str(q, text, x, y, sc, r, g, b, max_chars);
}

} // namespace detail

// ─── Init ─────────────────────────────────────────────────────────────────────

inline void debug_init(Pm& pm, DebugOverlay* debug, float input_phase, float hud_phase) {
	auto* keys_q = pm.state<KeyQueue>("keys");
	auto* draw_q = pm.state<DrawQueue>("draw");

	pm.schedule("debug/input", input_phase + 3.f, [debug, keys_q](TaskContext& ctx) {
		for (auto key : *keys_q) {
			if (key <= 0) continue;

			if (key == SDLK_TAB) {
				debug->visible = !debug->visible;
				if (debug->visible) {
					debug->rings.clear();
					ctx.pm().reset_task_stats();
					debug->fps = 0; debug->frame_ms = 0;
					debug->fps_accum = 0; debug->fps_frames = 0;
				}
			}

			if (!debug->visible) continue;

			if (key == SDLK_R && (SDL_GetModState() & SDL_KMOD_CTRL)) {
				debug->rings.clear();
				ctx.pm().reset_task_stats();
				debug->fps = 0; debug->frame_ms = 0;
				debug->fps_accum = 0; debug->fps_frames = 0;
			}

			if (key == SDLK_F10)
				ctx.pm().request_step();
		}
	});

	pm.schedule("debug/sample", hud_phase + 4.f, [debug](TaskContext& ctx) {
		if (!debug->visible) return;
		Pm& pm = ctx.pm();

		debug->fps_frames++;
		debug->fps_accum += ctx.dt();
		if (debug->fps_accum >= 0.5f) {
			debug->fps = (float)debug->fps_frames / debug->fps_accum;
			debug->fps_frames = 0;
			debug->fps_accum = 0;
		}
		debug->frame_ms = ctx.dt() * 1000.f;

		for (auto& t : pm.tasks()) {
			if (!t.active) continue;
			debug->rings[t.name].push(t.last_us);
		}
	});

	pm.schedule("debug/draw", hud_phase + 5.f, [debug, draw_q](TaskContext& ctx) {
		if (!debug->visible) return;
		Pm& pm = ctx.pm();
		int sc = 2;
		int cw = 4 * sc;
		int rh = 6 * sc + 2;

		int pad = 8;
		int x0  = pad;
		int y   = pad;

		// Columns: status | priority | task name | step | median | max
		int col_status = x0;
		int col_pri    = x0 + 4  * cw;
		int col_name   = x0 + 10 * cw;
		int col_step   = x0 + 30 * cw;
		int col_med    = col_step + 9 * cw;
		int col_max    = col_med  + 9 * cw;
		int panel_w    = col_max  + 9 * cw + pad;

		auto& all_tasks = pm.tasks();
		int task_rows  = (int)all_tasks.size();
		int fault_rows = pm.faults().empty() ? 0 : 1 + (int)pm.faults().size();

		// All tasks sorted by priority (active and inactive)
		std::vector<size_t> sorted_idx;
		sorted_idx.reserve(task_rows);
		for (size_t i = 0; i < all_tasks.size(); i++) sorted_idx.push_back(i);
		std::sort(sorted_idx.begin(), sorted_idx.end(),
			[&](size_t a, size_t b){ return all_tasks[a].priority < all_tasks[b].priority; });

		int total_rows = 1 + 1                            // fps + controls
					   + 1 + 1 + task_rows                // header + sep + tasks
					   + 1 + 1                            // sep + total
					   + 1                                // entities
					   + (int)debug->sizes.size()
					   + (int)debug->stats.size()
					   + fault_rows
					   + 1;
		int panel_h = pad + total_rows * rh + pad;

		// Background
		draw_q->push({0, 0, (float)panel_w, (float)panel_h, 8, 8, 14, 220});

		// FPS + state
		{
			char buf[96];
			const char* state = "";
			if (ctx.is_paused() && ctx.stepping()) state = "  |step|";
			else if (ctx.is_paused())              state = "  |paused|";
			snprintf(buf, sizeof(buf), "fps:%.0f  dt:%.1f ms  tick:%llu%s",
				debug->fps, debug->frame_ms, (unsigned long long)pm.tick_count(), state);
			push_str(draw_q, buf, x0, y, sc, 140, 220, 140);
			y += rh;
		}

		// Controls hint
		push_str(draw_q, "ctrl+r reset  f10 step", x0, y, sc, 70, 70, 90);
		y += rh;

		// Table header
		push_str(draw_q, "sts",    col_status, y, sc, 120, 120, 140);
		detail::rpad(draw_q, "pri",    col_pri,   5 * cw, y, sc, 120, 120, 140);
		push_str(draw_q, "task",   col_name,   y, sc, 120, 120, 140);
		detail::rpad(draw_q, "step",   col_step,  8 * cw, y, sc, 120, 120, 140);
		detail::rpad(draw_q, "median", col_med,   8 * cw, y, sc, 120, 120, 140);
		detail::rpad(draw_q, "max",    col_max,   8 * cw, y, sc, 120, 120, 140);
		y += rh;

		// Separator
		draw_q->push({(float)x0, (float)y, (float)(panel_w - 2 * pad), 1, 60, 60, 80, 200});
		y += 4;

		// Task rows (all tasks, sorted by priority)
		uint64_t total_step = 0, total_med = 0;
		for (size_t idx : sorted_idx) {
			auto& t = all_tasks[idx];
			const char* name = pm.name_str(t.name);
			bool running = t.active && !(t.pauseable && ctx.is_paused());

			// Status
			const char* sts; uint8_t sr, sg, sb;
			if      (!t.active)                          { sts = "er"; sr=255; sg=80;  sb=80;  }
			else if (t.pauseable && ctx.is_paused())     { sts = "ps"; sr=220; sg=160; sb=60;  }
			else                                         { sts = "ok"; sr=120; sg=200; sb=120; }
			push_str(draw_q, sts, col_status, y, sc, sr, sg, sb);

			// Priority
			char pribuf[12];
			if (t.priority == (float)(int)t.priority)
				snprintf(pribuf, sizeof(pribuf), "%d", (int)t.priority);
			else
				snprintf(pribuf, sizeof(pribuf), "%.1f", t.priority);
			detail::rpad(draw_q, pribuf, col_pri, 5 * cw, y, sc, 100, 100, 130);

			// Name (dim if stopped)
			push_str(draw_q, name, col_name, y, sc,
				t.active ? 180 : 100,
				t.active ? 180 : 80,
				t.active ? 200 : 80, 19);

			// Timing
			char buf[16]; uint8_t cr, cg, cb;

			detail::fmt_us(buf, sizeof(buf), t.last_us);
			if (running) { detail::heat(t.last_us, cr, cg, cb); total_step += t.last_us; }
			else         { cr = 60; cg = 60; cb = 70; }
			detail::rpad(draw_q, buf, col_step, 8 * cw, y, sc, cr, cg, cb);

			auto it = debug->rings.find(t.name);
			uint64_t med = (it != debug->rings.end()) ? it->second.median() : 0;
			detail::fmt_us(buf, sizeof(buf), med);
			if (running) { detail::heat(med, cr, cg, cb); total_med += med; }
			else         { cr = 60; cg = 60; cb = 70; }
			detail::rpad(draw_q, buf, col_med, 8 * cw, y, sc, cr, cg, cb);

			detail::fmt_us(buf, sizeof(buf), t.max_us);
			if (running) detail::heat(t.max_us, cr, cg, cb);
			else         { cr = 60; cg = 60; cb = 70; }
			detail::rpad(draw_q, buf, col_max, 8 * cw, y, sc, cr, cg, cb);

			y += rh;
		}

		// Total row (active tasks only)
		draw_q->push({(float)x0, (float)y, (float)(panel_w - 2 * pad), 1, 60, 60, 80, 200});
		y += 4;
		{
			push_str(draw_q, "total", col_name, y, sc, 150, 150, 180);
			char buf[16]; uint8_t cr, cg, cb;
			detail::fmt_us(buf, sizeof(buf), total_step);
			detail::heat(total_step, cr, cg, cb);
			detail::rpad(draw_q, buf, col_step, 8 * cw, y, sc, cr, cg, cb);
			detail::fmt_us(buf, sizeof(buf), total_med);
			detail::heat(total_med, cr, cg, cb);
			detail::rpad(draw_q, buf, col_med, 8 * cw, y, sc, cr, cg, cb);
			y += rh;
		}
		y += 4;

		// Entities + draw stats
		{
			char buf[128];
			snprintf(buf, sizeof(buf), "entities:%u  +%u/-%u  draws:%zu",
				pm.entity_count(), pm.frame_spawns(), pm.frame_removes(),
				draw_q->size());
			push_str(draw_q, buf, x0, y, sc, 180, 200, 255);
			y += rh;
		}

		// Pool sizes
		for (auto& s : debug->sizes) {
			char buf[48];
			snprintf(buf, sizeof(buf), "%s:%zu", s.label.c_str(), s.fn());
			push_str(draw_q, buf, x0, y, sc, 160, 160, 220);
			y += rh;
		}

		// Custom stats
		for (auto& s : debug->stats) {
			char val[96]; s.fn(val, sizeof(val));
			char buf[128];
			snprintf(buf, sizeof(buf), "%s: %s", s.label.c_str(), val);
			push_str(draw_q, buf, x0, y, sc, 220, 200, 140);
			y += rh;
		}

		// Fault list
		if (!pm.faults().empty()) {
			push_str(draw_q, "faults:", x0, y, sc, 255, 80, 80);
			y += rh;
			for (auto& f : pm.faults()) {
				push_str(draw_q, f.c_str(), x0 + cw, y, sc, 255, 130, 110, 38);
				y += rh;
			}
		}
	});
}

} // namespace pm
