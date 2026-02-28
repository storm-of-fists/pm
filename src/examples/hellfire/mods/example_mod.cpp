// example_mod.cpp — Demo hot-reload mod for hellfire client
//
// Registers a HUD task that draws a text label on screen.
// Rebuild while the game is running to see the change live:
//   cmake --build build --target example_mod

#include "hellfire_common.hpp"
#include "pm_sdl.hpp"

using namespace pm;

static const char *MESSAGE = "EXAMPLE MOD v1";

extern "C" void pm_mod_load(Pm &pm)
{
    pm.schedule("example_mod/hud", Phase::HUD + 9.f, [](TaskContext &ctx) {
        auto *draw = ctx.pm.state<DrawQueue>("draw");
        if (!draw)
            return;
        push_str(draw, MESSAGE, 4, 4, 1, 255, 220, 0);
    });
}

extern "C" void pm_mod_unload(Pm &pm)
{
    pm.unschedule("example_mod/hud");
}