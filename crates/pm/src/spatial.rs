//! Flat uniform spatial grid for broad-phase queries (port of
//! pm_spatial_grid.hpp). Each entity occupies exactly one cell by centre
//! position; queries scan the cells overlapping the query circle's
//! bounding box and filter by exact distance.
//!
//! Intended usage — rebuild every frame:
//! ```ignore
//! grid.clear();
//! for (id, m) in monsters.iter() { grid.insert(id, m.pos); }
//! grid.query(centre, radius, |id, pos| { /* narrow-phase */ });
//! ```
//! Per-cell vectors keep their capacity across `clear`, so steady-state
//! frames allocate nothing.

use crate::id::Id;
use crate::math::Vec2;

#[derive(Default)]
pub struct SpatialGrid {
    cell_sz: f32,
    cols: i32,
    rows: i32,
    cells: Vec<Vec<(Id, Vec2)>>,
}

impl SpatialGrid {
    /// `world_w`/`world_h` bound the space; `cell_sz` is the cell side
    /// length in the same units — tune so a typical query touches 1-4
    /// cells. Positions outside the bounds are clamped to edge cells.
    pub fn new(world_w: f32, world_h: f32, cell_sz: f32) -> Self {
        let cols = (world_w / cell_sz).ceil().max(1.0) as i32;
        let rows = (world_h / cell_sz).ceil().max(1.0) as i32;
        Self {
            cell_sz,
            cols,
            rows,
            cells: vec![Vec::new(); (cols * rows) as usize],
        }
    }

    /// Clear all cells for this frame (retains capacity).
    pub fn clear(&mut self) {
        for c in &mut self.cells {
            c.clear();
        }
    }

    pub fn insert(&mut self, id: Id, pos: Vec2) {
        let cx = ((pos.x / self.cell_sz) as i32).clamp(0, self.cols - 1);
        let cy = ((pos.y / self.cell_sz) as i32).clamp(0, self.rows - 1);
        self.cells[(cy * self.cols + cx) as usize].push((id, pos));
    }

    /// Call `fn(id, pos)` for every entity within `radius` of `centre`
    /// (exact distance — no false positives). The callback may mark or
    /// remove entities elsewhere, but must not touch this grid.
    pub fn query(&self, centre: Vec2, radius: f32, mut f: impl FnMut(Id, Vec2)) {
        let x0 = (((centre.x - radius) / self.cell_sz) as i32).max(0);
        let x1 = (((centre.x + radius) / self.cell_sz) as i32).min(self.cols - 1);
        let y0 = (((centre.y - radius) / self.cell_sz) as i32).max(0);
        let y1 = (((centre.y + radius) / self.cell_sz) as i32).min(self.rows - 1);
        let r2 = radius * radius;
        for cy in y0..=y1 {
            for cx in x0..=x1 {
                for &(id, pos) in &self.cells[(cy * self.cols + cx) as usize] {
                    let d = pos - centre;
                    if d.x * d.x + d.y * d.y <= r2 {
                        f(id, pos);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::math::vec2;

    #[test]
    fn finds_only_entities_in_radius() {
        let mut grid = SpatialGrid::new(900.0, 700.0, 64.0);
        let a = Id::new(0, 0, 1);
        let b = Id::new(0, 0, 2);
        let c = Id::new(0, 0, 3);
        grid.insert(a, vec2(100.0, 100.0));
        grid.insert(b, vec2(130.0, 100.0)); // 30 away
        grid.insert(c, vec2(300.0, 300.0)); // far
        let mut hits = Vec::new();
        grid.query(vec2(100.0, 100.0), 50.0, |id, _| hits.push(id));
        hits.sort_by_key(|id| id.index());
        assert_eq!(hits, vec![a, b]);
    }

    #[test]
    fn clamps_out_of_bounds_and_reuses_capacity() {
        // The cell is clamped to the grid edge, but the stored position
        // stays exact — distance filtering still applies to the real pos.
        let mut grid = SpatialGrid::new(100.0, 100.0, 32.0);
        let a = Id::new(0, 0, 1);
        grid.insert(a, vec2(-50.0, 5000.0));
        let mut count = 0;
        grid.query(vec2(0.0, 100.0), 200.0, |_, _| count += 1);
        assert_eq!(count, 0, "real position is far outside the radius");
        let mut count = 0;
        grid.query(vec2(0.0, 100.0), 5000.0, |_, _| count += 1);
        assert_eq!(count, 1);
        grid.clear();
        let mut count = 0;
        grid.query(vec2(0.0, 100.0), 5000.0, |_, _| count += 1);
        assert_eq!(count, 0);
    }
}
