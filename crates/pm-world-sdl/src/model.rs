//! glTF model import + the skeleton runtime — the RIGID-PER-BONE-PARTS
//! era of pm's mesh/animation plan: a `.glb` authored in Blender comes
//! in as named parts (one drawable mesh per bone/node), a joint
//! hierarchy, and keyframed clips; [`Skeleton::pose`] samples a clip
//! into per-node WORLD matrices, and each part draws through the
//! existing (instanced) pipeline with `entity_model * mats[part.node]`.
//! That's the whole animation system: it is the heli-rotor / wing-flap
//! habit generalized, with the matrices coming from Blender keyframes
//! instead of `sin(tick)`.
//!
//! Deliberately NOT here (yet): vertex skinning. Smooth deformation
//! needs a skinned vertex format, bone palettes in the vertex shader,
//! and per-instance palette plumbing for the horde — real pipeline
//! work that boxy art doesn't need. Meshes with `JOINTS_0` load rigid
//! with a loud warning. When art goes organic, skinning slots in
//! WITHOUT re-authoring: same rigs, same clips, the mesh just stops
//! being split per bone.
//!
//! Authoring rules (Blender → glTF export, .glb single file):
//! - **Vertex colors or solid materials, no textures** — gpu3d cannot
//!   sample textures in a fragment shader (naga vs SDL_gpu combined
//!   samplers), and doesn't want to: `COLOR_0` × the material's
//!   `baseColorFactor` becomes the per-vertex color, so plain colored
//!   materials work with zero painting.
//! - **1 unit = 1 meter**, face **+Z** like every pm mesh (glTF's
//!   "assets face −Z" recommendation is a camera convention we ignore),
//!   apply scale before export (non-uniform scale on animated nodes
//!   skews normals — the renderer transforms them by the model matrix).
//! - One object per bone, parented to the armature bones or just to
//!   each other — the importer reads the NODE hierarchy, so plain
//!   object parenting animates fine without an armature at all.
//! - Bake/export animations as sampled LINEAR (Blender's default);
//!   CUBICSPLINE tangents are dropped to linear with a warning.
//!
//! External `.gltf` + sidecar `.bin`/images are rejected on purpose —
//! one file per model, like one WAV per clip in audio.

use pm::{Mat4, Quat, Vec3, vec3};

use crate::gpu3d::{Mesh3, Renderer3d, Vertex3};

/// A local TRS transform — a glTF node's pose, decomposed so clips can
/// overwrite the three tracks independently.
#[derive(Clone, Copy)]
pub struct Transform3 {
    pub t: Vec3,
    pub r: Quat,
    pub s: Vec3,
}

impl Transform3 {
    pub const IDENTITY: Transform3 = Transform3 {
        t: Vec3::ZERO,
        r: Quat::IDENTITY,
        s: Vec3 { x: 1.0, y: 1.0, z: 1.0 },
    };

    pub fn mat(&self) -> Mat4 {
        Mat4::translate(self.t) * self.r.to_mat4() * Mat4::scale_xyz(self.s.x, self.s.y, self.s.z)
    }
}

/// `Skeleton::parents` sentinel: this node is a root.
pub const NO_PARENT: usize = usize::MAX;

/// The joint hierarchy: parent index + rest pose + name per node, in
/// TOPOLOGICAL order (a parent always precedes its children — the DFS
/// import order guarantees it), so one forward pass composes world
/// matrices.
#[derive(Clone)]
pub struct Skeleton {
    pub parents: Vec<usize>,
    pub rest: Vec<Transform3>,
    pub names: Vec<String>,
}

impl Skeleton {
    pub fn len(&self) -> usize {
        self.parents.len()
    }

    pub fn is_empty(&self) -> bool {
        self.parents.is_empty()
    }

    /// Index of the node called `name`, if any.
    pub fn node(&self, name: &str) -> Option<usize> {
        self.names.iter().position(|n| n == name)
    }

    /// Per-node world matrices at the REST pose (the authored layout).
    pub fn pose_rest(&self, out: &mut Vec<Mat4>) {
        out.clear();
        for i in 0..self.len() {
            let m = self.rest[i].mat();
            out.push(match self.parents[i] {
                NO_PARENT => m,
                p => out[p] * m,
            });
        }
    }

    /// Sample `clip` at `t` seconds (wrapping — a looping walk cycle
    /// is the normal case; pass a clamped `t` for one-shots) into
    /// per-node WORLD matrices. Draw a part as
    /// `entity_model * out[part.node]`. Allocates one small locals
    /// vec per call — fine at horde scale, revisit if profiles say.
    pub fn pose(&self, clip: &Clip3, t: f32, out: &mut Vec<Mat4>) {
        let t = if clip.duration > 1e-6 { t.rem_euclid(clip.duration) } else { 0.0 };
        let mut locals = self.rest.clone();
        for ch in &clip.channels {
            ch.sample(t, &mut locals[ch.node]);
        }
        out.clear();
        for i in 0..self.len() {
            let m = locals[i].mat();
            out.push(match self.parents[i] {
                NO_PARENT => m,
                p => out[p] * m,
            });
        }
    }
}

/// One animated track: keyframes for a single node's T, R, or S.
#[derive(Clone)]
pub struct Channel3 {
    pub node: usize,
    /// STEP interpolation holds each key; LINEAR lerps (nlerp for
    /// rotations, short-arc).
    pub step: bool,
    pub times: Vec<f32>,
    pub keys: Keys3,
}

#[derive(Clone)]
pub enum Keys3 {
    T(Vec<Vec3>),
    R(Vec<Quat>),
    S(Vec<Vec3>),
}

impl Channel3 {
    /// `(lo, hi, frac)` bracketing `t`, clamped at both ends.
    fn bracket(&self, t: f32) -> (usize, usize, f32) {
        let times = &self.times;
        if t <= times[0] {
            return (0, 0, 0.0);
        }
        let last = times.len() - 1;
        if t >= times[last] {
            return (last, last, 0.0);
        }
        let hi = times.partition_point(|&k| k <= t);
        let lo = hi - 1;
        let span = times[hi] - times[lo];
        let f = if span > 1e-9 { (t - times[lo]) / span } else { 0.0 };
        (lo, hi, if self.step { 0.0 } else { f })
    }

    fn sample(&self, t: f32, into: &mut Transform3) {
        if self.times.is_empty() {
            return;
        }
        let (lo, hi, f) = self.bracket(t);
        let lv = |a: Vec3, b: Vec3| a + (b - a) * f;
        match &self.keys {
            Keys3::T(k) => into.t = lv(k[lo], k[hi]),
            Keys3::S(k) => into.s = lv(k[lo], k[hi]),
            Keys3::R(k) => into.r = Quat::nlerp(k[lo], k[hi], f),
        }
    }
}

/// A named animation: Blender action → glTF animation → this.
#[derive(Clone)]
pub struct Clip3 {
    pub name: String,
    pub duration: f32,
    pub channels: Vec<Channel3>,
}

/// CPU side of one part: the node that poses it plus its baked
/// vertices (node-local space — the pose matrix is the placement).
pub struct PartData {
    pub name: String,
    pub node: usize,
    pub verts: Vec<Vertex3>,
}

/// A parsed model, before GPU upload — separate from [`Model3`] so
/// parsing and the pose math stay testable without a GPU device.
pub struct ModelData {
    pub parts: Vec<PartData>,
    pub skeleton: Skeleton,
    pub clips: Vec<Clip3>,
}

/// The uploaded model: parts hold GPU meshes, the skeleton and clips
/// ride along for posing.
pub struct Model3 {
    pub parts: Vec<Part3>,
    pub skeleton: Skeleton,
    pub clips: Vec<Clip3>,
}

pub struct Part3 {
    pub name: String,
    pub node: usize,
    pub mesh: Mesh3,
}

impl Model3 {
    /// Parse + upload in one go — the normal loading path. Callers
    /// keep the sfx `clip_or` doctrine: try the asset, fall back to
    /// procedural geometry so a zero-asset checkout still runs.
    pub fn load(r3d: &Renderer3d, path: &str) -> Result<Model3, String> {
        r3d.upload_model(&ModelData::load(path)?)
    }

    /// Index of the clip called `name`, if any.
    pub fn clip(&self, name: &str) -> Option<usize> {
        self.clips.iter().position(|c| c.name == name)
    }

    /// Index of the part called `name`, if any.
    pub fn part(&self, name: &str) -> Option<usize> {
        self.parts.iter().position(|p| p.name == name)
    }

    /// The named part's mesh. Panics when absent — part names are a
    /// CHECKED contract: validate required names at load (fall back to
    /// procedural geometry there), and the draw loop gets to treat
    /// them as infallible.
    pub fn mesh(&self, name: &str) -> &Mesh3 {
        let i = self
            .part(name)
            .unwrap_or_else(|| panic!("model has no part '{name}' (load-time validation missed)"));
        &self.parts[i].mesh
    }
}

impl Renderer3d {
    /// Upload every part of a parsed model.
    pub fn upload_model(&self, data: &ModelData) -> Result<Model3, String> {
        let mut parts = Vec::with_capacity(data.parts.len());
        for p in &data.parts {
            parts.push(Part3 {
                name: p.name.clone(),
                node: p.node,
                mesh: self.upload_mesh(&p.verts)?,
            });
        }
        Ok(Model3 {
            parts,
            skeleton: data.skeleton.clone(),
            clips: data.clips.clone(),
        })
    }
}

impl ModelData {
    pub fn load(path: &str) -> Result<ModelData, String> {
        let bytes = std::fs::read(path).map_err(|e| format!("{path}: {e}"))?;
        ModelData::parse(&bytes).map_err(|e| format!("{path}: {e}"))
    }

    /// Index of the clip called `name`, if any.
    pub fn clip(&self, name: &str) -> Option<usize> {
        self.clips.iter().position(|c| c.name == name)
    }

    /// Parse a `.glb` from memory. Loud, specific errors — a bad
    /// export should say what to fix in Blender, not render garbage.
    pub fn parse(bytes: &[u8]) -> Result<ModelData, String> {
        let gltf = gltf::Gltf::from_slice(bytes).map_err(|e| format!("glb parse: {e}"))?;
        let blob = gltf.blob.as_deref();
        let buf = move |b: gltf::Buffer<'_>| -> Option<&[u8]> {
            match b.source() {
                gltf::buffer::Source::Bin => blob,
                // One file per model — re-export as .glb.
                gltf::buffer::Source::Uri(_) => None,
            }
        };

        // Walk the scene depth-first: assign OUR node indices in visit
        // order (parents before children — pose()'s one-pass contract),
        // decompose rest transforms, and pull each node's mesh into a
        // part. glTF indices can point anywhere; `map` translates.
        let scene = gltf
            .default_scene()
            .or_else(|| gltf.scenes().next())
            .ok_or("no scene in glb")?;
        let mut skeleton = Skeleton {
            parents: Vec::new(),
            rest: Vec::new(),
            names: Vec::new(),
        };
        let mut parts: Vec<PartData> = Vec::new();
        let mut map: Vec<Option<usize>> = vec![None; gltf.nodes().count()];
        for node in scene.nodes() {
            visit(node, NO_PARENT, &mut skeleton, &mut parts, &mut map, &buf)?;
        }

        // Animations → clips. Channels whose target node isn't in the
        // scene (glTF allows it) are skipped with a warning.
        let mut clips = Vec::new();
        for anim in gltf.animations() {
            let name = anim
                .name()
                .map(str::to_string)
                .unwrap_or_else(|| format!("clip{}", anim.index()));
            let mut clip = Clip3 { name, duration: 0.0, channels: Vec::new() };
            for ch in anim.channels() {
                let Some(node) = map[ch.target().node().index()] else {
                    eprintln!("[model] {}: channel targets a node outside the scene", clip.name);
                    continue;
                };
                let interp = ch.sampler().interpolation();
                let cubic = interp == gltf::animation::Interpolation::CubicSpline;
                if cubic {
                    eprintln!(
                        "[model] {}: CUBICSPLINE dropped to linear (export sampled/linear)",
                        clip.name
                    );
                }
                let reader = ch.reader(buf);
                let times: Vec<f32> = reader
                    .read_inputs()
                    .ok_or("animation input missing (external buffer? re-export as .glb)")?
                    .collect();
                // Cubic-spline output triples (in-tangent, value,
                // out-tangent): keep the middles, drop the tangents.
                let pick = |i: usize| if cubic { i * 3 + 1 } else { i };
                let keys = match reader
                    .read_outputs()
                    .ok_or("animation output missing (external buffer? re-export as .glb)")?
                {
                    gltf::animation::util::ReadOutputs::Translations(it) => {
                        let v: Vec<[f32; 3]> = it.collect();
                        Keys3::T(times.iter().enumerate().map(|(i, _)| to_v3(v[pick(i)])).collect())
                    }
                    gltf::animation::util::ReadOutputs::Scales(it) => {
                        let v: Vec<[f32; 3]> = it.collect();
                        Keys3::S(times.iter().enumerate().map(|(i, _)| to_v3(v[pick(i)])).collect())
                    }
                    gltf::animation::util::ReadOutputs::Rotations(it) => {
                        let v: Vec<[f32; 4]> = it.into_f32().collect();
                        Keys3::R(
                            times
                                .iter()
                                .enumerate()
                                .map(|(i, _)| {
                                    let q = v[pick(i)];
                                    Quat { x: q[0], y: q[1], z: q[2], w: q[3] }.norm()
                                })
                                .collect(),
                        )
                    }
                    gltf::animation::util::ReadOutputs::MorphTargetWeights(_) => continue,
                };
                if let Some(&last) = times.last() {
                    clip.duration = clip.duration.max(last);
                }
                clip.channels.push(Channel3 {
                    node,
                    step: interp == gltf::animation::Interpolation::Step,
                    times,
                    keys,
                });
            }
            clips.push(clip);
        }

        Ok(ModelData { parts, skeleton, clips })
    }

    /// Serialize to `.glb` bytes — the writer half of the pipeline,
    /// with two jobs: SEED assets from procedural geometry (`hogs
    /// genassets` authors each model's first .glb from the same code
    /// that is its runtime fallback, so file and fallback start
    /// equivalent and diverge only in Blender), and pin the parser
    /// with roundtrip tests. Geometry + hierarchy only — clips are
    /// dropped with a warning (keyframes are Blender's business).
    /// One part per node, at least one part.
    pub fn to_glb(&self) -> Result<Vec<u8>, String> {
        if !self.clips.is_empty() {
            eprintln!("[model] to_glb: {} clip(s) not serialized", self.clips.len());
        }
        if self.parts.is_empty() {
            return Err("no parts (glTF forbids empty meshes)".into());
        }
        let mut bin: Vec<u8> = Vec::new();
        let mut views: Vec<String> = Vec::new();
        let mut accs: Vec<String> = Vec::new();
        let mut meshes: Vec<String> = Vec::new();
        let mut node_mesh: Vec<Option<usize>> = vec![None; self.skeleton.len()];
        for p in &self.parts {
            if p.verts.is_empty() {
                return Err(format!("part '{}' has no vertices", p.name));
            }
            if p.node >= self.skeleton.len()
                || node_mesh[p.node].replace(meshes.len()).is_some()
            {
                return Err(format!("part '{}': bad or shared node {}", p.name, p.node));
            }
            // One tightly packed float VEC3 accessor per attribute.
            let mut push = |sel: &dyn Fn(&Vertex3) -> [f32; 3], minmax: bool| -> usize {
                let off = bin.len();
                let (mut lo, mut hi) = ([f32::MAX; 3], [f32::MIN; 3]);
                for v in &p.verts {
                    for (i, c) in sel(v).into_iter().enumerate() {
                        lo[i] = lo[i].min(c);
                        hi[i] = hi[i].max(c);
                        bin.extend_from_slice(&c.to_le_bytes());
                    }
                }
                views.push(format!(
                    r#"{{"buffer":0,"byteOffset":{off},"byteLength":{}}}"#,
                    bin.len() - off
                ));
                let mm = if minmax {
                    format!(
                        r#","min":[{},{},{}],"max":[{},{},{}]"#,
                        lo[0], lo[1], lo[2], hi[0], hi[1], hi[2]
                    )
                } else {
                    String::new()
                };
                accs.push(format!(
                    r#"{{"bufferView":{},"componentType":5126,"count":{},"type":"VEC3"{mm}}}"#,
                    views.len() - 1,
                    p.verts.len()
                ));
                accs.len() - 1
            };
            let pos = push(&|v| v.pos, true);
            let nrm = push(&|v| v.normal, false);
            let col = push(&|v| v.color, false);
            meshes.push(format!(
                r#"{{"name":{:?},"primitives":[{{"attributes":{{"POSITION":{pos},"NORMAL":{nrm},"COLOR_0":{col}}}}}]}}"#,
                p.name
            ));
        }

        let mut children: Vec<Vec<usize>> = vec![Vec::new(); self.skeleton.len()];
        let mut roots: Vec<usize> = Vec::new();
        for (i, &par) in self.skeleton.parents.iter().enumerate() {
            if par == NO_PARENT {
                roots.push(i);
            } else if par < i {
                children[par].push(i);
            } else {
                return Err("skeleton parents must precede children".into());
            }
        }
        if roots.is_empty() {
            return Err("no root nodes".into());
        }
        let mut nodes: Vec<String> = Vec::new();
        for i in 0..self.skeleton.len() {
            // {:?} on str/Vec<usize> is valid JSON for ASCII names.
            let mut n = format!(r#"{{"name":{:?}"#, self.skeleton.names[i]);
            if let Some(m) = node_mesh[i] {
                n.push_str(&format!(r#","mesh":{m}"#));
            }
            if !children[i].is_empty() {
                n.push_str(&format!(r#","children":{:?}"#, children[i]));
            }
            let r = self.skeleton.rest[i];
            if (r.t.x, r.t.y, r.t.z) != (0.0, 0.0, 0.0) {
                n.push_str(&format!(r#","translation":[{},{},{}]"#, r.t.x, r.t.y, r.t.z));
            }
            if (r.r.x, r.r.y, r.r.z, r.r.w) != (0.0, 0.0, 0.0, 1.0) {
                n.push_str(&format!(
                    r#","rotation":[{},{},{},{}]"#,
                    r.r.x, r.r.y, r.r.z, r.r.w
                ));
            }
            if (r.s.x, r.s.y, r.s.z) != (1.0, 1.0, 1.0) {
                n.push_str(&format!(r#","scale":[{},{},{}]"#, r.s.x, r.s.y, r.s.z));
            }
            n.push('}');
            nodes.push(n);
        }

        let json = format!(
            r#"{{"asset":{{"generator":"pm to_glb","version":"2.0"}},"scene":0,"scenes":[{{"nodes":{roots:?}}}],"nodes":[{}],"meshes":[{}],"accessors":[{}],"bufferViews":[{}],"buffers":[{{"byteLength":{}}}]}}"#,
            nodes.join(","),
            meshes.join(","),
            accs.join(","),
            views.join(","),
            bin.len()
        );
        Ok(glb_pack(json.into_bytes(), bin))
    }
}

/// Wrap a JSON chunk + BIN chunk in the GLB container.
fn glb_pack(mut json: Vec<u8>, mut bin: Vec<u8>) -> Vec<u8> {
    while json.len() % 4 != 0 {
        json.push(b' ');
    }
    while bin.len() % 4 != 0 {
        bin.push(0);
    }
    let mut out: Vec<u8> = Vec::new();
    out.extend_from_slice(b"glTF");
    out.extend_from_slice(&2u32.to_le_bytes());
    out.extend_from_slice(&((12 + 8 + json.len() + 8 + bin.len()) as u32).to_le_bytes());
    out.extend_from_slice(&(json.len() as u32).to_le_bytes());
    out.extend_from_slice(b"JSON");
    out.extend_from_slice(&json);
    out.extend_from_slice(&(bin.len() as u32).to_le_bytes());
    out.extend_from_slice(b"BIN\0");
    out.extend_from_slice(&bin);
    out
}

fn to_v3(a: [f32; 3]) -> Vec3 {
    vec3(a[0], a[1], a[2])
}

/// One DFS step of the scene walk (recursive — sibling order is the
/// authored order, and glTF forbids node cycles).
fn visit<'s, F>(
    node: gltf::Node<'_>,
    parent: usize,
    skeleton: &mut Skeleton,
    parts: &mut Vec<PartData>,
    map: &mut [Option<usize>],
    buf: &F,
) -> Result<(), String>
where
    F: Clone + for<'x> Fn(gltf::Buffer<'x>) -> Option<&'s [u8]>,
{
    let my = skeleton.parents.len();
    map[node.index()] = Some(my);
    let (t, r, s) = node.transform().decomposed();
    skeleton.parents.push(parent);
    skeleton.rest.push(Transform3 {
        t: vec3(t[0], t[1], t[2]),
        r: Quat { x: r[0], y: r[1], z: r[2], w: r[3] },
        s: vec3(s[0], s[1], s[2]),
    });
    let name = node
        .name()
        .map(str::to_string)
        .unwrap_or_else(|| format!("node{}", node.index()));
    if let Some(mesh) = node.mesh() {
        let verts = mesh_verts(&mesh, &name, buf)?;
        if !verts.is_empty() {
            parts.push(PartData { name: name.clone(), node: my, verts });
        }
    }
    skeleton.names.push(name);
    for child in node.children() {
        visit(child, my, skeleton, parts, map, buf)?;
    }
    Ok(())
}

/// All of a mesh's triangle primitives, de-indexed and flattened into
/// one `Vertex3` list. Color = `COLOR_0` × the material's
/// `baseColorFactor` (either alone works; solid materials need no
/// painting). Missing normals get flat per-face ones.
fn mesh_verts<'s, F>(mesh: &gltf::Mesh<'_>, name: &str, buf: &F) -> Result<Vec<Vertex3>, String>
where
    F: Clone + for<'x> Fn(gltf::Buffer<'x>) -> Option<&'s [u8]>,
{
    let mut verts: Vec<Vertex3> = Vec::new();
    for prim in mesh.primitives() {
        if prim.mode() != gltf::mesh::Mode::Triangles {
            return Err(format!("{name}: non-triangle primitive (triangulate on export)"));
        }
        let reader = prim.reader(buf.clone());
        if reader.read_joints(0).is_some() {
            eprintln!("[model] {name}: skinned mesh loaded RIGID (skinning not wired yet)");
        }
        let pos: Vec<[f32; 3]> = reader
            .read_positions()
            .ok_or_else(|| format!("{name}: no positions (external buffer? re-export as .glb)"))?
            .collect();
        let normals: Option<Vec<[f32; 3]>> = reader.read_normals().map(|it| it.collect());
        let colors: Option<Vec<[f32; 3]>> =
            reader.read_colors(0).map(|c| c.into_rgb_f32().collect());
        let base = prim.material().pbr_metallic_roughness().base_color_factor();
        let indices: Vec<u32> = match reader.read_indices() {
            Some(it) => it.into_u32().collect(),
            None => (0..pos.len() as u32).collect(),
        };
        for tri in indices.chunks_exact(3) {
            let [a, b, c] = [tri[0] as usize, tri[1] as usize, tri[2] as usize];
            // Flat normal fallback: the authored look here is faceted
            // boxes anyway, and it keeps hand-built exporters honest.
            let flat = {
                let (pa, pb, pc) = (to_v3(pos[a]), to_v3(pos[b]), to_v3(pos[c]));
                let n = (pb - pa).cross(pc - pa);
                if n.len() > 1e-9 { n.norm() } else { Vec3::UP }
            };
            for &i in &[a, b, c] {
                let n = normals.as_ref().map_or(flat, |ns| to_v3(ns[i]));
                let vc = colors.as_ref().map_or([1.0, 1.0, 1.0], |cs| cs[i]);
                verts.push(Vertex3 {
                    pos: pos[i],
                    normal: [n.x, n.y, n.z],
                    color: [vc[0] * base[0], vc[1] * base[1], vc[2] * base[2]],
                });
            }
        }
    }
    Ok(verts)
}

/// The importer + runtime, pinned against a hand-built two-bone GLB:
/// a body with a child arm, one LINEAR rotation clip. Covers the
/// container parse, hierarchy order, rest pose, de-index + flat
/// normals, material color fallback, keyframe bracketing, nlerp, and
/// wrapping — everything short of the GPU upload.
#[cfg(test)]
mod tests {
    use super::*;

    /// `(0,0,0),(1,0,0),(0,1,0)` — one CCW triangle in the XY plane.
    const TRI: [f32; 9] = [0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0];

    fn glb() -> Vec<u8> {
        // bin: 3 positions (36 B), 2 key times (8 B), 2 rotations
        // (32 B) — identity, then 90° about +Z.
        let mut bin: Vec<u8> = Vec::new();
        let h = std::f32::consts::FRAC_1_SQRT_2;
        for f in TRI
            .iter()
            .copied()
            .chain([0.0f32, 1.0])
            .chain([0.0, 0.0, 0.0, 1.0])
            .chain([0.0, 0.0, h, h])
        {
            bin.extend_from_slice(&f.to_le_bytes());
        }
        let json = format!(
            r#"{{"asset":{{"version":"2.0"}},"scene":0,"scenes":[{{"nodes":[0]}}],
"nodes":[{{"name":"body","mesh":0,"children":[1]}},{{"name":"arm","mesh":1,"translation":[0.0,1.0,0.0]}}],
"meshes":[{{"primitives":[{{"attributes":{{"POSITION":0}}}}]}},{{"primitives":[{{"attributes":{{"POSITION":0}},"material":0}}]}}],
"materials":[{{"pbrMetallicRoughness":{{"baseColorFactor":[1.0,0.0,0.0,1.0]}}}}],
"accessors":[
 {{"bufferView":0,"componentType":5126,"count":3,"type":"VEC3","min":[0.0,0.0,0.0],"max":[1.0,1.0,0.0]}},
 {{"bufferView":1,"componentType":5126,"count":2,"type":"SCALAR","min":[0.0],"max":[1.0]}},
 {{"bufferView":2,"componentType":5126,"count":2,"type":"VEC4"}}],
"bufferViews":[
 {{"buffer":0,"byteOffset":0,"byteLength":36}},
 {{"buffer":0,"byteOffset":36,"byteLength":8}},
 {{"buffer":0,"byteOffset":44,"byteLength":32}}],
"buffers":[{{"byteLength":{}}}],
"animations":[{{"name":"wave","samplers":[{{"input":1,"output":2,"interpolation":"LINEAR"}}],
"channels":[{{"sampler":0,"target":{{"node":1,"path":"rotation"}}}}]}}]}}"#,
            bin.len()
        );
        glb_pack(json.into_bytes(), bin)
    }

    #[test]
    fn parses_parts_hierarchy_and_colors() {
        let m = ModelData::parse(&glb()).expect("parse");
        assert_eq!(m.skeleton.len(), 2);
        assert_eq!(m.skeleton.parents, vec![NO_PARENT, 0], "DFS order: parent first");
        assert_eq!(m.skeleton.names, vec!["body", "arm"]);
        assert!((m.skeleton.rest[1].t.y - 1.0).abs() < 1e-6, "arm rest offset");
        assert_eq!(m.parts.len(), 2);
        assert_eq!(m.parts[1].node, 1, "part poses by its own node");
        assert_eq!(m.parts[0].verts.len(), 3, "de-indexed triangle");
        let v = &m.parts[0].verts[0];
        assert_eq!(v.color, [1.0, 1.0, 1.0], "no material = white");
        assert_eq!(v.normal, [0.0, 0.0, 1.0], "flat normal generated (CCW in XY)");
        let v = &m.parts[1].verts[0];
        assert_eq!(v.color, [1.0, 0.0, 0.0], "baseColorFactor with no COLOR_0");
    }

    #[test]
    fn clip_samples_lerp_and_wrap() {
        let m = ModelData::parse(&glb()).expect("parse");
        let clip = &m.clips[m.clip("wave").expect("clip by name")];
        assert!((clip.duration - 1.0).abs() < 1e-6);

        let arm_tip = |t: f32| {
            let mut mats = Vec::new();
            m.skeleton.pose(clip, t, &mut mats);
            mats[1].transform_point(vec3(1.0, 0.0, 0.0))
        };
        // t=0: identity rotation, just the (0,1,0) offset.
        let p = arm_tip(0.0);
        assert!((p.x - 1.0).abs() < 1e-4 && (p.y - 1.0).abs() < 1e-4, "rest at t=0, got {p:?}");
        // t=1: 90° about +Z takes local +X to world +Y.
        let p = arm_tip(1.0 - 1e-6);
        assert!(p.x.abs() < 1e-2 && (p.y - 2.0).abs() < 1e-2, "90° at t=1, got {p:?}");
        // Midway: nlerp of a 90° arc ≈ 45°.
        let h = std::f32::consts::FRAC_1_SQRT_2;
        let p = arm_tip(0.5);
        assert!(
            (p.x - h).abs() < 1e-2 && (p.y - 1.0 - h).abs() < 1e-2,
            "45° at t=0.5, got {p:?}"
        );
        // Wrapping: one full duration later is the same pose.
        let (a, b) = (arm_tip(0.25), arm_tip(1.25));
        assert!((a.x - b.x).abs() < 1e-4 && (a.y - b.y).abs() < 1e-4, "clip wraps");
    }

    /// The writer half: what `to_glb` emits, `parse` reads back
    /// verbatim — names, hierarchy, rest transforms, and every vertex
    /// bit-exact (colors picked exactly representable in f32).
    #[test]
    fn writer_roundtrips_through_parser() {
        let cube = |c: (f32, f32, f32)| {
            crate::gpu3d::bake(
                &crate::gpu3d::box_tris(vec3(-0.5, 0.0, -0.5), vec3(0.5, 1.0, 0.5)),
                c,
            )
        };
        let data = ModelData {
            parts: vec![
                PartData { name: "body".into(), node: 0, verts: cube((1.0, 1.0, 1.0)) },
                PartData { name: "head".into(), node: 1, verts: cube((0.5, 0.25, 0.125)) },
            ],
            skeleton: Skeleton {
                parents: vec![NO_PARENT, 0],
                rest: vec![
                    Transform3::IDENTITY,
                    Transform3 { t: vec3(0.0, 1.0, 0.25), ..Transform3::IDENTITY },
                ],
                names: vec!["body".into(), "head".into()],
            },
            clips: Vec::new(),
        };
        let back = ModelData::parse(&data.to_glb().expect("write")).expect("reparse");
        assert_eq!(back.skeleton.names, data.skeleton.names);
        assert_eq!(back.skeleton.parents, data.skeleton.parents);
        assert!((back.skeleton.rest[1].t.y - 1.0).abs() < 1e-9, "rest offset survives");
        assert_eq!(back.parts.len(), 2);
        for (a, b) in data.parts.iter().zip(&back.parts) {
            assert_eq!(a.name, b.name);
            assert_eq!(a.node, b.node);
            assert_eq!(a.verts.len(), b.verts.len());
            for (va, vb) in a.verts.iter().zip(&b.verts) {
                assert_eq!(va.pos, vb.pos);
                assert_eq!(va.normal, vb.normal);
                assert_eq!(va.color, vb.color, "baked color roundtrips bit-exact");
            }
        }
    }
}
