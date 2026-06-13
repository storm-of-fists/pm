//! Drive's 3D player client: pm::camera rigs attached to your car
//! entity (cameras are entities related to the car by `CamRig.target`),
//! the whole field rendered through pm_sdl::gpu3d's panini projection.
//!
//! WASD drives; 1-4 switch cameras (chase / hood / backup / side, each
//! with its own FOV); P toggles panini; Esc quits.

use pm::{CamAnchor, CamRig, CamView, Id, Mat4, Pm, Predictor, Vec3, camera_attach, camera_follow, camera_use, vec3};
use pm_sdl::gpu3d::{Renderer3d, bake, box_tris, checker_ground, panini_for_fov};
use pm_sdl::sdl3;
use sdl3::event::Event;
use sdl3::keyboard::Scancode;

use pm::{NetInput, NetStatus};

use crate::client::{Stats, add_client_tasks, connect};
use crate::common::*;

const W: u32 = 1280;
const H: u32 = 800;

fn car_model(c: &Car) -> Mat4 {
    Mat4::translate(vec3(c.x, 0.0, c.z)) * Mat4::rot_y(c.heading)
}

#[allow(dead_code)] // pause-menu stub, not wired up yet
pub struct InGameMenu {
    pub paused: bool
}

/// Single `"camctl"`: the car's camera rack (ids from `camera_attach`,
/// C cycles through them) plus the panini toggle.
struct CamCtl {
    cams: Vec<Id>,
    active: usize,
    panini: bool,
}

impl Default for CamCtl {
    fn default() -> Self {
        Self { cams: Vec::new(), active: 0, panini: true }
    }
}

pub fn run() {
    let mut pm = Pm::new();
    let car = pm.pool::<Car>("car");
    let draw = pm.pool::<Car>("car_draw");
    let mut net = pm::NetClient::new();
    net.pool_sync("car", &car);
    let quic = connect(&net);
    eprintln!("connecting to {ADDR} ...");
    add_client_tasks(&mut pm, quic, net, &car, &draw);

    let cmd = pm.single::<NetInput<Drive>>("net.input");
    let status = pm.single::<NetStatus>("net.status");
    let stats = pm.single::<Stats>("stats");
    let pred = pm.single::<Predictor<Car, Drive>>("pred");
    let ctl = pm.single::<CamCtl>("camctl");
    let draw_pool = draw.clone();

    let (mut window, mut pump, refresh) =
        pm_sdl::window("pm drive — wasd, c cam, p panini, esc quits", W, H);
    let mut r3d = Renderer3d::new(&window).expect("renderer");
    r3d.fog_distance = 160.0;
    let ground = r3d
        .upload_mesh(&checker_ground(12, 8.0, (0.22, 0.25, 0.30), (0.15, 0.17, 0.21)))
        .expect("ground");
    // Car: body + cabin, authored facing +z, baked white so the peer
    // color arrives as a per-draw tint.
    let body = r3d
        .upload_mesh(&bake(&box_tris(vec3(-0.9, 0.15, -1.7), vec3(0.9, 0.95, 1.7)), (1.0, 1.0, 1.0)))
        .expect("body");
    let cabin = r3d
        .upload_mesh(&bake(&box_tris(vec3(-0.7, 0.95, -1.1), vec3(0.7, 1.55, 0.45)), (1.0, 1.0, 1.0)))
        .expect("cabin");
    // Arena walls: four long boxes on the perimeter.
    let wall_x = r3d
        .upload_mesh(&bake(
            &box_tris(vec3(-0.4, 0.0, -ARENA - 0.8), vec3(0.4, 1.4, ARENA + 0.8)),
            (0.45, 0.40, 0.50),
        ))
        .expect("wall");
    let wall_z = r3d
        .upload_mesh(&bake(
            &box_tris(vec3(-ARENA - 0.8, 0.0, -0.4), vec3(ARENA + 0.8, 1.4, 0.4)),
            (0.45, 0.40, 0.50),
        ))
        .expect("wall");

    pm.task_add("input", 4.0, 0.0, {
        let cmd = cmd.clone();
        let ctl = ctl.clone();
        move |pm| {
            for ev in pump.poll_iter() {
                match ev {
                    Event::Quit { .. }
                    | Event::KeyDown { scancode: Some(Scancode::Escape), .. } => pm.loop_quit(),
                    Event::KeyDown { scancode: Some(Scancode::P), repeat: false, .. } => {
                        let mut c = ctl.borrow_mut();
                        c.panini = !c.panini;
                    }
                    Event::KeyDown { scancode: Some(Scancode::C), repeat: false, .. } => {
                        let next = {
                            let mut c = ctl.borrow_mut();
                            if c.cams.is_empty() {
                                None
                            } else {
                                c.active = (c.active + 1) % c.cams.len();
                                Some(c.cams[c.active])
                            }
                        };
                        if let Some(cam) = next {
                            camera_use(pm, cam);
                        }
                    }
                    _ => {}
                }
            }
            let k = pump.keyboard_state();
            let held = |sc: Scancode| k.is_scancode_pressed(sc) as i32 as f32;
            cmd.borrow_mut().0 = Drive {
                thrust: held(Scancode::W) - 0.6 * held(Scancode::S),
                turn: held(Scancode::D) - held(Scancode::A),
            };
        }
    });

    // The moment the server tells us which car is ours, rig the ENTITY:
    // one camera_follow feeding its anchor from the smooth-predicted
    // draw state, four camera_attach mounts (chase/hood/backup/side),
    // then this task retires itself. The first camera_* call bootstraps
    // the whole pm::camera module — no install ceremony.
    pm.task_add("rig_cams", 32.0, 0.0, {
        let stats = stats.clone();
        let draw = draw.clone();
        let ctl = ctl.clone();
        move |pm| {
            let Some(id) = stats.borrow().mine else { return };
            let draw = draw.clone();
            camera_follow(pm, id, move |_pm| {
                draw.borrow().get(id).copied().map(|c| CamAnchor {
                    pos: vec3(c.x, 0.0, c.z),
                    fwd: vec3(c.heading.sin(), 0.0, c.heading.cos()),
                })
            });
            let cams = [CamRig::chase(), CamRig::hood(), CamRig::backup(), CamRig::side()]
                .map(|rig| camera_attach(pm, id, rig));
            camera_use(pm, cams[0]);
            ctl.borrow_mut().cams = cams.to_vec();
            pm.task_stop("rig_cams");
        }
    });

    pm.task_add("render", 70.0, 0.0, {
        let stats = stats.clone();
        let ctl = ctl.clone();
        move |pm| {
            // Fetched through pm each frame (not hoisted): the camera
            // module doesn't exist until rig_cams bootstraps it, and a
            // once-per-frame name lookup costs nothing.
            let view = pm.single::<CamView>("cam.view");
            let v = view.borrow();
            let (view_mat, fov) = if v.ready() {
                (v.matrix(), v.fov_deg)
            } else {
                (Mat4::look_at(vec3(0.0, 6.0, -ARENA), Vec3::ZERO, Vec3::UP), 100.0)
            };
            drop(v);
            // The active rig drives the FOV; P flips the house panini
            // look off and back (both are plain renderer fields, read
            // per frame — `set_fov` is just the coupled-curve setter).
            r3d.fov_deg = fov;
            r3d.panini = if ctl.borrow().panini { panini_for_fov(fov) } else { 0.0 };
            if let Some(mut frame) = r3d.frame(&window, view_mat, vec3(0.45, 1.0, 0.35)) {
                let white = (1.0, 1.0, 1.0);
                frame.draw(&ground, Mat4::IDENTITY, white, true);
                frame.draw(&wall_x, Mat4::translate(vec3(-ARENA - 0.5, 0.0, 0.0)), white, true);
                frame.draw(&wall_x, Mat4::translate(vec3(ARENA + 0.5, 0.0, 0.0)), white, true);
                frame.draw(&wall_z, Mat4::translate(vec3(0.0, 0.0, -ARENA - 0.5)), white, true);
                frame.draw(&wall_z, Mat4::translate(vec3(0.0, 0.0, ARENA + 0.5)), white, true);
                for (id, c) in draw_pool.borrow().iter() {
                    let tint = peer_color(id.peer());
                    let model = car_model(c);
                    frame.draw(&body, model, tint, true);
                    frame.draw(&cabin, model, (tint.0 * 0.5, tint.1 * 0.5, tint.2 * 0.5), true);
                }
            }
            if pm.tick() % 30 == 0 {
                let st = status.borrow();
                let speed =
                    pred.borrow().state().map(|c| c.speed * 3.6 / 1.6).unwrap_or(0.0); // ~mph, for flavor
                let title = format!(
                    "pm drive — peer {}  {:.0} mph  rtt {:.0} ms  corrections {}  (wasd, esc)",
                    st.peer, speed.abs(), st.rtt_ms, stats.borrow().corrections,
                );
                let _ = window.set_title(&title);
            }
        }
    });

    // Display refresh paces the loop (WSLg ignores vsync; see solids).
    pm.loop_rate = refresh;
    pm.loop_run();
}
