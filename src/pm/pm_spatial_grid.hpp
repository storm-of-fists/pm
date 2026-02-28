// pm_spatial_grid.hpp — Flat uniform spatial grid for broad-phase queries
//
// Divides a bounded 2D world into a fixed grid of cells.
// Each entity occupies exactly one cell (by centre position).
// Queries scan all cells that overlap the query circle's bounding box
// and filter by exact Euclidean distance.
//
// Intended usage — rebuild every frame:
//   grid.clear();
//   for each entity: grid.insert(id, pos);
//   grid.query(centre, radius, [](Id id, Vec2 pos){ /* narrow-phase */ });
//
// Vectors inside each cell retain their heap capacity across clear() calls,
// so there are no allocations after the first few frames.
//
// Depends: pm_core.hpp, pm_math.hpp

#pragma once
#include "pm_core.hpp"
#include "pm_math.hpp"
#include <vector>
#include <utility>
#include <algorithm>
#include <cmath>

namespace pm {

// =============================================================================
// SpatialGrid
// =============================================================================
struct SpatialGrid {
	int cell_sz = 64;
	int cols    = 0;
	int rows    = 0;
	std::vector<std::vector<std::pair<Id, Vec2>>> cells;

	SpatialGrid() = default;

	// world_w / world_h — extent of the simulation space (pixels, units, etc.)
	// cell_sz           — side length of one grid cell in the same units.
	//                     Tune so that a typical query touches 1–4 cells.
	SpatialGrid(float world_w, float world_h, int csz = 64)
		: cell_sz(csz)
		, cols(std::max(1, (int)std::ceil(world_w / (float)csz)))
		, rows(std::max(1, (int)std::ceil(world_h / (float)csz)))
		, cells(cols * rows)
	{}

	// Clear all cells for this frame (retains vector capacity).
	void clear() {
		for (auto& c : cells) c.clear();
	}

	// Insert entity at pos. Positions outside the grid are clamped to the edge.
	void insert(Id id, Vec2 pos) {
		int cx = std::clamp((int)(pos.x / cell_sz), 0, cols - 1);
		int cy = std::clamp((int)(pos.y / cell_sz), 0, rows - 1);
		cells[cy * cols + cx].push_back({id, pos});
	}

	// Call fn(Id, Vec2) for every entity whose centre lies within radius of
	// centre. The exact Euclidean distance is checked (not just the bounding
	// box), so no false positives reach fn.
	// fn may remove or mark entities; it must not call insert() or clear().
	template<typename F>
	void query(Vec2 centre, float radius, F&& fn) const {
		int x0 = std::max(0,        (int)((centre.x - radius) / cell_sz));
		int x1 = std::min(cols - 1, (int)((centre.x + radius) / cell_sz));
		int y0 = std::max(0,        (int)((centre.y - radius) / cell_sz));
		int y1 = std::min(rows - 1, (int)((centre.y + radius) / cell_sz));
		float r2 = radius * radius;
		for (int cy = y0; cy <= y1; cy++) {
			for (int cx = x0; cx <= x1; cx++) {
				for (auto& [id, pos] : cells[cy * cols + cx]) {
					float dx = pos.x - centre.x, dy = pos.y - centre.y;
					if (dx * dx + dy * dy <= r2)
						fn(id, pos);
				}
			}
		}
	}
};

} // namespace pm
