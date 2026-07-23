//! The hogs models, as DATA: every entity's parts are defined here
//! once, and serve two masters — `hogs genassets` writes them out as
//! the seed `assets/*.glb` files, and both sides use them as the
//! procedural FALLBACK when an asset is missing or broken (the sfx
//! `clip_or` doctrine: a zero-asset checkout still runs). Files and
//! fallback start byte-equivalent; from then on the .glb is the art —
//! open it in Blender, reshape, re-export, and the code never changes.
//!
//! ONE MODEL, TWO READERS. A model's parts split by NAME into layers:
//! - Render parts (`body`, `cabin`, `rotor`, …): as detailed as the
//!   art wants. The client uploads and draws these.
//! - **`collide.*` parts**: deliberately dumb boxes roughed around the
//!   render geometry (baked magenta so they read as markers in
//!   Blender; never drawn). The loader derives the game's collision
//!   vocabulary from each box's bounds — footprint long axis becomes
//!   the capsule segment, half the short extent the radius, y extent
//!   the altitude band ([`collide_protos`]) — and the SERVER poses
//!   those protos into the collider pool every tick. Reshape a tail
//!   in Blender and its hitbox moves with it; the sweep never learns
//!   triangles exist.
//! The flow, in pm shapes: glb file → [`Models`] SINGLE (kind data,
//! by name) → collider POOL (instance data, by id) → contact facts.
//!
//! Contract with the draw code:
//! - Vertex colors carry the model's OWN shading (cabin darker than
//!   body, snout darker than hide); the per-draw tint carries
//!   IDENTITY/STATE (peer color, hp fade) and multiplies through.
//!   Parts that were fixed-color in the old draw code (heli skid,
//!   rotor) bake their ABSOLUTE color and draw with white.
//! - Parts are authored in ENTITY space with identity node transforms,
//!   so the draw loop keeps hand-composing pose matrices (turret,
//!   rotor spin, wing flap) exactly as before. The heli "gun" is the
//!   exception to entity-space: it's authored about its PIVOT and the
//!   draw translates it to the chin. Clip-driven posing
//!   (`Skeleton::pose`) takes over when real Blender rigs arrive.
//! - The client draws the parts it KNOWS by name (`Model3::mesh`);
//!   extra render parts in an edited .glb are ignored for now (extra
//!   `collide.*` parts DO collide — as PART_BODY, with a warning),
//!   and a missing required part rejects the file loudly at load
//!   (fallback wins).

// TODO(ship): hog gait — author a real hog.glb in Blender with a
// walk clip; the import/skeleton runtime (`Skeleton::pose`) is in.
// The gait cycle derives from replicated speed, so it costs zero wire.
// TODO(ship): content — the pipeline is DONE, feed it: hog variants
// (a tint + a `Params` row), a second weapon feel, a real map layout
// (`BUILDINGS` in common.rs is data). Each is art + numbers, zero
// engine work — this is what all the engine work was FOR.
// TODO(roadmap): model hot-reload — an mtime poll in the render task
// plus a server-side reload, so Blender edits land without a restart
// (the registry currently loads once at startup).

use pm::vec3;
use pm_sdl::gpu3d::{Renderer3d, Vertex3, bake, box_tris};
use pm_sdl::model::{Model3, ModelData, NO_PARENT, PartData, Skeleton, Transform3};

use crate::common::{
    FLYER_H, FLYER_R, HOG_H, HOG_R, Hull, PART_BODY, PART_ROTOR, PART_TAIL, TRUCK_R,
};

/// Marker color for `collide.*` boxes: loud in Blender, never drawn.
const MAGENTA: (f32, f32, f32) = (1.0, 0.0, 1.0);

/// Flat-part model: every part its own root node, identity rest.
fn model(parts: Vec<(&str, Vec<Vertex3>)>) -> ModelData {
    ModelData {
        skeleton: Skeleton {
            parents: vec![NO_PARENT; parts.len()],
            rest: vec![Transform3::IDENTITY; parts.len()],
            names: parts.iter().map(|(n, _)| n.to_string()).collect(),
        },
        parts: parts
            .into_iter()
            .enumerate()
            .map(|(i, (name, verts))| PartData { name: name.to_string(), node: i, verts })
            .collect(),
        clips: Vec::new(),
    }
}

/// Truck: body + cabin, turret barrel as its own part (the draw poses
/// it by `heading + aim`). Authored facing +z. The collide box is the
/// landed hull: capsule ±0.8 along forward at r = TRUCK_R, band to the
/// old TRUCK_HULL_H (1.6).
pub fn truck() -> ModelData {
    model(vec![
        ("body", bake(&box_tris(vec3(-0.9, 0.15, -1.7), vec3(0.9, 0.95, 1.7)), (1.0, 1.0, 1.0))),
        ("cabin", bake(&box_tris(vec3(-0.7, 0.95, -1.1), vec3(0.7, 1.55, 0.45)), (0.5, 0.5, 0.5))),
        (
            "barrel",
            bake(&box_tris(vec3(-0.12, 1.45, -0.35), vec3(0.12, 1.72, 1.9)), (0.35, 0.35, 0.35)),
        ),
        (
            "collide.body",
            bake(&box_tris(vec3(-TRUCK_R, 0.0, -1.7), vec3(TRUCK_R, 1.6, 1.7)), MAGENTA),
        ),
    ])
}

/// Heli: cabin pod, tail boom, skid plate, one rotor blade (drawn
/// spinning), and the chin gun (authored about its pivot — the draw
/// translates it under the nose and aims it by the gimbal). Collide
/// boxes are the landed stage-4 parts: cabin ball (r 1.0), tail-boom
/// capsule (−2.8..−1.2, r 0.45), rotor disc (r 1.7, band 0.6..1.1) —
/// all RELATIVE to the body center (the pose adds the altitude).
pub fn heli() -> ModelData {
    model(vec![
        ("body", bake(&box_tris(vec3(-0.9, 0.35, -1.3), vec3(0.9, 1.6, 1.6)), (1.0, 1.0, 1.0))),
        ("tail", bake(&box_tris(vec3(-0.16, 0.85, -3.6), vec3(0.16, 1.35, -1.3)), (0.6, 0.6, 0.6))),
        ("skid", bake(&box_tris(vec3(-1.0, 0.0, -1.3), vec3(1.0, 0.2, 1.4)), (0.2, 0.2, 0.22))),
        (
            "rotor",
            bake(&box_tris(vec3(-3.1, 1.72, -0.16), vec3(3.1, 1.86, 0.16)), (0.25, 0.25, 0.28)),
        ),
        ("gun", bake(&box_tris(vec3(-0.09, -0.11, 0.15), vec3(0.09, 0.11, 1.75)), (0.35, 0.35, 0.35))),
        ("collide.body", bake(&box_tris(vec3(-1.0, -1.0, -1.0), vec3(1.0, 1.0, 1.0)), MAGENTA)),
        (
            "collide.tail",
            bake(&box_tris(vec3(-0.45, -0.45, -3.25), vec3(0.45, 0.45, -0.75)), MAGENTA),
        ),
        (
            "collide.rotor",
            bake(&box_tris(vec3(-1.7, 0.6, -1.7), vec3(1.7, 1.1, 1.7)), MAGENTA),
        ),
    ])
}

/// Hog: low mean slab + snout, tinted by hp at draw. The collide box
/// is the gameplay band [0, HOG_H] — taller than the drawn hog on
/// purpose (flat truck shots must keep connecting; hitbox, not
/// silhouette).
pub fn hog() -> ModelData {
    model(vec![
        ("body", bake(&box_tris(vec3(-0.55, 0.1, -0.7), vec3(0.55, 0.8, 0.7)), (1.0, 1.0, 1.0))),
        ("snout", bake(&box_tris(vec3(-0.28, 0.2, 0.7), vec3(0.28, 0.6, 1.05)), (0.7, 0.7, 0.7))),
        (
            "collide.body",
            bake(&box_tris(vec3(-HOG_R, 0.0, -HOG_R), vec3(HOG_R, HOG_H, HOG_R)), MAGENTA),
        ),
    ])
}

/// Flyer: airborne slab about its center + two wings hinged at the
/// body (the draw flaps them about the forward axis). Collide band is
/// ±FLYER_H about the center; the pose adds the flyer's altitude.
pub fn flyer() -> ModelData {
    model(vec![
        ("body", bake(&box_tris(vec3(-0.45, -0.26, -0.6), vec3(0.45, 0.26, 0.65)), (1.0, 1.0, 1.0))),
        (
            "wing.l",
            bake(&box_tris(vec3(-1.7, -0.05, -0.45), vec3(-0.35, 0.05, 0.4)), (0.55, 0.55, 0.55)),
        ),
        (
            "wing.r",
            bake(&box_tris(vec3(0.35, -0.05, -0.45), vec3(1.7, 0.05, 0.4)), (0.55, 0.55, 0.55)),
        ),
        (
            "collide.body",
            bake(
                &box_tris(vec3(-FLYER_R, -FLYER_H, -FLYER_R), vec3(FLYER_R, FLYER_H, FLYER_R)),
                MAGENTA,
            ),
        ),
    ])
}

/// Every model by asset name — `genassets` writes this list, both
/// sides load it. The part-name lists are the game's contract with a
/// .glb: draw parts the client poses by name, `collide.*` parts the
/// server needs for hitboxes — a file missing any of them is rejected
/// loudly and the code definition wins.
pub fn all() -> [(&'static str, fn() -> ModelData, &'static [&'static str]); 4] {
    [
        ("truck", truck, &["body", "cabin", "barrel", "collide.body"]),
        (
            "heli",
            heli,
            &["body", "tail", "skid", "rotor", "gun", "collide.body", "collide.tail", "collide.rotor"],
        ),
        ("hog", hog, &["body", "snout", "collide.body"]),
        ("flyer", flyer, &["body", "wing.l", "wing.r", "collide.body"]),
    ]
}

// --- collision protos --------------------------------------------------------

/// A collision part in MODEL space, derived from a `collide.*` box:
/// capsule endpoints + radius on the ground plane, altitude band
/// relative to the entity origin. [`Proto::pose`] is the per-tick step
/// from KIND data (this) to INSTANCE data (a collider-pool [`Hull`]).
#[derive(Clone, Copy)]
pub struct Proto {
    /// `PART_*` tag, mapped from the node name — response tasks
    /// branch on it (rotor ×2 damage, tail kick).
    pub part: u8,
    pub a: (f32, f32),
    pub b: (f32, f32),
    pub r: f32,
    /// Altitude band relative to the entity origin: ground vehicles
    /// author it absolute and pose with y = 0, fliers author it about
    /// their center and pose with their altitude.
    pub y: (f32, f32),
}

impl Proto {
    /// Pose into a world hull: yaw about the entity origin (heading
    /// convention — forward is (sin, cos)), then translate.
    pub fn pose(&self, x: f32, y: f32, z: f32, yaw: f32) -> Hull {
        let (s, c) = yaw.sin_cos();
        let rot = |p: (f32, f32)| (x + p.0 * c + p.1 * s, z - p.0 * s + p.1 * c);
        Hull {
            a: rot(self.a),
            b: rot(self.b),
            r: self.r,
            y: (y + self.y.0, y + self.y.1),
        }
    }
}

/// Pose every proto of a kind — what `parts_add` and the per-tick
/// re-pose feed straight into the collider pool.
pub fn posed(protos: &[Proto], x: f32, y: f32, z: f32, yaw: f32) -> Vec<(u8, Hull)> {
    protos.iter().map(|p| (p.part, p.pose(x, y, z, yaw))).collect()
}

/// The hitbox DEBUG CAGE for a kind: every proto's swept shape — the
/// stadium footprint (capsule cross-section) extruded over its altitude
/// band — as thin ribbon triangles: a ring at the bottom of the band, a
/// ring at the top, and struts between them. This is the DERIVED hull
/// the sweep actually tests, not the authored `collide.*` box (the
/// derivation rounds the box's corners off — exactly the difference a
/// debug view exists to show). Model space, like the protos: draw with
/// `translate(x,y,z) * rot_y(yaw)` using the same (x,y,z,yaw) the
/// server feeds [`Proto::pose`] and the cage lands on the hitbox.
pub fn hull_cage_tris(protos: &[Proto]) -> Vec<[pm::Vec3; 3]> {
    /// Ribbon thickness — thin enough to read as wireframe.
    const RIB: f32 = 0.06;
    let mut tris = Vec::new();
    for p in protos {
        let outline = stadium_outline(p.a, p.b, p.r);
        let (y0, y1) = p.y;
        let v = |q: (f32, f32), y: f32| vec3(q.0, y, q.1);
        let mut quad = |a: pm::Vec3, b: pm::Vec3, c: pm::Vec3, d: pm::Vec3| {
            tris.push([a, b, c]);
            tris.push([a, c, d]);
        };
        let n = outline.len();
        for i in 0..n {
            let (p0, p1) = (outline[i], outline[(i + 1) % n]);
            quad(v(p0, y0), v(p1, y0), v(p1, y0 + RIB), v(p0, y0 + RIB));
            quad(v(p0, y1 - RIB), v(p1, y1 - RIB), v(p1, y1), v(p0, y1));
        }
        // Struts: every eighth of the outline, a full-height wall one
        // segment wide.
        for i in (0..n).step_by((n / 8).max(1)) {
            let (p0, p1) = (outline[i], outline[(i + 1) % n]);
            quad(v(p0, y0), v(p1, y0), v(p1, y1), v(p0, y1));
        }
    }
    tris
}

/// The capsule cross-section's boundary at radius `r` around segment
/// `a..b`, counter-clockwise in the footprint plane: two half-arcs
/// joined by the straight sides (a plain circle when the capsule is a
/// point). Every point is EXACTLY r from the segment — the cage
/// inherits the hull's precision.
fn stadium_outline(a: (f32, f32), b: (f32, f32), r: f32) -> Vec<(f32, f32)> {
    use std::f32::consts::{PI, TAU};
    const SEG: usize = 10;
    let (dx, dz) = (b.0 - a.0, b.1 - a.1);
    let mut pts = Vec::new();
    if (dx * dx + dz * dz).sqrt() < 1e-6 {
        for k in 0..2 * SEG {
            let ang = TAU * k as f32 / (2 * SEG) as f32;
            pts.push((a.0 + r * ang.cos(), a.1 + r * ang.sin()));
        }
    } else {
        let ab = dz.atan2(dx);
        // Half-arc capping b (sweeping through b's far side), then the
        // half-arc capping a; consecutive points bridge the straights.
        for k in 0..=SEG {
            let ang = ab + PI / 2.0 - PI * k as f32 / SEG as f32;
            pts.push((b.0 + r * ang.cos(), b.1 + r * ang.sin()));
        }
        for k in 0..=SEG {
            let ang = ab - PI / 2.0 - PI * k as f32 / SEG as f32;
            pts.push((a.0 + r * ang.cos(), a.1 + r * ang.sin()));
        }
    }
    pts
}

/// `collide.*` node name → part tag. Unknown names still collide —
/// as the body, with a warning — so a Blender-added box is never
/// silently ignored.
fn collide_tag(name: &str) -> u8 {
    match name {
        "collide.body" => PART_BODY,
        "collide.tail" => PART_TAIL,
        "collide.rotor" => PART_ROTOR,
        other => {
            eprintln!("[models] unknown part '{other}' — colliding as the body");
            PART_BODY
        }
    }
}

/// Derive a model's collision protos from its `collide.*` parts: each
/// box's bounds become a capsule + band — the footprint's long axis is
/// the segment, half the short extent the radius (a near-square
/// footprint collapses to a point capsule = cylinder), the y extent
/// the band. The BODY part sorts first (`ids[0]` bite convention).
pub fn collide_protos(data: &ModelData) -> Vec<Proto> {
    let mut protos = Vec::new();
    for p in data.parts.iter().filter(|p| p.name.starts_with("collide.")) {
        let (mut lo, mut hi) = ([f32::MAX; 3], [f32::MIN; 3]);
        for v in &p.verts {
            for i in 0..3 {
                lo[i] = lo[i].min(v.pos[i]);
                hi[i] = hi[i].max(v.pos[i]);
            }
        }
        let (cx, cz) = ((lo[0] + hi[0]) * 0.5, (lo[2] + hi[2]) * 0.5);
        let (hx, hz) = ((hi[0] - lo[0]) * 0.5, (hi[2] - lo[2]) * 0.5);
        let (a, b, r) = if (hx - hz).abs() < 0.05 {
            ((cx, cz), (cx, cz), hx.max(hz))
        } else if hz > hx {
            ((cx, cz - (hz - hx)), (cx, cz + (hz - hx)), hx)
        } else {
            ((cx - (hx - hz), cz), (cx + (hx - hz), cz), hz)
        };
        protos.push(Proto { part: collide_tag(&p.name), a, b, r, y: (lo[1], hi[1]) });
    }
    protos.sort_by_key(|p| p.part != PART_BODY);
    protos
}

// --- the registry ------------------------------------------------------------

/// The models REGISTRY: kind-level data both sides read, installed as
/// the LOCAL `"models"` single on each `Pm` — the params-single
/// pattern applied to shape. Name-keyed on purpose: kind is which
/// POOL an entity lives in, so the join between rendering and physics
/// happens at the instance level (the collider pool, by id); this
/// single just answers "what shape is a <name>".
#[derive(Default)]
pub struct Models {
    entries: Vec<(&'static str, ModelData, Vec<Proto>)>,
}

impl Models {
    /// Load every registered model, asset-or-procedural. CPU only —
    /// no GPU, the headless server runs this too.
    pub fn load() -> Models {
        Models {
            entries: all()
                .into_iter()
                .map(|(name, fallback, required)| {
                    let data = load_data(name, fallback, required);
                    let protos = collide_protos(&data);
                    (name, data, protos)
                })
                .collect(),
        }
    }

    pub fn data(&self, name: &str) -> &ModelData {
        &self.entries.iter().find(|e| e.0 == name).expect("unknown model").1
    }

    /// The kind's collision protos, body first.
    pub fn protos(&self, name: &str) -> &[Proto] {
        &self.entries.iter().find(|e| e.0 == name).expect("unknown model").2
    }

    /// Upload a kind's render parts (the client's side of the split).
    pub fn upload(&self, r3d: &Renderer3d, name: &str) -> Model3 {
        r3d.upload_model(self.data(name)).expect("model upload")
    }
}

/// The migration's proof: the protos derived from the collide boxes
/// must reproduce the hand-tuned hulls this game shipped with (truck
/// capsule ±0.8/r0.9/0..1.6, heli cabin-tail-rotor, hog and flyer
/// cylinders) — moving shape authoring into the models changed NO
/// gameplay numbers. If art later CHOOSES to move a hitbox, this test
/// is the place that documents the baseline it moved from.
#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-5
    }

    #[test]
    fn derived_protos_match_the_landed_hulls() {
        let t = collide_protos(&truck());
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].part, PART_BODY);
        assert!(close(t[0].r, 0.9) && close(t[0].a.1, -0.8) && close(t[0].b.1, 0.8));
        assert!(close(t[0].y.0, 0.0) && close(t[0].y.1, 1.6));

        let h = collide_protos(&heli());
        assert_eq!(h.len(), 3);
        assert_eq!((h[0].part, h[1].part, h[2].part), (PART_BODY, PART_TAIL, PART_ROTOR));
        assert!(close(h[0].r, 1.0) && close(h[0].y.0, -1.0) && close(h[0].y.1, 1.0));
        let (za, zb) = (h[1].a.1.min(h[1].b.1), h[1].a.1.max(h[1].b.1));
        assert!(close(h[1].r, 0.45) && close(za, -2.8) && close(zb, -1.2));
        assert!(close(h[2].r, 1.7) && close(h[2].y.0, 0.6) && close(h[2].y.1, 1.1));

        let g = collide_protos(&hog());
        assert_eq!(g.len(), 1);
        assert!(close(g[0].r, HOG_R) && close(g[0].y.0, 0.0) && close(g[0].y.1, HOG_H));

        let f = collide_protos(&flyer());
        assert_eq!(f.len(), 1);
        assert!(close(f[0].r, FLYER_R) && close(f[0].y.0, -FLYER_H) && close(f[0].y.1, FLYER_H));
    }

    /// The debug cage must lie ON the hull it visualizes: every vertex
    /// of the hog's cage sits at exactly HOG_R from the axis and inside
    /// the altitude band (the tail-boom capsule gets the same check
    /// against its segment).
    #[test]
    fn hull_cage_hugs_the_derived_hull() {
        let g = collide_protos(&hog());
        for t in hull_cage_tris(&g) {
            for v in t {
                assert!(((v.x * v.x + v.z * v.z).sqrt() - HOG_R).abs() < 1e-4);
                assert!(v.y >= g[0].y.0 - 1e-6 && v.y <= g[0].y.1 + 1e-6);
            }
        }
        let tail = collide_protos(&heli())[1];
        for t in hull_cage_tris(&[tail]) {
            for v in t {
                // Distance from (x,z) to the boom segment == r.
                let (az, bz) = (tail.a.1, tail.b.1);
                let cz = v.z.clamp(az.min(bz), az.max(bz));
                let d = (v.x * v.x + (v.z - cz) * (v.z - cz)).sqrt();
                assert!((d - tail.r).abs() < 1e-4, "cage off the boom hull: {d}");
            }
        }
    }

    /// Posing: yaw swings the boom behind the heading, altitude
    /// offsets the band — the kind→instance step in one assert.
    #[test]
    fn proto_pose_follows_heading_and_altitude() {
        let tail = collide_protos(&heli())[1];
        // Facing +x (yaw = π/2): the boom trails toward −x.
        let hull = tail.pose(10.0, 5.0, 3.0, std::f32::consts::FRAC_PI_2);
        let far = hull.a.0.min(hull.b.0);
        assert!((far - (10.0 - 2.8)).abs() < 1e-4, "boom trails a +x heading, got {far}");
        assert!((hull.a.1 - 3.0).abs() < 1e-4, "no z drift for an on-axis boom");
        assert!((hull.y.0 - (5.0 - 0.45)).abs() < 1e-4, "band rides the altitude");
    }
}

/// Asset-or-procedural: `NAME.glb` from the assets dirs wins when it
/// parses AND carries every required part; anything else falls back
/// to the code definition, loudly.
fn load_data(name: &str, fallback: fn() -> ModelData, required: &[&str]) -> ModelData {
    for dir in ["examples/hogs/assets", "assets"] {
        let path = format!("{dir}/{name}.glb");
        if !std::path::Path::new(&path).exists() {
            continue;
        }
        match ModelData::load(&path) {
            Ok(data) => {
                if let Some(missing) =
                    required.iter().find(|r| !data.parts.iter().any(|p| &p.name == *r))
                {
                    eprintln!("[models] {path}: no part '{missing}' — using built-in");
                    break;
                }
                eprintln!("[models] {path} ({} parts)", data.parts.len());
                return data;
            }
            Err(e) => eprintln!("[models] {path}: {e} — using built-in"),
        }
        break;
    }
    fallback()
}
