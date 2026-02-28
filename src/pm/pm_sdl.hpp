// pm_sdl.hpp — SDL3 window, renderer, draw queue, and input as pm systems
//
// Provides:
//   DrawRect, DrawQueue       — geometry command buffer
//   KeyQueue                  — SDL key event buffer (std::vector<int>)
//   tiny_font(), push_str()   — pixel font utilities
//   render_draw_queue()       — batched SDL_RenderGeometry submission
//   SdlSystem                 — plain state struct owning window/renderer
//   sdl_init(pm, sdl)         — registers input and render tasks
//
// Usage:
//   auto* sdl = pm.state<SdlSystem>("sdl");
//   sdl->open("Title", W, H);
//   sdl_init(pm, sdl);

#pragma once
#include "pm_core.hpp"
#include <SDL3/SDL.h>
#include <string>
#include <vector>
#ifdef _WIN32
#  include <windows.h>
#else
#  include <unistd.h>
#endif

namespace pm
{

// ─── exe_dir ──────────────────────────────────────────────────────────────────
// Returns the directory containing the running binary, with trailing slash.
// Use as a base for resource paths: exe_dir() + "resources/sprite.png"

inline std::string exe_dir()
{
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
	buf[len] = '\0';
	std::string s(buf);
	auto pos = s.rfind('/');
	return (pos != std::string::npos) ? s.substr(0, pos + 1) : "./";
#endif
}

// ─── Draw Queue ───────────────────────────────────────────────────────────────

struct DrawRect
{
	float x, y, w, h;
	uint8_t r, g, b, a;
};

struct DrawQueue
{
	std::vector<DrawRect> items;
	void push(DrawRect r) { items.push_back(r); }
	void clear() { items.clear(); }
	size_t size() const { return items.size(); }
	auto begin() { return items.begin(); }
	auto end()   { return items.end(); }
	auto begin() const { return items.begin(); }
	auto end()   const { return items.end(); }
};

// ─── Key Queue ────────────────────────────────────────────────────────────────
// Positive value = key down, negative = key up (negated SDL keycode).

using KeyQueue = std::vector<int>;

// ─── Pixel Font ──────────────────────────────────────────────────────────────

inline uint16_t tiny_font(char c)
{
#define G3(a, b, c, d, e) (uint16_t)(((a) << 12) | ((b) << 9) | ((c) << 6) | ((d) << 3) | (e))
	if (c >= '0' && c <= '9')
	{
		const uint16_t D[] = {
			G3(07, 05, 05, 05, 07), G3(02, 06, 02, 02, 07), G3(07, 01, 07, 04, 07), G3(07, 01, 07, 01, 07),
			G3(05, 05, 07, 01, 01), G3(07, 04, 07, 01, 07), G3(07, 04, 07, 05, 07), G3(07, 01, 01, 01, 01),
			G3(07, 05, 07, 05, 07), G3(07, 05, 07, 01, 07)};
		return D[c - '0'];
	}
	if (c >= 'a' && c <= 'z')
	{
		const uint16_t A[] = {
			G3(02, 05, 07, 05, 05), G3(06, 05, 06, 05, 06), G3(03, 04, 04, 04, 03), G3(06, 05, 05, 05, 06),
			G3(07, 04, 06, 04, 07), G3(03, 04, 06, 04, 04), G3(03, 05, 03, 01, 06), G3(04, 04, 06, 05, 05),
			G3(02, 02, 02, 02, 02), G3(01, 01, 01, 05, 02), G3(05, 05, 06, 05, 05), G3(02, 02, 02, 02, 03),
			G3(05, 07, 07, 05, 05), G3(06, 05, 05, 05, 05), G3(02, 05, 05, 05, 02), G3(07, 05, 07, 04, 04),
			G3(03, 05, 03, 01, 01), G3(06, 05, 04, 04, 04), G3(03, 04, 02, 01, 06), G3(07, 02, 02, 02, 02),
			G3(05, 05, 05, 05, 03), G3(05, 05, 05, 02, 02), G3(05, 05, 07, 07, 05), G3(05, 05, 02, 05, 05),
			G3(05, 05, 03, 01, 06), G3(07, 01, 02, 04, 07)};
		return A[c - 'a'];
	}
	switch (c)
	{
	case ':': return G3(00, 02, 00, 02, 00);
	case '.': return G3(00, 00, 00, 00, 02);
	case '-': return G3(00, 00, 07, 00, 00);
	case '_': return G3(00, 00, 00, 00, 07);
	case '>': return G3(04, 02, 01, 02, 04);
	case '<': return G3(01, 02, 04, 02, 01);
	case '/': return G3(01, 01, 02, 04, 04);
	case '|': return G3(02, 02, 02, 02, 02);
	case '!': return G3(02, 02, 02, 00, 02);
	case '+': return G3(00, 02, 07, 02, 00);
	case ' ': return 0;
	default:  return G3(07, 01, 07, 04, 07);
	}
#undef G3
}

inline void push_str(DrawQueue *q, const char *s, int x, int y, int sc,
					 uint8_t r, uint8_t g, uint8_t b, int max_chars = 9999)
{
	for (int i = 0; s[i] && i < max_chars; i++)
	{
		uint16_t gl = tiny_font(s[i]);
		for (int row = 0; row < 5; row++)
			for (int col = 0; col < 3; col++)
				if (gl & (1 << (14 - row * 3 - col)))
					q->push({(float)(x + col * sc), (float)(y + row * sc), (float)sc, (float)sc, r, g, b, 255});
		x += 4 * sc;
	}
}

// ─── Batched Renderer ─────────────────────────────────────────────────────────

inline void render_draw_queue(SDL_Renderer *ren, DrawQueue *q)
{
	if (q->size() == 0)
		return;
	static std::vector<SDL_Vertex> verts;
	static std::vector<int> indices;
	verts.clear();
	indices.clear();
	verts.reserve(q->size() * 4);
	indices.reserve(q->size() * 6);

	for (auto &cmd : *q)
	{
		SDL_FColor c{cmd.r / 255.f, cmd.g / 255.f, cmd.b / 255.f, cmd.a / 255.f};
		int i = (int)verts.size();
		SDL_FPoint p0{cmd.x,         cmd.y};
		SDL_FPoint p1{cmd.x + cmd.w, cmd.y};
		SDL_FPoint p2{cmd.x + cmd.w, cmd.y + cmd.h};
		SDL_FPoint p3{cmd.x,         cmd.y + cmd.h};
		SDL_FPoint uv{0, 0};
		verts.push_back({p0, c, uv});
		verts.push_back({p1, c, uv});
		verts.push_back({p2, c, uv});
		verts.push_back({p3, c, uv});
		indices.insert(indices.end(), {i, i + 1, i + 2, i, i + 2, i + 3});
	}
	SDL_SetRenderDrawBlendMode(ren, SDL_BLENDMODE_BLEND);
	SDL_RenderGeometry(ren, nullptr, verts.data(), (int)verts.size(),
					   indices.data(), (int)indices.size());
}

// ─── SDL System ───────────────────────────────────────────────────────────────

struct SdlSystem
{
	SDL_Window   *window   = nullptr;
	SDL_Renderer *renderer = nullptr;
	struct { uint8_t r = 15, g = 15, b = 25; } clear_color;

	bool open(const char *title, int w, int h,
			  SDL_WindowFlags win_flags = 0, bool vsync = true)
	{
		if (!SDL_Init(SDL_INIT_VIDEO))
			return false;
		window   = SDL_CreateWindow(title, w, h, win_flags);
		renderer = SDL_CreateRenderer(window, nullptr);
		if (window && renderer && vsync)
			SDL_SetRenderVSync(renderer, 1);
		return window && renderer;
	}

	~SdlSystem()
	{
		if (renderer) { SDL_DestroyRenderer(renderer); renderer = nullptr; }
		if (window)   { SDL_DestroyWindow(window);     window   = nullptr; }
		SDL_Quit();
	}
};

// ─── Init ─────────────────────────────────────────────────────────────────────

inline void sdl_init(Pm &pm, SdlSystem *sdl, float input_phase, float render_phase)
{
	auto *draw_q = pm.state<DrawQueue>("draw");
	auto *keys_q = pm.state<KeyQueue>("keys");
	auto *wheel  = pm.state<float>("wheel");

	pm.schedule("sdl/input", input_phase, [keys_q, wheel](Pm& pm) {
		keys_q->clear();
		*wheel = 0.f;
		SDL_Event ev;
		while (SDL_PollEvent(&ev)) {
			if (ev.type == SDL_EVENT_QUIT) pm.quit();
			if (ev.type == SDL_EVENT_KEY_DOWN && !ev.key.repeat)
				keys_q->push_back((int)ev.key.key);
			if (ev.type == SDL_EVENT_KEY_UP)
				keys_q->push_back(-(int)ev.key.key);
			if (ev.type == SDL_EVENT_MOUSE_WHEEL)
				*wheel += ev.wheel.y;
		}
	});

	// render_phase     — clear background + flush DrawQueue (solid rects, text)
	// render_phase+0.5 — open slot for sprite/texture draws (game code)
	// render_phase+1   — present
	pm.schedule("sdl/render", render_phase, [sdl, draw_q](Pm&) {
		SDL_SetRenderDrawColor(sdl->renderer, sdl->clear_color.r, sdl->clear_color.g, sdl->clear_color.b, 255);
		SDL_RenderClear(sdl->renderer);
		render_draw_queue(sdl->renderer, draw_q);
		draw_q->clear();
	});
	pm.schedule("sdl/present", render_phase + 1.f, [sdl](Pm&) {
		SDL_RenderPresent(sdl->renderer);
	});
}

} // namespace pm
