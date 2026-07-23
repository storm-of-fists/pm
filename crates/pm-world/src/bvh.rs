//! Dynamic AABB tree — the engine's broadphase.
//!
//! Every entry ("proxy") is a leaf holding an [`Aabb`], a category
//! bitmask, and an [`Id`]. Internal nodes hold the union of their two
//! children's boxes and the OR of their bits, so a query descends only
//! into subtrees whose box AND bits match — `O(log n)` per query instead
//! of a scan.
//!
//! Nodes live in one flat `Vec`; parent/child links are `u32` indices
//! and removed nodes go on a free list threaded through the same array
//! (the `parent` field doubles as the free-list next). No boxes, no Rc —
//! the same index-linked style as the entity pools.
//!
//! **Fat boxes**: `insert`/`update` inflate the box you pass by the
//! tree's `margin`. While an entry's tight box stays inside its stored
//! fat box, [`DynamicTree::update`] is a no-op — a mover only re-inserts
//! after travelling `margin`, and idle entries cost nothing. Pick margin
//! ≈ typical per-tick travel × a few ticks.
//!
//! Serves PRESENT-time queries only: lag-comp rewinds scan the history
//! ring linearly (decided 2026-07-19 — versioning or rewinding the tree
//! would copy structure every tick for a scan that measures fine
//! linear).
//!
//! # Whose design this is (decided 2026-07-19/20)
//!
//! A deliberate port of the Box2D-v3 / Box3D `b3DynamicTree` — Erin
//! Catto's lineage (b2DynamicTree 2007 → v3 rewrite → Box3D 2026), the
//! same structure a five-engine survey found at the bottom of Chaos and
//! Jolt. Taken as-is: the flat-array/free-list node pool, the
//! `b3FindBestSibling` branch-and-bound insertion (minimize added
//! SURFACE AREA: direct cost + growth inflicted on every ancestor,
//! centroid distance as tie-break — the part that makes incrementally
//! built trees good, so ported faithfully), fat AABBs, and v3's
//! category bits OR'd up internal nodes for subtree pruning.
//!
//! Deliberately NOT taken: v3's surface-area rotations — `balance`
//! uses the older Box2D-v2 height rule (~60 lines vs ~300, teachable,
//! height bounded by test); the swap-in trigger is written on it. Also
//! left behind: arena allocators, enlarged-AABB rebuild machinery,
//! tree serialization — scale machinery pm doesn't need yet.
//!
//! # Why not the structures other engines use
//!
//! - **Quake's BSP + areanodes**: baked at compile time for a static
//!   world; pm's world is dynamic by ambition (buildings will move).
//! - **Source's voxel hash + Havok SAP**: SAP's strength is
//!   maintaining PERSISTENT overlap pairs for a solver's islands. pm
//!   asks questions (nearest / crossed / touching) — there is no pair
//!   list to maintain, so SAP's bookkeeping buys nothing.
//! - **Uniform grids** ([`SpatialGrid`](crate::SpatialGrid) stays as
//!   teaching code): win only for bounded worlds and uniform entry
//!   sizes — one cell size cannot serve a hog and a tower.
//! - **Octrees**: better than grids for mixed sizes, but cell
//!   boundaries still force re-bucketing churn; the fat margin makes
//!   the equivalent laziness explicit and cheaper.
//! - **Jolt/Box2D's static/dynamic TREE SPLIT**: real, deferred — one
//!   tree is fine while everything in it moves. The split earns its
//!   keep when a large static set (buildings-as-pool) would make
//!   dynamic re-insertions churn a shared tree; queued with that work.
//!
//! Broadphase contract, worth stating once: the tree may over-report
//! (fat boxes, box-vs-shape gap) and callers ALWAYS re-run their exact
//! narrow test on candidates. It must never under-report — pinned by
//! the fuzz test against a linear scan.
//!
//! ```
//! use pm::{Aabb, DynamicTree, Id, vec3};
//!
//! let mut tree = DynamicTree::new(0.5);
//! const HOG: u64 = 1 << 0;
//! const HELI: u64 = 1 << 1;
//! let hog = tree.insert(Aabb::around(vec3(10.0, 0.0, 10.0), vec3(1.0, 1.0, 1.0)), HOG, Id::new(0, 0, 1));
//! let _heli = tree.insert(Aabb::around(vec3(10.0, 30.0, 10.0), vec3(2.0, 1.0, 2.0)), HELI, Id::new(0, 0, 2));
//!
//! // Ground-level query masked to hogs: the heli subtree is pruned.
//! let mut hits = Vec::new();
//! tree.query(Aabb::around(vec3(10.0, 0.0, 10.0), vec3(5.0, 2.0, 5.0)), HOG, |id| hits.push(id));
//! assert_eq!(hits, vec![Id::new(0, 0, 1)]);
//!
//! // Small move inside the fat margin: no tree surgery.
//! assert!(!tree.update(hog, Aabb::around(vec3(10.2, 0.0, 10.0), vec3(1.0, 1.0, 1.0))));
//! ```

use crate::id::Id;
use crate::math::Vec3;

/// Axis-aligned bounding box: `min`/`max` corner per axis. The
/// conservative stand-in the tree stores for any exact shape; overlap
/// is a per-axis interval test, ~6 compares.
#[derive(Clone, Copy, PartialEq, Default, Debug)]
pub struct Aabb {
    pub min: Vec3,
    pub max: Vec3,
}

impl Aabb {
    pub fn new(min: Vec3, max: Vec3) -> Aabb {
        Aabb { min, max }
    }

    /// Box centred on `c` with half-extents `half` per axis.
    pub fn around(c: Vec3, half: Vec3) -> Aabb {
        Aabb { min: c - half, max: c + half }
    }

    /// Smallest box containing both.
    pub fn union(self, o: Aabb) -> Aabb {
        Aabb {
            min: Vec3 {
                x: self.min.x.min(o.min.x),
                y: self.min.y.min(o.min.y),
                z: self.min.z.min(o.min.z),
            },
            max: Vec3 {
                x: self.max.x.max(o.max.x),
                y: self.max.y.max(o.max.y),
                z: self.max.z.max(o.max.z),
            },
        }
    }

    pub fn overlaps(self, o: Aabb) -> bool {
        self.min.x <= o.max.x
            && self.max.x >= o.min.x
            && self.min.y <= o.max.y
            && self.max.y >= o.min.y
            && self.min.z <= o.max.z
            && self.max.z >= o.min.z
    }

    /// True when `o` fits entirely inside `self` — the fat-margin test.
    pub fn contains(self, o: Aabb) -> bool {
        self.min.x <= o.min.x
            && self.min.y <= o.min.y
            && self.min.z <= o.min.z
            && o.max.x <= self.max.x
            && o.max.y <= self.max.y
            && o.max.z <= self.max.z
    }

    /// Grown by `r` on every face.
    pub fn expand(self, r: f32) -> Aabb {
        let e = Vec3 { x: r, y: r, z: r };
        Aabb { min: self.min - e, max: self.max + e }
    }

    pub fn center(self) -> Vec3 {
        (self.min + self.max) * 0.5
    }

    /// Surface area — the insertion cost metric (SAH): siblings are
    /// chosen to minimise total added surface, which minimises the
    /// chance an unrelated query has to visit the subtree.
    pub fn area(self) -> f32 {
        let w = self.max - self.min;
        2.0 * (w.x * w.y + w.y * w.z + w.z * w.x)
    }

    /// Does the segment `p1 → p1 + t·(p2−p1)`, `t ∈ [0, max_t]`, hit the
    /// box grown by `r`? Slab test; degenerate axes fall back to an
    /// interval check.
    pub fn hit_by(self, p1: Vec3, p2: Vec3, r: f32, max_t: f32) -> bool {
        let b = self.expand(r);
        let d = p2 - p1;
        let (mut t0, mut t1) = (0.0f32, max_t);
        for axis in 0..3 {
            let (p, dir, min, max) = match axis {
                0 => (p1.x, d.x, b.min.x, b.max.x),
                1 => (p1.y, d.y, b.min.y, b.max.y),
                _ => (p1.z, d.z, b.min.z, b.max.z),
            };
            if dir.abs() < 1e-8 {
                if p < min || p > max {
                    return false;
                }
            } else {
                let inv = 1.0 / dir;
                let (a, b) = ((min - p) * inv, (max - p) * inv);
                let (near, far) = if a < b { (a, b) } else { (b, a) };
                t0 = t0.max(near);
                t1 = t1.min(far);
                if t0 > t1 {
                    return false;
                }
            }
        }
        true
    }
}

const NULL: u32 = u32::MAX;

#[derive(Clone, Copy)]
struct Node {
    /// Leaf: the FAT box. Internal: union of children.
    aabb: Aabb,
    /// Leaf: category bits. Internal: OR of children — lets a masked
    /// query prune whole subtrees that contain nothing it wants.
    bits: u64,
    /// Leaf payload; unused on internal nodes.
    id: Id,
    /// Parent index; doubles as free-list next while the node is free.
    parent: u32,
    child1: u32,
    child2: u32,
    /// Leaves are 0, internals 1 + max(children); -1 marks a free node.
    height: i32,
}

impl Node {
    fn leaf(&self) -> bool {
        self.child1 == NULL
    }
}

/// Incrementally-maintained bounding volume hierarchy over `Id`-tagged
/// fat AABBs. See the module docs for the model; hogs' `WorldIndex`
/// (the one-query-seam pattern) is where it slots into a game.
pub struct DynamicTree {
    nodes: Vec<Node>,
    root: u32,
    free: u32,
    margin: f32,
    leaves: usize,
}

impl DynamicTree {
    /// `margin` is how much every stored box is inflated on each face —
    /// the distance an entry can move before `update` touches the tree.
    pub fn new(margin: f32) -> DynamicTree {
        DynamicTree { nodes: Vec::new(), root: NULL, free: NULL, margin, leaves: 0 }
    }

    /// Number of live entries (leaves).
    pub fn len(&self) -> usize {
        self.leaves
    }

    pub fn is_empty(&self) -> bool {
        self.leaves == 0
    }

    /// Add an entry; `aabb` is the TIGHT box (the tree fattens it).
    /// Returns the proxy index used by `update`/`remove`.
    pub fn insert(&mut self, aabb: Aabb, bits: u64, id: Id) -> u32 {
        let leaf = self.alloc();
        self.nodes[leaf as usize] = Node {
            aabb: aabb.expand(self.margin),
            bits,
            id,
            parent: NULL,
            child1: NULL,
            child2: NULL,
            height: 0,
        };
        self.insert_leaf(leaf);
        self.leaves += 1;
        leaf
    }

    pub fn remove(&mut self, proxy: u32) {
        debug_assert!(self.nodes[proxy as usize].leaf());
        self.remove_leaf(proxy);
        self.dealloc(proxy);
        self.leaves -= 1;
    }

    /// Move an entry to a new TIGHT box. While the box still fits in the
    /// stored fat box this is a no-op; past the margin the leaf is
    /// re-inserted (and re-fattened). Returns whether the tree changed.
    pub fn update(&mut self, proxy: u32, aabb: Aabb) -> bool {
        debug_assert!(self.nodes[proxy as usize].leaf());
        if self.nodes[proxy as usize].aabb.contains(aabb) {
            return false;
        }
        self.remove_leaf(proxy);
        self.nodes[proxy as usize].aabb = aabb.expand(self.margin);
        self.insert_leaf(proxy);
        true
    }

    /// The `Id` a proxy was inserted with.
    pub fn id(&self, proxy: u32) -> Id {
        self.nodes[proxy as usize].id
    }

    /// The stored (fat) box of a proxy.
    pub fn fat(&self, proxy: u32) -> Aabb {
        self.nodes[proxy as usize].aabb
    }

    /// Call `f(id)` for every entry whose fat box overlaps `aabb` and
    /// whose bits intersect `mask`. Subtrees whose combined bits miss
    /// the mask are pruned without visiting.
    pub fn query(&self, aabb: Aabb, mask: u64, mut f: impl FnMut(Id)) {
        if self.root == NULL {
            return;
        }
        let mut stack = Vec::with_capacity(64);
        stack.push(self.root);
        while let Some(i) = stack.pop() {
            let n = &self.nodes[i as usize];
            if n.bits & mask == 0 || !n.aabb.overlaps(aabb) {
                continue;
            }
            if n.leaf() {
                f(n.id);
            } else {
                stack.push(n.child1);
                stack.push(n.child2);
            }
        }
    }

    /// Sweep the segment `p1 → p2` (a sphere of `radius` if non-zero)
    /// through the tree. `f(id)` runs for every candidate whose fat box
    /// the swept segment crosses and must return the new maximum
    /// fraction: `1.0` = keep looking everywhere, `t` = clip the search
    /// to `p1 + t·(p2−p1)` (return your exact hit fraction to shrink the
    /// search), `0.0` = stop. Exact narrow-phase is the caller's job —
    /// the tree only vouches for boxes.
    pub fn cast(&self, p1: Vec3, p2: Vec3, radius: f32, mask: u64, mut f: impl FnMut(Id) -> f32) {
        if self.root == NULL {
            return;
        }
        let mut max_t = 1.0f32;
        let mut stack = Vec::with_capacity(64);
        stack.push(self.root);
        while let Some(i) = stack.pop() {
            let n = &self.nodes[i as usize];
            if n.bits & mask == 0 || !n.aabb.hit_by(p1, p2, radius, max_t) {
                continue;
            }
            if n.leaf() {
                max_t = f(n.id).min(max_t);
                if max_t <= 0.0 {
                    return;
                }
            } else {
                stack.push(n.child1);
                stack.push(n.child2);
            }
        }
    }

    /// Visit every node's box (leaves and internals) — for debug draws.
    pub fn walk(&self, mut f: impl FnMut(Aabb, bool)) {
        if self.root == NULL {
            return;
        }
        let mut stack = vec![self.root];
        while let Some(i) = stack.pop() {
            let n = &self.nodes[i as usize];
            f(n.aabb, n.leaf());
            if !n.leaf() {
                stack.push(n.child1);
                stack.push(n.child2);
            }
        }
    }

    /// Height of the root (leaves are 0). Balanced trees stay near
    /// `log2(len)`; use with [`DynamicTree::validate`] in tests.
    pub fn height(&self) -> i32 {
        if self.root == NULL { 0 } else { self.nodes[self.root as usize].height }
    }

    // --- allocation ------------------------------------------------------

    fn alloc(&mut self) -> u32 {
        if self.free == NULL {
            self.nodes.push(Node {
                aabb: Aabb::default(),
                bits: 0,
                id: Id(0),
                parent: NULL,
                child1: NULL,
                child2: NULL,
                height: -1,
            });
            (self.nodes.len() - 1) as u32
        } else {
            let i = self.free;
            self.free = self.nodes[i as usize].parent;
            i
        }
    }

    fn dealloc(&mut self, i: u32) {
        self.nodes[i as usize].parent = self.free;
        self.nodes[i as usize].height = -1;
        self.free = i;
    }

    // --- structure -------------------------------------------------------

    /// Pick the existing node the new leaf should pair with: descend one
    /// greedy path from the root, tracking for each candidate the cost
    /// of pairing there (its box grown to fit the leaf) plus the growth
    /// inflicted on every ancestor, and stop when no child's lower bound
    /// can beat the best seen. (Box3D's branch-and-bound
    /// `b3FindBestSibling`, surface-area metric.)
    fn best_sibling(&self, box_d: Aabb) -> u32 {
        let nodes = &self.nodes;
        let center_d = box_d.center();
        let area_d = box_d.area();

        let mut index = self.root;
        let root_box = nodes[index as usize].aabb;
        let mut area_base = root_box.area();
        let mut direct_cost = root_box.union(box_d).area();
        let mut inherited = 0.0f32;

        let mut best = index;
        let mut best_cost = direct_cost;

        while !nodes[index as usize].leaf() {
            let cost = direct_cost + inherited;
            if cost < best_cost {
                best = index;
                best_cost = cost;
            }
            inherited += direct_cost - area_base;

            let c1 = nodes[index as usize].child1;
            let c2 = nodes[index as usize].child2;
            let leaf1 = nodes[c1 as usize].leaf();
            let leaf2 = nodes[c2 as usize].leaf();

            let box1 = nodes[c1 as usize].aabb;
            let direct1 = box1.union(box_d).area();
            let mut lower1 = f32::MAX;
            let mut area1 = 0.0;
            if leaf1 {
                let cost1 = direct1 + inherited;
                if cost1 < best_cost {
                    best = c1;
                    best_cost = cost1;
                }
            } else {
                area1 = box1.area();
                lower1 = inherited + direct1 + (area_d - area1).min(0.0);
            }

            let box2 = nodes[c2 as usize].aabb;
            let direct2 = box2.union(box_d).area();
            let mut lower2 = f32::MAX;
            let mut area2 = 0.0;
            if leaf2 {
                let cost2 = direct2 + inherited;
                if cost2 < best_cost {
                    best = c2;
                    best_cost = cost2;
                }
            } else {
                area2 = box2.area();
                lower2 = inherited + direct2 + (area_d - area2).min(0.0);
            }

            if (leaf1 && leaf2) || (best_cost <= lower1 && best_cost <= lower2) {
                break;
            }

            if lower1 == lower2 && !leaf1 {
                // Both children fully contain the leaf — surface area
                // can't break the tie, fall back to centroid distance.
                let d1 = box1.center() - center_d;
                let d2 = box2.center() - center_d;
                lower1 = d1.dot(d1);
                lower2 = d2.dot(d2);
            }

            if lower1 < lower2 && !leaf1 {
                index = c1;
                area_base = area1;
                direct_cost = direct1;
            } else {
                index = c2;
                area_base = area2;
                direct_cost = direct2;
            }
        }
        best
    }

    fn insert_leaf(&mut self, leaf: u32) {
        if self.root == NULL {
            self.root = leaf;
            self.nodes[leaf as usize].parent = NULL;
            return;
        }

        let sibling = self.best_sibling(self.nodes[leaf as usize].aabb);

        // Splice a fresh parent in between the sibling and its old parent.
        let old_parent = self.nodes[sibling as usize].parent;
        let new_parent = self.alloc();
        self.nodes[new_parent as usize] = Node {
            aabb: self.nodes[leaf as usize].aabb.union(self.nodes[sibling as usize].aabb),
            bits: self.nodes[leaf as usize].bits | self.nodes[sibling as usize].bits,
            id: Id(0),
            parent: old_parent,
            child1: sibling,
            child2: leaf,
            height: self.nodes[sibling as usize].height + 1,
        };
        self.nodes[sibling as usize].parent = new_parent;
        self.nodes[leaf as usize].parent = new_parent;
        if old_parent == NULL {
            self.root = new_parent;
        } else if self.nodes[old_parent as usize].child1 == sibling {
            self.nodes[old_parent as usize].child1 = new_parent;
        } else {
            self.nodes[old_parent as usize].child2 = new_parent;
        }

        self.refit_up(new_parent);
    }

    fn remove_leaf(&mut self, leaf: u32) {
        if self.root == leaf {
            self.root = NULL;
            return;
        }
        // The leaf's parent disappears with it: the sibling takes the
        // parent's place under the grandparent.
        let parent = self.nodes[leaf as usize].parent;
        let grand = self.nodes[parent as usize].parent;
        let sibling = if self.nodes[parent as usize].child1 == leaf {
            self.nodes[parent as usize].child2
        } else {
            self.nodes[parent as usize].child1
        };
        self.dealloc(parent);
        self.nodes[sibling as usize].parent = grand;
        if grand == NULL {
            self.root = sibling;
        } else {
            if self.nodes[grand as usize].child1 == parent {
                self.nodes[grand as usize].child1 = sibling;
            } else {
                self.nodes[grand as usize].child2 = sibling;
            }
            self.refit_up(grand);
        }
    }

    /// Walk from `start` to the root re-deriving each node's box, bits,
    /// and height from its children, rotating where imbalanced.
    fn refit_up(&mut self, start: u32) {
        let mut i = start;
        while i != NULL {
            i = self.balance(i);
            let c1 = self.nodes[i as usize].child1 as usize;
            let c2 = self.nodes[i as usize].child2 as usize;
            self.nodes[i as usize].height =
                1 + self.nodes[c1].height.max(self.nodes[c2].height);
            self.nodes[i as usize].aabb = self.nodes[c1].aabb.union(self.nodes[c2].aabb);
            self.nodes[i as usize].bits = self.nodes[c1].bits | self.nodes[c2].bits;
            i = self.nodes[i as usize].parent;
        }
    }

    /// AVL-style rotation: when one child of `a` is two levels taller
    /// than the other, the taller child rotates up to become `a`'s
    /// parent and hands its shorter grandchild down. Keeps height (and
    /// so query cost) `O(log n)` no matter the insertion order. Returns
    /// the node now occupying `a`'s position. (Box3D balances by
    /// surface-area rotations instead — same role; swap in if
    /// `area_ratio` ever shows bad trees.)
    fn balance(&mut self, a: u32) -> u32 {
        let ia = a as usize;
        if self.nodes[ia].leaf() || self.nodes[ia].height < 2 {
            return a;
        }
        let b = self.nodes[ia].child1;
        let c = self.nodes[ia].child2;
        let diff = self.nodes[c as usize].height - self.nodes[b as usize].height;
        if diff > 1 {
            self.rotate_up(c, a)
        } else if diff < -1 {
            self.rotate_up(b, a)
        } else {
            a
        }
    }

    /// Rotate child `c` above its parent `a`: `c` takes `a`'s slot,
    /// `a` becomes `c`'s child, and `c`'s shorter grandchild moves to
    /// the slot `c` vacated on `a`.
    fn rotate_up(&mut self, c: u32, a: u32) -> u32 {
        let (ia, ic) = (a as usize, c as usize);
        let f = self.nodes[ic].child1;
        let g = self.nodes[ic].child2;

        // c takes a's place under a's parent.
        let ap = self.nodes[ia].parent;
        self.nodes[ic].parent = ap;
        if ap == NULL {
            self.root = c;
        } else if self.nodes[ap as usize].child1 == a {
            self.nodes[ap as usize].child1 = c;
        } else {
            self.nodes[ap as usize].child2 = c;
        }

        // a hangs under c, paired with the taller of c's children; the
        // shorter one fills the child slot a lost.
        let (keep, give) = if self.nodes[f as usize].height >= self.nodes[g as usize].height {
            (f, g)
        } else {
            (g, f)
        };
        self.nodes[ic].child1 = a;
        self.nodes[ic].child2 = keep;
        self.nodes[ia].parent = c;
        if self.nodes[ia].child1 == c {
            self.nodes[ia].child1 = give;
        } else {
            self.nodes[ia].child2 = give;
        }
        self.nodes[give as usize].parent = a;

        // Only a and c changed shape; re-derive them bottom-up.
        for &n in &[ia, ic] {
            let c1 = self.nodes[n].child1 as usize;
            let c2 = self.nodes[n].child2 as usize;
            self.nodes[n].aabb = self.nodes[c1].aabb.union(self.nodes[c2].aabb);
            self.nodes[n].bits = self.nodes[c1].bits | self.nodes[c2].bits;
            self.nodes[n].height = 1 + self.nodes[c1].height.max(self.nodes[c2].height);
        }
        c
    }

    /// Check every structural invariant (parent links, box containment,
    /// bits coverage, height math, leaf count). Panics on violation —
    /// test/debug aid.
    pub fn validate(&self) {
        if self.root == NULL {
            assert_eq!(self.leaves, 0);
            return;
        }
        assert_eq!(self.nodes[self.root as usize].parent, NULL);
        let mut leaves = 0;
        let mut stack = vec![self.root];
        while let Some(i) = stack.pop() {
            let n = &self.nodes[i as usize];
            assert!(n.height >= 0, "free node reachable");
            if n.leaf() {
                assert_eq!(n.height, 0);
                assert_eq!(n.child2, NULL);
                leaves += 1;
                continue;
            }
            let c1 = &self.nodes[n.child1 as usize];
            let c2 = &self.nodes[n.child2 as usize];
            assert_eq!(c1.parent, i);
            assert_eq!(c2.parent, i);
            // (No strict AVL-factor assert: single rotations along the
            // touched path keep height O(log n) heuristically, not
            // everywhere-balanced — the sorted-line test bounds it.)
            assert_eq!(n.height, 1 + c1.height.max(c2.height));
            assert!(n.aabb.contains(c1.aabb) && n.aabb.contains(c2.aabb));
            assert_eq!(n.bits, c1.bits | c2.bits);
            stack.push(n.child1);
            stack.push(n.child2);
        }
        assert_eq!(leaves, self.leaves);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::math::{Rng, vec3};

    fn id(i: u32) -> Id {
        Id::new(0, 0, i)
    }

    fn cube(c: Vec3, half: f32) -> Aabb {
        Aabb::around(c, vec3(half, half, half))
    }

    /// Brute-force reference for the fuzz tests.
    fn scan(entries: &[(Id, Aabb, u64)], q: Aabb, mask: u64) -> Vec<Id> {
        let mut v: Vec<Id> = entries
            .iter()
            .filter(|(_, a, b)| b & mask != 0 && a.overlaps(q))
            .map(|(i, _, _)| *i)
            .collect();
        v.sort_by_key(|i| i.0);
        v
    }

    #[test]
    fn insert_query_remove() {
        let mut t = DynamicTree::new(0.5);
        let a = t.insert(cube(vec3(0.0, 0.0, 0.0), 1.0), 1, id(1));
        let _b = t.insert(cube(vec3(100.0, 0.0, 0.0), 1.0), 1, id(2));
        t.validate();

        let mut hits = Vec::new();
        t.query(cube(vec3(0.0, 0.0, 0.0), 5.0), u64::MAX, |i| hits.push(i));
        assert_eq!(hits, vec![id(1)]);

        t.remove(a);
        t.validate();
        hits.clear();
        t.query(cube(vec3(0.0, 0.0, 0.0), 5.0), u64::MAX, |i| hits.push(i));
        assert!(hits.is_empty());
        assert_eq!(t.len(), 1);
    }

    #[test]
    fn mask_prunes_but_bits_stay_reachable() {
        // Mixed-bit subtrees must still yield the matching leaf: internal
        // bits are ORs, never filters of their own.
        let mut t = DynamicTree::new(0.5);
        for i in 0..64u32 {
            let bits = 1u64 << (i % 4);
            t.insert(cube(vec3(i as f32, 0.0, 0.0), 0.4), bits, id(i));
        }
        t.validate();
        let mut hits = Vec::new();
        t.query(cube(vec3(32.0, 0.0, 0.0), 100.0), 1 << 2, |i| hits.push(i));
        hits.sort_by_key(|i| i.0);
        let want: Vec<Id> = (0..64).filter(|i| i % 4 == 2).map(id).collect();
        assert_eq!(hits, want);
    }

    #[test]
    fn update_is_lazy_within_margin() {
        let mut t = DynamicTree::new(1.0);
        let p = t.insert(cube(vec3(0.0, 0.0, 0.0), 1.0), 1, id(1));
        assert!(!t.update(p, cube(vec3(0.5, 0.0, 0.0), 1.0)), "inside margin");
        assert!(t.update(p, cube(vec3(3.0, 0.0, 0.0), 1.0)), "past margin");
        t.validate();
        let mut hits = 0;
        t.query(cube(vec3(3.0, 0.0, 0.0), 0.5), u64::MAX, |_| hits += 1);
        assert_eq!(hits, 1);
    }

    #[test]
    fn sorted_line_insert_stays_balanced() {
        // Worst case for an unbalanced tree: monotone insertion order.
        let mut t = DynamicTree::new(0.1);
        for i in 0..1024u32 {
            t.insert(cube(vec3(i as f32 * 2.0, 0.0, 0.0), 0.5), 1, id(i));
        }
        t.validate();
        assert!(t.height() <= 22, "height {} for 1024 leaves", t.height());
    }

    #[test]
    fn cast_finds_first_hit_and_clips() {
        let mut t = DynamicTree::new(0.1);
        for i in 1..=5u32 {
            t.insert(cube(vec3(i as f32 * 10.0, 0.0, 0.0), 1.0), 1, id(i));
        }
        // Sweep down the row; report each candidate, clip at its center.
        let mut seen = Vec::new();
        t.cast(vec3(0.0, 0.0, 0.0), vec3(100.0, 0.0, 0.0), 0.0, u64::MAX, |i| {
            seen.push(i);
            i.index() as f32 * 10.0 / 100.0
        });
        // Box order isn't sorted along the ray, but after all callbacks
        // the clip must have excluded boxes past the nearest hit.
        assert!(seen.contains(&id(1)));
        let nearest = seen.iter().map(|i| i.index()).min().unwrap();
        assert_eq!(nearest, 1);
        // A miss (parallel offset ray) reports nothing.
        let mut n = 0;
        t.cast(vec3(0.0, 50.0, 0.0), vec3(100.0, 50.0, 0.0), 0.0, u64::MAX, |_| {
            n += 1;
            1.0
        });
        assert_eq!(n, 0);
        // Radius turns the miss back into hits (sweep passes within 50).
        let mut n = 0;
        t.cast(vec3(0.0, 50.0, 0.0), vec3(100.0, 50.0, 0.0), 49.5, u64::MAX, |_| {
            n += 1;
            1.0
        });
        assert!(n == 5, "fat sweep sees the whole row, got {n}");
    }

    #[test]
    fn fuzz_against_linear_scan() {
        let mut rng = Rng::new(0xB3);
        let mut t = DynamicTree::new(0.5);
        let mut entries: Vec<(Id, Aabb, u64, u32)> = Vec::new(); // (.., proxy)
        let mut next = 0u32;
        for round in 0..2000 {
            let op = rng.next_u32() % 10;
            if op < 4 || entries.is_empty() {
                let a = cube(
                    vec3(rng.rfr(-100.0, 100.0), rng.rfr(-20.0, 20.0), rng.rfr(-100.0, 100.0)),
                    rng.rfr(0.2, 8.0),
                );
                let bits = 1u64 << (rng.next_u32() % 3);
                next += 1;
                let p = t.insert(a, bits, id(next));
                entries.push((id(next), a, bits, p));
            } else if op < 6 {
                let k = rng.next_u32() as usize % entries.len();
                let (i, _, bits, p) = entries[k];
                let a = cube(
                    vec3(rng.rfr(-100.0, 100.0), rng.rfr(-20.0, 20.0), rng.rfr(-100.0, 100.0)),
                    rng.rfr(0.2, 8.0),
                );
                t.update(p, a);
                entries[k] = (i, a, bits, p);
            } else if op < 7 {
                let k = rng.next_u32() as usize % entries.len();
                t.remove(entries[k].3);
                entries.swap_remove(k);
            } else {
                let q = cube(
                    vec3(rng.rfr(-100.0, 100.0), rng.rfr(-20.0, 20.0), rng.rfr(-100.0, 100.0)),
                    rng.rfr(1.0, 40.0),
                );
                let mask = 1u64 << (rng.next_u32() % 3);
                let mut got = Vec::new();
                t.query(q, mask, |i| got.push(i));
                got.sort_by_key(|i| i.0);
                let flat: Vec<(Id, Aabb, u64)> =
                    entries.iter().map(|&(i, a, b, _)| (i, a, b)).collect();
                // The tree stores FAT boxes, so it may report a few extra
                // near-misses (broadphase is allowed false positives,
                // never false negatives) — assert the reference set is a
                // subset of what the tree returned.
                for want in scan(&flat, q, mask) {
                    assert!(got.contains(&want), "round {round}: missing {want:?}");
                }
                for have in &got {
                    let (_, a, b, _) = entries.iter().find(|e| e.0 == *have).unwrap();
                    assert!(
                        b & mask != 0 && a.expand(0.5).overlaps(q),
                        "round {round}: {have:?} beyond fat bounds"
                    );
                }
            }
            if round % 250 == 0 {
                t.validate();
            }
        }
        t.validate();
    }
}
