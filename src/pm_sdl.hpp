// pm_sdl.hpp — SDL2 window, renderer, draw queue, and input as pm systems
//
// Provides:
//   DrawRect, DrawQueue       — geometry command buffer
//   tiny_font(), push_str()   — pixel font utilities
//   render_draw_queue()       — batched SDL_RenderGeometry submission
//   SdlSystem                 — System owning window/renderer/input/render tasks
//
// Register in main: pm.sys<SdlSystem>("sdl")->open("Title", W, H);

#pragma once
#include "pm_core.hpp"
#include <SDL2/SDL.h>
#include <vector>

namespace pm
{

    // ─── Draw Queue ──────────────────────────────────────────────────────────────

    struct DrawRect
    {
        float x, y, w, h;
        uint8_t r, g, b, a;
    };
    using DrawQueue = Queue<DrawRect>;

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
        case ':':
            return G3(00, 02, 00, 02, 00);
        case '.':
            return G3(00, 00, 00, 00, 02);
        case '-':
            return G3(00, 00, 07, 00, 00);
        case '_':
            return G3(00, 00, 00, 00, 07);
        case '>':
            return G3(04, 02, 01, 02, 04);
        case '<':
            return G3(01, 02, 04, 02, 01);
        case '/':
            return G3(01, 01, 02, 04, 04);
        case '|':
            return G3(02, 02, 02, 02, 02);
        case '!':
            return G3(02, 02, 02, 00, 02);
        case '+':
            return G3(00, 02, 07, 02, 00);
        case ' ':
            return 0;
        default:
            return G3(07, 01, 07, 04, 07);
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
            SDL_Color c{cmd.r, cmd.g, cmd.b, cmd.a};
            int i = (int)verts.size();
            verts.push_back({{cmd.x, cmd.y}, c, {0, 0}});
            verts.push_back({{cmd.x + cmd.w, cmd.y}, c, {0, 0}});
            verts.push_back({{cmd.x + cmd.w, cmd.y + cmd.h}, c, {0, 0}});
            verts.push_back({{cmd.x, cmd.y + cmd.h}, c, {0, 0}});
            indices.insert(indices.end(), {i, i + 1, i + 2, i, i + 2, i + 3});
        }
        SDL_SetRenderDrawBlendMode(ren, SDL_BLENDMODE_BLEND);
        SDL_RenderGeometry(ren, nullptr, verts.data(), (int)verts.size(),
                           indices.data(), (int)indices.size());
    }

    // ─── SDL System ───────────────────────────────────────────────────────────────

    class SdlSystem : public System
    {
    public:
        SDL_Window *window = nullptr;
        SDL_Renderer *renderer = nullptr;
        struct
        {
            uint8_t r = 15, g = 15, b = 25;
        } clear_color;

        bool open(const char *title, int w, int h,
                  uint32_t flags = SDL_RENDERER_ACCELERATED | SDL_RENDERER_PRESENTVSYNC)
        {
            if (SDL_Init(SDL_INIT_VIDEO) != 0)
                return false;
            window = SDL_CreateWindow(title, SDL_WINDOWPOS_CENTERED, SDL_WINDOWPOS_CENTERED, w, h, 0);
            renderer = SDL_CreateRenderer(window, -1, flags);
            return window && renderer;
        }

        void shutdown(Pm &) override
        {
            if (renderer)
            {
                SDL_DestroyRenderer(renderer);
                renderer = nullptr;
            }
            if (window)
            {
                SDL_DestroyWindow(window);
                window = nullptr;
            }
            SDL_Quit();
        }

        void initialize(Pm &pm) override
        {
            auto draw_q = pm.queue<DrawRect>("draw");

            pm.schedule("sdl/input", Pm::Phase::INPUT, [](Pm &pm)
                        {
            SDL_Event ev;
            while (SDL_PollEvent(&ev)) {
                if (ev.type == SDL_QUIT) pm.quit();
                if (ev.type == SDL_KEYDOWN && ev.key.repeat == 0)
                    pm.queue<int>("keys")->push(ev.key.keysym.sym);
                if (ev.type == SDL_KEYUP)
                    pm.queue<int>("keys")->push(-(int)ev.key.keysym.sym);
            } });

            pm.schedule("sdl/render", Pm::Phase::RENDER, [this, draw_q](Pm &pm)
                        {
            SDL_SetRenderDrawColor(renderer, clear_color.r, clear_color.g, clear_color.b, 255);
            SDL_RenderClear(renderer);
            render_draw_queue(renderer, draw_q);
            SDL_RenderPresent(renderer); });
        }
    };

} // namespace pm