// pm_sprite.hpp — PNG sprite loading, hot-reload, and centered drawing
//
// Provides:
//   Sprite                   — texture wrapper with load/reload/draw_centered/check_mtime
//
// Depends on: pm_sdl.hpp (SdlSystem), SDL3_image (-lSDL3_image)
//
// Usage:
//   Sprite s;
//   s.load(sdl->renderer, exe_dir() + "resources/player.png");
//
//   // In render task (Phase::RENDER + 0.5):
//   s.draw_centered(ren, x, y, display_width);
//
//   // Hot-reload: schedule a task at ~1Hz to call reload if file changed:
//   if (s.changed()) s.reload(ren);

#pragma once
#include "pm_sdl.hpp"
#include <SDL3_image/SDL_image.h>
#include <sys/stat.h>
#include <cstdio>
#include <string>

namespace pm
{

// ─── File mtime helper ────────────────────────────────────────────────────────

inline time_t sprite_file_mtime(const char *path)
{
	struct stat st{};
	return (stat(path, &st) == 0) ? st.st_mtime : 0;
}

// ─── Sprite ───────────────────────────────────────────────────────────────────
//
// Plain value type. Does NOT auto-destroy its texture — textures are owned
// by the SDL renderer and freed when the renderer is destroyed. Call
// reload() explicitly to swap in a new texture (it destroys the old one first).

struct Sprite
{
	SDL_Texture *tex  = nullptr;
	float        w    = 0;
	float        h    = 0;
	std::string  path;
	time_t       mtime = 0;

	// Load from path. Returns true on success.
	bool load(SDL_Renderer *ren, const std::string &file_path)
	{
		path = file_path;
		return reload(ren);
	}

	// Destroy old texture and reload from the stored path.
	// Loads new surface first — if the load fails the old texture is kept intact.
	bool reload(SDL_Renderer *ren)
	{
		SDL_Surface *surf = IMG_Load(path.c_str());
		if (!surf)
		{
			printf("[sprite] warning: could not load '%s' (%s)\n", path.c_str(), SDL_GetError());
			return false;  // old texture unchanged; will retry on next check
		}
		if (tex) { SDL_DestroyTexture(tex); tex = nullptr; w = h = 0; }
		tex = SDL_CreateTextureFromSurface(ren, surf);
		SDL_DestroySurface(surf);
		if (tex) SDL_GetTextureSize(tex, &w, &h);
		mtime = sprite_file_mtime(path.c_str());
		return tex != nullptr;
	}

	// Returns true if the file on disk is newer than when it was last loaded.
	bool changed() const
	{
		return !path.empty() && sprite_file_mtime(path.c_str()) != mtime;
	}

	// Draw centered at (cx, cy), scaled to display_w wide with proportional height.
	void draw_centered(SDL_Renderer *ren, float cx, float cy, float display_w) const
	{
		if (!tex) return;
		float display_h = (w > 0) ? display_w * (h / w) : display_w;
		SDL_FRect dst = {
			cx - display_w  * 0.5f,
			cy - display_h * 0.5f,
			display_w,
			display_h
		};
		SDL_RenderTexture(ren, tex, nullptr, &dst);
	}

	explicit operator bool() const { return tex != nullptr; }
};

} // namespace pm