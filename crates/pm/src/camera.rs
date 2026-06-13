//! Cameras as ENTITIES attached to other entities — pm's deliberate
//! step into flecs-style relationship territory.
//!
//! Each camera is its own entity carrying a [`CamRig`] component whose
//! `target` field names the entity it's mounted on. That's the
//! relationship: one car can carry a chase cam, a rear cam and a side
//! cam at once, each with its own mount offsets, FOV and spring
//! stiffness, and switching the screen between them is one call.
//!
//! ## The surface
//!
//! Setup goes through [`camera_track`], which fixes the tracked entity
//! once and hands back a [`CameraRack`]:
//!
//! ```ignore
//! let mut rack = camera_track(&mut pm, car, move |_| sample_draw_state());
//! let chase = rack.mount(CamRig::chase());
//! rack.mount(CamRig::rear());
//! rack.mount(CamRig::side());
//! rack.show(chase);
//! ```
//!
//! `track` does the two `pm`-touching things — register the anchor
//! sampler task, and (per `mount`) allocate a camera entity — because
//! making entities and tasks is the kernel's job. Everything *after*
//! setup runs through the [`CamManager`] single and never touches
//! `pm`: capture it with [`camera_manager`], then
//!
//! ```ignore
//! mgr.borrow_mut().show_index(2);   // 0/1/2 = the mount order
//! mgr.borrow_mut().toggle_panini(); // presentation lives on cam.view
//! ```
//!
//! That split is the rule pm reaches for: **`pm` is for lifecycle —
//! ids, tasks, modules; pools and singles are for state and behavior.**
//! The per-frame system is all handles, no kernel.
//!
//! The module installs itself the first time any of these touch a `Pm`
//! (guarded by the `"cam.manager"` single); everything is owned by
//! `module_add("camera")`, so `module_remove("camera")` is the one-call
//! teardown and the next `track` reinstalls clean.
//!
//! Per tick, in priority order: the anchor sampler writes `"cam.anchor"`
//! (CAMERA_PRIO - 1) → the spring task moves every rig toward its mount
//! and publishes the active one to `"cam.view"` (CAMERA_PRIO) →
//! rendering reads [`CamView`] for the matrix, FOV and panini flag.

use crate::Id;
use crate::kernel::{Pm, Single};
use crate::math::{Mat4, Vec3, vec3};

/// Priority of the camera module's spring task. The anchor sampler runs
/// at `CAMERA_PRIO - 1.0`; read `"cam.view"` above `CAMERA_PRIO`.
pub const CAMERA_PRIO: f32 = 35.0;

/// Where a tracked entity is right now — pool `"cam.anchor"`, keyed by
/// the TRACKED entity's id, written each tick by the sampler closure
/// passed to [`camera_track`].
#[derive(Clone, Copy, Default)]
pub struct CamAnchor {
    pub pos: Vec3,
    /// Unit facing direction on the ground plane (or wherever).
    pub fwd: Vec3,
}

/// A camera mounted on an entity. Lives in pool `"cam.rig"` under the
/// CAMERA's own id; `target` names the entity it follows (the
/// relationship). Mount offsets are in the anchor's frame:
/// x = right (`UP × fwd`, matching `Mat4::look_at`), y = up, z = fwd.
#[derive(Clone, Copy)]
pub struct CamRig {
    /// The entity this camera is mounted on (set by [`CameraRack::mount`]).
    pub target: Id,
    /// Eye offset from the anchor, in its frame.
    pub eye: Vec3,
    /// Look-at offset from the anchor, in its frame.
    pub look: Vec3,
    /// Horizontal FOV this camera wants; published to `"cam.view"`
    /// while active so the renderer can follow.
    pub fov_deg: f32,
    /// Spring stiffness (1/s); higher = tighter follow.
    /// 0 = rigid mount, welded to the entity.
    pub stiffness: f32,
    eye_w: Vec3,
    look_w: Vec3,
    seeded: bool,
}

impl CamRig {
    pub fn new(eye: Vec3, look: Vec3, fov_deg: f32, stiffness: f32) -> Self {
        Self {
            target: Id(0),
            eye,
            look,
            fov_deg,
            stiffness,
            eye_w: Vec3::ZERO,
            look_w: Vec3::ZERO,
            seeded: false,
        }
    }

    /// Classic third-person chase: behind, above, sprung.
    pub fn chase() -> Self {
        Self::new(vec3(0.0, 3.6, -9.0), vec3(0.0, 1.0, 4.0), 100.0, 6.0)
    }

    /// Rear-facing cam: tail-mounted, looking back, very wide. Rigid.
    pub fn rear() -> Self {
        Self::new(vec3(0.0, 1.5, -1.9), vec3(0.0, 0.4, -12.0), 120.0, 0.0)
    }

    /// Broadcast-style wheel cam off the right flank, looking forward.
    /// Rigid.
    pub fn side() -> Self {
        Self::new(vec3(1.7, 0.7, 0.4), vec3(1.7, 0.6, 14.0), 85.0, 0.0)
    }
}

impl Default for CamRig {
    fn default() -> Self {
        Self::chase()
    }
}

/// Single `"cam.view"`: the active camera's smoothed presentation —
/// eye/target plus the FOV and panini flag the renderer applies each
/// frame.
pub struct CamView {
    /// Which CAMERA drives the screen (`None` until something is shown).
    pub active: Option<Id>,
    pub eye: Vec3,
    pub target: Vec3,
    /// The active rig's FOV; renderers apply this each frame.
    pub fov_deg: f32,
    /// Whether the panini look is on — toggled live, read by the
    /// renderer (the house default is on).
    pub panini: bool,
}

impl Default for CamView {
    fn default() -> Self {
        Self { active: None, eye: Vec3::ZERO, target: Vec3::ZERO, fov_deg: 100.0, panini: true }
    }
}

impl CamView {
    pub fn matrix(&self) -> Mat4 {
        Mat4::look_at(self.eye, self.target, Vec3::UP)
    }

    /// True once the active camera has produced a usable view.
    pub fn ready(&self) -> bool {
        self.active.is_some() && self.eye != self.target
    }
}

/// Single `"cam.manager"`: the camera module's runtime API surface. It
/// caches a handle to `"cam.view"` and the ordered list of mounted
/// cameras, so switching and toggling are plain methods that go through
/// stored handles — no `Pm` in per-frame code. Created (empty) by the
/// first camera call; its `view` being `Some` is the install latch.
#[derive(Default)]
pub struct CamManager {
    view: Option<Single<CamView>>,
    cams: Vec<Id>,
}

impl CamManager {
    /// The cameras mounted so far, in mount order (what `show_index`
    /// indexes — e.g. number keys 1/2/3).
    pub fn cams(&self) -> &[Id] {
        &self.cams
    }

    /// Which camera currently drives the screen.
    pub fn active(&self) -> Option<Id> {
        self.view().active
    }

    /// Make `cam` drive the screen.
    pub fn show(&self, cam: Id) {
        self.view().active = Some(cam);
    }

    /// Show the i-th mounted camera (mount order). No-op if out of range.
    pub fn show_index(&self, i: usize) {
        if let Some(&cam) = self.cams.get(i) {
            self.show(cam);
        }
    }

    /// Flip the panini look on the active view.
    pub fn toggle_panini(&self) {
        let mut v = self.view();
        v.panini = !v.panini;
    }

    fn view(&self) -> std::cell::RefMut<'_, CamView> {
        self.view.as_ref().expect("camera module not installed").borrow_mut()
    }
}

/// Fetch the camera manager single (installing the module if needed),
/// to capture into a task for live camera switching / panini toggling.
pub fn camera_manager(pm: &mut Pm) -> Single<CamManager> {
    camera_install(pm);
    pm.single::<CamManager>("cam.manager")
}

/// Install the camera module: the `"cam.anchor"` / `"cam.rig"` pools,
/// the `"cam.view"` and `"cam.manager"` singles, and the spring task
/// that ties them. Idempotent and called for you by [`camera_track`] /
/// [`camera_manager`] — games only need it explicitly to control
/// install timing.
pub fn camera_install(pm: &mut Pm) {
    let _ = pm.module_add("camera", |pm| {
        let mgr = pm.single::<CamManager>("cam.manager");
        if mgr.borrow().view.is_some() {
            return; // already installed
        }
        let anchors = pm.pool::<CamAnchor>("cam.anchor");
        let rigs = pm.pool::<CamRig>("cam.rig");
        let view = pm.single::<CamView>("cam.view");
        mgr.borrow_mut().view = Some(view.clone());

        pm.task_add("camera", CAMERA_PRIO, 0.0, move |pm| {
            let dt = pm.loop_dt();
            let mut v = view.borrow_mut();
            let anchors = anchors.borrow();
            for (cam_id, mut r) in rigs.borrow_mut().iter_mut() {
                let Some(a) = anchors.get(r.target) else { continue };
                let fwd = a.fwd.norm();
                let right = Vec3::UP.cross(fwd).norm();
                let mount = |o: Vec3| a.pos + right * o.x + Vec3::UP * o.y + fwd * o.z;
                let want_eye = mount(r.eye);
                let want_look = mount(r.look);
                if r.seeded && r.stiffness > 0.0 {
                    // Frame-rate independent spring: same convergence
                    // per second whatever the loop rate.
                    let k = 1.0 - (-r.stiffness * dt).exp();
                    let (e, l) = (r.eye_w, r.look_w);
                    r.eye_w = e + (want_eye - e) * k;
                    r.look_w = l + (want_look - l) * k;
                } else {
                    r.eye_w = want_eye;
                    r.look_w = want_look;
                    r.seeded = true;
                }
                if v.active == Some(cam_id) {
                    v.eye = r.eye_w;
                    v.target = r.look_w;
                    v.fov_deg = r.fov_deg;
                }
            }
        });
    });
}

/// Start tracking `target`: install the module, register a sampler task
/// (priority `CAMERA_PRIO - 1.0`) that feeds `"cam.anchor"` from the
/// game each tick — return `None` to skip a tick (entity not visible
/// yet) — and hand back a [`CameraRack`] to mount cameras on it. The
/// sampler usually reads the smoothed DRAW pool, never raw fixed-step
/// state, or the camera inherits its stutter.
pub fn camera_track<'a>(
    pm: &'a mut Pm,
    target: Id,
    mut sample: impl FnMut(&mut Pm) -> Option<CamAnchor> + 'static,
) -> CameraRack<'a> {
    camera_install(pm);
    let _ = pm.module_add("camera", |pm| {
        let anchors = pm.pool::<CamAnchor>("cam.anchor");
        pm.task_add(&format!("cam.follow.{}", target.0), CAMERA_PRIO - 1.0, 0.0, move |pm| {
            if let Some(a) = sample(pm) {
                anchors.borrow_mut().add(target, a);
            }
        });
    });
    CameraRack { pm, target }
}

/// Builder over a tracked entity: mount cameras on it and pick the
/// active one. Holds `&mut Pm` for the duration of setup (mounting
/// allocates camera entities), so do all the mounting in one block.
pub struct CameraRack<'a> {
    pm: &'a mut Pm,
    target: Id,
}

impl CameraRack<'_> {
    /// Mount a camera on the tracked entity. Returns the camera's id
    /// (hand to [`CamManager::show`], or `id_remove` to take it off).
    pub fn mount(&mut self, mut rig: CamRig) -> Id {
        let cam = self.pm.id_add();
        rig.target = self.target;
        self.pm.pool::<CamRig>("cam.rig").borrow_mut().add(cam, rig);
        self.pm.single::<CamManager>("cam.manager").borrow_mut().cams.push(cam);
        cam
    }

    /// Make `cam` drive the screen now.
    pub fn show(&mut self, cam: Id) {
        self.pm.single::<CamManager>("cam.manager").borrow().show(cam);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track_static(pm: &mut Pm, id: Id) -> CameraRack<'_> {
        camera_track(pm, id, move |_pm| {
            Some(CamAnchor { pos: vec3(10.0, 0.0, 5.0), fwd: vec3(0.0, 0.0, 1.0) })
        })
    }

    #[test]
    fn chase_converges_on_a_static_anchor() {
        let mut pm = Pm::new();
        // No install call anywhere: the first camera_* touch bootstraps
        // the module (the "cam.manager" latch).
        let car = pm.id_add();
        let cam = {
            let mut rack = track_static(&mut pm, car);
            let cam = rack.mount(CamRig::chase());
            rack.show(cam);
            cam
        };
        assert_eq!(camera_manager(&mut pm).borrow().active(), Some(cam));

        for _ in 0..240 {
            pm.loop_once(1.0 / 60.0);
        }
        let view = pm.single::<CamView>("cam.view");
        let v = view.borrow();
        assert!(v.ready());
        // Eye should settle at anchor - fwd*9 + up*3.6: (10, 3.6, -4).
        assert!((v.eye - vec3(10.0, 3.6, -4.0)).len() < 0.05, "eye = {:?}", v.eye);
        assert!((v.target - vec3(10.0, 1.0, 9.0)).len() < 0.05, "target = {:?}", v.target);
        assert!((v.fov_deg - 100.0).abs() < 1e-6);
    }

    #[test]
    fn rigid_mounts_weld_and_switching_swaps_fov() {
        let mut pm = Pm::new();
        let car = pm.id_add();
        let rear = {
            let mut rack = track_static(&mut pm, car);
            rack.mount(CamRig::chase());
            let rear = rack.mount(CamRig::rear());
            rack.show(rear);
            rear
        };

        // Rigid (stiffness 0): exact after a single tick, no settling.
        pm.loop_once(1.0 / 60.0);
        let view = pm.single::<CamView>("cam.view");
        let v = view.borrow();
        assert_eq!(v.active, Some(rear));
        // fwd = +z, right = UP×fwd = +x; rear eye (0, 1.5, -1.9).
        assert!((v.eye - vec3(10.0, 1.5, 3.1)).len() < 1e-4, "eye = {:?}", v.eye);
        assert!((v.target - vec3(10.0, 0.4, -7.0)).len() < 1e-4, "target = {:?}", v.target);
        assert!((v.fov_deg - 120.0).abs() < 1e-6);
    }

    #[test]
    fn show_index_follows_mount_order() {
        let mut pm = Pm::new();
        let car = pm.id_add();
        let (chase, side) = {
            let mut rack = track_static(&mut pm, car);
            let chase = rack.mount(CamRig::chase());
            rack.mount(CamRig::rear());
            let side = rack.mount(CamRig::side());
            (chase, side)
        };
        let mgr = camera_manager(&mut pm);
        mgr.borrow().show_index(0);
        assert_eq!(mgr.borrow().active(), Some(chase));
        mgr.borrow().show_index(2);
        assert_eq!(mgr.borrow().active(), Some(side));
        mgr.borrow().show_index(9); // out of range: no-op
        assert_eq!(mgr.borrow().active(), Some(side));
    }

    #[test]
    fn toggle_panini_flips_the_view_flag() {
        let mut pm = Pm::new();
        let mgr = camera_manager(&mut pm);
        let view = pm.single::<CamView>("cam.view");
        assert!(view.borrow().panini); // house default
        mgr.borrow().toggle_panini();
        assert!(!view.borrow().panini);
        mgr.borrow().toggle_panini();
        assert!(view.borrow().panini);
    }

    #[test]
    fn module_remove_tears_the_camera_down() {
        let mut pm = Pm::new();
        let car = pm.id_add();
        {
            let mut rack = track_static(&mut pm, car);
            let cam = rack.mount(CamRig::chase());
            rack.show(cam);
        }
        pm.module_remove("camera");
        pm.loop_once(1.0 / 60.0);
        // Everything the module owned is gone — including the manager
        // latch, so the next track reinstalls a working module.
        assert_eq!(pm.pool::<CamRig>("cam.rig").borrow().len(), 0);
        assert_eq!(pm.pool::<CamAnchor>("cam.anchor").borrow().len(), 0);
        let cam = {
            let mut rack = track_static(&mut pm, car);
            let cam = rack.mount(CamRig::rear());
            rack.show(cam);
            cam
        };
        assert_eq!(pm.pool::<CamRig>("cam.rig").borrow().len(), 1);
        pm.loop_once(1.0 / 60.0);
        let view = pm.single::<CamView>("cam.view");
        assert!(view.borrow().ready());
        assert_eq!(view.borrow().active, Some(cam));
    }
}
