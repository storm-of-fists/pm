//! Cameras as ENTITIES attached to other entities — pm's deliberate
//! step into flecs-style relationship territory.
//!
//! Each camera is its own entity carrying a [`CamRig`] component whose
//! `target` field names the entity it's mounted on. That's the
//! relationship: one car can carry a chase cam, a hood cam, a backup
//! cam and a side cam at once, each with its own mount offsets, FOV
//! and spring stiffness, and switching the screen between them is one
//! [`camera_use`] call.
//!
//! Games never touch the module's pools directly; the surface is three
//! functions plus the published `"cam.view"` single:
//!
//! - [`camera_follow`] — once per tracked entity: a sampler closure
//!   that reports where the entity is each tick (from smoothed DRAW
//!   state, never raw fixed-step state, or the camera inherits its
//!   stutter). Registers the anchor task for you, owned by the module;
//! - [`camera_attach`] — any number of times per entity: mounts a rig,
//!   returns the camera's id;
//! - [`camera_use`] — point the screen at one of them.
//!
//! There is no setup call: the first of these to touch a `Pm` installs
//! the module's one-time machinery (pools, `"cam.view"`, the spring
//! task) by itself, guarded by the `"cam.manager"` single — interacting
//! with a single entity's camera bootstraps the global manager. All of
//! it is owned by `module_add("camera")`, so `module_remove("camera")`
//! is still the one-call teardown (and the next attach reinstalls
//! fresh).
//!
//! All callable mid-game from inside a task (the kernel merges runtime
//! `task_add`s at end of tick), so "rig the car the moment the server
//! tells us which one is ours" is the normal flow.
//!
//! Per tick, in priority order: follow tasks write `"cam.anchor"`
//! (CAMERA_PRIO - 1) → the spring task moves every rig toward its
//! mount and publishes the active one to `"cam.view"` (CAMERA_PRIO) →
//! rendering reads [`CamView`], applies [`CamView::fov_deg`], and
//! calls [`CamView::matrix`].

use crate::Id;
use crate::kernel::Pm;
use crate::math::{Mat4, Vec3, vec3};

/// Priority of the camera module's spring task. Anchor follow tasks run
/// at `CAMERA_PRIO - 1.0`; read `"cam.view"` above `CAMERA_PRIO`.
pub const CAMERA_PRIO: f32 = 35.0;

/// Where a tracked entity is right now — pool `"cam.anchor"`, keyed by
/// the TRACKED entity's id, written each tick by [`camera_follow`]'s
/// sampler.
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
    /// The entity this camera is mounted on (set by [`camera_attach`]).
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

    /// On the hood looking down the road. Rigid.
    pub fn hood() -> Self {
        Self::new(vec3(0.0, 1.1, 1.3), vec3(0.0, 0.9, 20.0), 90.0, 0.0)
    }

    /// Rear-facing backup cam: tail-mounted, very wide. Rigid.
    pub fn backup() -> Self {
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

/// Single `"cam.view"`: the active camera's smoothed eye/target/FOV.
pub struct CamView {
    /// Which CAMERA drives the screen (`None` until [`camera_use`]).
    pub active: Option<Id>,
    pub eye: Vec3,
    pub target: Vec3,
    /// The active rig's FOV; renderers apply this each frame.
    pub fov_deg: f32,
}

impl Default for CamView {
    fn default() -> Self {
        Self { active: None, eye: Vec3::ZERO, target: Vec3::ZERO, fov_deg: 100.0 }
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

/// Single `"cam.manager"`: the module's one-time-setup latch. First
/// created (default `installed: false`) by whichever camera call
/// touches the `Pm` first; torn down with the module, so a reinstall
/// after `module_remove("camera")` starts clean.
#[derive(Default)]
pub struct CamManager {
    pub installed: bool,
}

/// Install the camera module: the `"cam.anchor"` / `"cam.rig"` pools,
/// the `"cam.view"` single, and the spring task that ties them.
/// Idempotent, and called for you by [`camera_follow`] /
/// [`camera_attach`] / [`camera_use`] — games only need it explicitly
/// to control install timing.
pub fn camera_install(pm: &mut Pm) {
    let _ = pm.module_add("camera", |pm| {
        let mgr = pm.single::<CamManager>("cam.manager");
        if std::mem::replace(&mut mgr.borrow_mut().installed, true) {
            return;
        }
        let anchors = pm.pool::<CamAnchor>("cam.anchor");
        let rigs = pm.pool::<CamRig>("cam.rig");
        let view = pm.single::<CamView>("cam.view");
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

/// Feed `"cam.anchor"` for `target` from a game-supplied sampler each
/// tick — e.g. read the smooth-predicted draw pool. Return `None` to
/// skip a tick (entity not visible yet). The task this registers is
/// owned by the camera module, so `module_remove("camera")` stops it.
pub fn camera_follow(
    pm: &mut Pm,
    target: Id,
    mut sample: impl FnMut(&mut Pm) -> Option<CamAnchor> + 'static,
) {
    camera_install(pm);
    let _ = pm.module_add("camera", |pm| {
        let anchors = pm.pool::<CamAnchor>("cam.anchor");
        pm.task_add(&format!("cam.follow.{}", target.0), CAMERA_PRIO - 1.0, 0.0, move |pm| {
            if let Some(a) = sample(pm) {
                anchors.borrow_mut().add(target, a);
            }
        });
    });
}

/// Mount a camera on `target`. The camera is its own entity; the
/// returned id is what you hand to [`camera_use`] (or `id_remove` to
/// take the camera off).
pub fn camera_attach(pm: &mut Pm, target: Id, mut rig: CamRig) -> Id {
    camera_install(pm);
    let cam = pm.id_add();
    rig.target = target;
    pm.pool::<CamRig>("cam.rig").borrow_mut().add(cam, rig);
    cam
}

/// Make this camera drive the screen.
pub fn camera_use(pm: &mut Pm, cam: Id) {
    camera_install(pm);
    pm.single::<CamView>("cam.view").borrow_mut().active = Some(cam);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn static_anchor(pm: &mut Pm) -> Id {
        let id = pm.id_add();
        camera_follow(pm, id, move |_pm| {
            Some(CamAnchor { pos: vec3(10.0, 0.0, 5.0), fwd: vec3(0.0, 0.0, 1.0) })
        });
        id
    }

    #[test]
    fn chase_converges_on_a_static_anchor() {
        let mut pm = Pm::new();
        // No install call anywhere: the first camera_* touch bootstraps
        // the module (the "cam.manager" latch).
        let car = static_anchor(&mut pm);
        let cam = camera_attach(&mut pm, car, CamRig::chase());
        camera_use(&mut pm, cam);

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
        let car = static_anchor(&mut pm);
        let _chase = camera_attach(&mut pm, car, CamRig::chase());
        let backup = camera_attach(&mut pm, car, CamRig::backup());
        camera_use(&mut pm, backup);

        // Rigid (stiffness 0): exact after a single tick, no settling.
        pm.loop_once(1.0 / 60.0);
        let view = pm.single::<CamView>("cam.view");
        let v = view.borrow();
        // fwd = +z, right = UP×fwd = +x; backup eye (0, 1.5, -1.9).
        assert!((v.eye - vec3(10.0, 1.5, 3.1)).len() < 1e-4, "eye = {:?}", v.eye);
        assert!((v.target - vec3(10.0, 0.4, -7.0)).len() < 1e-4, "target = {:?}", v.target);
        assert!((v.fov_deg - 120.0).abs() < 1e-6);
    }

    #[test]
    fn side_mount_hangs_off_the_right_flank() {
        let mut pm = Pm::new();
        let car = static_anchor(&mut pm);
        let side = camera_attach(&mut pm, car, CamRig::side());
        camera_use(&mut pm, side);
        pm.loop_once(1.0 / 60.0);
        let view = pm.single::<CamView>("cam.view");
        let v = view.borrow();
        // right = +x at fwd = +z: eye x = 10 + 1.7.
        assert!((v.eye.x - 11.7).abs() < 1e-4, "eye = {:?}", v.eye);
    }

    #[test]
    fn module_remove_tears_the_camera_down() {
        let mut pm = Pm::new();
        let car = static_anchor(&mut pm);
        let cam = camera_attach(&mut pm, car, CamRig::chase());
        camera_use(&mut pm, cam);
        pm.module_remove("camera");
        pm.loop_once(1.0 / 60.0);
        // Everything the module owned is gone — including the manager
        // latch, so the next attach reinstalls a working module.
        let cam = camera_attach(&mut pm, car, CamRig::backup());
        assert_eq!(pm.pool::<CamRig>("cam.rig").borrow().len(), 1);
        camera_follow(&mut pm, car, move |_pm| {
            Some(CamAnchor { pos: Vec3::ZERO, fwd: vec3(0.0, 0.0, 1.0) })
        });
        camera_use(&mut pm, cam);
        pm.loop_once(1.0 / 60.0);
        assert!(pm.single::<CamView>("cam.view").borrow().ready());
    }
}
