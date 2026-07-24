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
//!   Blender; never drawn). The server's Box3D world builds its
//!   hitbox shapes straight from each box's bounds
//!   ([`collide_boxes`]); the tilde debug cage draws those SAME boxes
//!   ([`box_cage_tris`]). Reshape a tail in Blender and its hitbox
//!   moves with it; game code never learns triangles exist.
//! The flow, in pm shapes: glb file → [`Models`] SINGLE (kind data,
//! by name) → phys world BODIES (instance data, by id) → contact
//! facts.
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
    FLYER_H, FLYER_R, HOG_H, HOG_R, PART_BODY, PART_ROTOR, PART_TAIL, TRUCK_R,
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

// --- hitbox debug cages -------------------------------------------------------

/// Cage ribbon thickness — thin enough to read as wireframe.
const RIB: f32 = 0.06;

/// The hitbox DEBUG CAGE for one solver box (entity space, center +
/// half extents): this is EXACTLY the box Box3D contacts and casts —
/// the authored `collide.*` boxes for heli/flyer, the phys constants
/// for the truck. Model space; the instance pose carries the FULL
/// replicated `Body` rotation, so a tumbling truck's cage tumbles.
pub fn box_cage_tris(center: pm::Vec3, half: pm::Vec3) -> Vec<[pm::Vec3; 3]> {
    let corners = [
        (center.x + half.x, center.z + half.z),
        (center.x - half.x, center.z + half.z),
        (center.x - half.x, center.z - half.z),
        (center.x + half.x, center.z - half.z),
    ];
    cage_tris(&corners, center.y - half.y, center.y + half.y, 1)
}

/// The cage for a solver capsule, drawn as its bounding cylinder:
/// radius `r` over `[y0, y1]` (the hemispherical caps round off inside
/// it — close enough for a debug view, exact at the equator where
/// flat shots live).
pub fn cylinder_cage_tris(r: f32, y0: f32, y1: f32) -> Vec<[pm::Vec3; 3]> {
    const SEG: usize = 20;
    let pts: Vec<(f32, f32)> = (0..SEG)
        .map(|k| {
            let a = std::f32::consts::TAU * k as f32 / SEG as f32;
            (r * a.cos(), r * a.sin())
        })
        .collect();
    cage_tris(&pts, y0, y1, SEG / 8)
}

/// Ribbon-cage generator over a closed footprint outline: a ring at
/// the bottom of the band, a ring at the top, and a full-height strut
/// at every `strut`th outline point (narrow, so box faces stay open).
fn cage_tris(outline: &[(f32, f32)], y0: f32, y1: f32, strut: usize) -> Vec<[pm::Vec3; 3]> {
    let mut tris = Vec::new();
    let n = outline.len();
    let v = |q: (f32, f32), y: f32| vec3(q.0, y, q.1);
    let mut quad = |a: pm::Vec3, b: pm::Vec3, c: pm::Vec3, d: pm::Vec3| {
        tris.push([a, b, c]);
        tris.push([a, c, d]);
    };
    for i in 0..n {
        let (p0, p1) = (outline[i], outline[(i + 1) % n]);
        quad(v(p0, y0), v(p1, y0), v(p1, y0 + RIB), v(p0, y0 + RIB));
        quad(v(p0, y1 - RIB), v(p1, y1 - RIB), v(p1, y1), v(p0, y1));
        if i % strut.max(1) == 0 {
            let (dx, dz) = (p1.0 - p0.0, p1.1 - p0.1);
            let len = (dx * dx + dz * dz).sqrt().max(1e-6);
            let w = (3.0 * RIB).min(len) / len;
            let pw = (p0.0 + dx * w, p0.1 + dz * w);
            quad(v(p0, y0), v(pw, y0), v(pw, y1), v(p0, y1));
        }
    }
    tris
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

/// A model's `collide.*` parts as the AUTHORED boxes (entity space,
/// center + half extents), body first — the Box3D door: the solver
/// casts and overlaps the box exactly as Blender drew it, and the
/// debug cage draws the same boxes ([`box_cage_tris`]).
pub fn collide_boxes(data: &ModelData) -> Vec<(u8, pm::Vec3, pm::Vec3)> {
    let mut boxes = Vec::new();
    for p in data.parts.iter().filter(|p| p.name.starts_with("collide.")) {
        let (mut lo, mut hi) = ([f32::MAX; 3], [f32::MIN; 3]);
        for v in &p.verts {
            for i in 0..3 {
                lo[i] = lo[i].min(v.pos[i]);
                hi[i] = hi[i].max(v.pos[i]);
            }
        }
        let center = vec3((lo[0] + hi[0]) * 0.5, (lo[1] + hi[1]) * 0.5, (lo[2] + hi[2]) * 0.5);
        let half = vec3((hi[0] - lo[0]) * 0.5, (hi[1] - lo[1]) * 0.5, (hi[2] - lo[2]) * 0.5);
        boxes.push((collide_tag(&p.name), center, half));
    }
    boxes.sort_by_key(|b| b.0 != PART_BODY);
    boxes
}

// --- the registry ------------------------------------------------------------

/// The models REGISTRY: kind-level data both sides read, installed as
/// the LOCAL `"models"` single on each `Pm` — the params-single
/// pattern applied to shape. Name-keyed on purpose: kind is which
/// POOL an entity lives in, so the join between rendering and physics
/// happens at the instance level (the phys world's bodies, by id);
/// this single just answers "what shape is a <name>".
#[derive(Default)]
pub struct Models {
    entries: Vec<(&'static str, ModelData)>,
}

impl Models {
    /// Load every registered model, asset-or-procedural. CPU only —
    /// no GPU, the headless server runs this too.
    pub fn load() -> Models {
        Models {
            entries: all()
                .into_iter()
                .map(|(name, fallback, required)| (name, load_data(name, fallback, required)))
                .collect(),
        }
    }

    pub fn data(&self, name: &str) -> &ModelData {
        &self.entries.iter().find(|e| e.0 == name).expect("unknown model").1
    }

    /// The kind's authored `collide.*` boxes, body first — what the
    /// server's Box3D world builds hitbox shapes from.
    pub fn boxes(&self, name: &str) -> Vec<(u8, pm::Vec3, pm::Vec3)> {
        collide_boxes(self.data(name))
    }

    /// Upload a kind's render parts (the client's side of the split).
    pub fn upload(&self, r3d: &Renderer3d, name: &str) -> Model3 {
        r3d.upload_model(self.data(name)).expect("model upload")
    }
}

/// The authored-hitbox baseline: the `collide.*` boxes the Box3D world
/// builds shapes from must reproduce the landed hitboxes (truck slab,
/// heli cabin/tail/rotor, hog and flyer bands). If art later CHOOSES
/// to move a hitbox, this test is the place that documents the
/// baseline it moved from.
#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-5
    }

    fn boxed(v: pm::Vec3, x: f32, y: f32, z: f32) -> bool {
        close(v.x, x) && close(v.y, y) && close(v.z, z)
    }

    #[test]
    fn collide_boxes_match_the_landed_hitboxes() {
        let t = collide_boxes(&truck());
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].0, PART_BODY);
        assert!(boxed(t[0].1, 0.0, 0.8, 0.0) && boxed(t[0].2, TRUCK_R, 0.8, 1.7));

        let h = collide_boxes(&heli());
        assert_eq!(h.len(), 3);
        assert_eq!((h[0].0, h[1].0, h[2].0), (PART_BODY, PART_TAIL, PART_ROTOR));
        assert!(boxed(h[0].1, 0.0, 0.0, 0.0) && boxed(h[0].2, 1.0, 1.0, 1.0));
        assert!(boxed(h[1].1, 0.0, 0.0, -2.0) && boxed(h[1].2, 0.45, 0.45, 1.25));
        assert!(boxed(h[2].1, 0.0, 0.85, 0.0) && boxed(h[2].2, 1.7, 0.25, 1.7));

        let g = collide_boxes(&hog());
        assert_eq!(g.len(), 1);
        assert!(boxed(g[0].1, 0.0, HOG_H * 0.5, 0.0) && boxed(g[0].2, HOG_R, HOG_H * 0.5, HOG_R));

        let f = collide_boxes(&flyer());
        assert_eq!(f.len(), 1);
        assert!(boxed(f[0].1, 0.0, 0.0, 0.0) && boxed(f[0].2, FLYER_R, FLYER_H, FLYER_R));
    }

    /// The debug cage must lie ON the shape it visualizes: every box
    /// cage vertex sits on the box's boundary inside its band, every
    /// cylinder cage vertex at exactly r from the axis.
    #[test]
    fn cages_hug_their_shapes() {
        let (_, c, h) = collide_boxes(&hog())[0];
        for t in box_cage_tris(c, h) {
            for v in t {
                let on_x = (v.x.abs() - h.x).abs() < 1e-4;
                let on_z = (v.z.abs() - h.z).abs() < 1e-4;
                assert!(on_x || on_z, "cage vertex off the box surface");
                assert!(v.x.abs() <= h.x + 1e-4 && v.z.abs() <= h.z + 1e-4);
                assert!(v.y >= c.y - h.y - 1e-6 && v.y <= c.y + h.y + 1e-6);
            }
        }
        for t in cylinder_cage_tris(0.45, 0.0, 1.5) {
            for v in t {
                assert!(((v.x * v.x + v.z * v.z).sqrt() - 0.45).abs() < 1e-4);
                assert!(v.y >= -1e-6 && v.y <= 1.5 + 1e-6);
            }
        }
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
