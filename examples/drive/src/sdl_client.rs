//! Drive's 3D player client: chase camera behind your predicted car,
//! the whole field rendered through pm_sdl::gpu3d. WASD drives, Esc
//! quits.

use pm::{Mat4, Pm, Predictor, Vec3, vec3};
use pm_sdl::gpu3d::{Renderer3d, bake, box_tris, checker_ground};
use pm_sdl::sdl3;
use sdl3::event::Event;
use sdl3::keyboard::Scancode;

use pm::{NetInput, NetStatus};

use crate::client::{Stats, add_client_tasks, connect};
use crate::common::*;

const W: u32 = 1280;
const H: u32 = 800;

/// Smoothed chase camera: follows a point behind/above the car, always
/// looking a little ahead of it.
struct Chase {
    pos: Vec3,
    target: Vec3,
}

impl Default for Chase {
    fn default() -> Self {
        Self { pos: vec3(0.0, 6.0, -ARENA), target: Vec3::ZERO }
    }
}

fn car_model(c: &Car) -> Mat4 {
    Mat4::translate(vec3(c.x, 0.0, c.z)) * Mat4::rot_y(c.heading)
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
    let chase = pm.single::<Chase>("chase");
    let draw_pool = draw.clone();

    let (mut window, mut pump, refresh) = pm_sdl::window("pm drive — wasd, esc quits", W, H);
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
        move |pm| {
            for ev in pump.poll_iter() {
                match ev {
                    Event::Quit { .. }
                    | Event::KeyDown { scancode: Some(Scancode::Escape), .. } => pm.loop_quit(),
                    _ => {}
                }
            }
            let k = pump.keyboard_state();
            let held = |sc: Scancode| k.is_scancode_pressed(sc) as i32 as f32;
            cmd.borrow_mut().0 = Drive {
                thrust: held(Scancode::W) - 0.6 * held(Scancode::S),
                turn: held(Scancode::A) - held(Scancode::D),
            };
        }
    });

    // Chase camera: spring toward a point behind the predicted car.
    pm.task_add("camera", 35.0, 0.0, {
        let chase = chase.clone();
        let pred = pred.clone();
        move |pm| {
            let Some(c) = pred.borrow().state() else { return };
            let dt = pm.loop_dt();
            let fwd = vec3(c.heading.sin(), 0.0, c.heading.cos());
            let want_pos = vec3(c.x, 0.0, c.z) - fwd * 9.0 + Vec3::UP * 3.6;
            let want_tgt = vec3(c.x, 1.0, c.z) + fwd * 4.0;
            let mut ch = chase.borrow_mut();
            let k = (6.0 * dt).min(1.0);
            // Locals first: += through the RefMut would need deref_mut
            // and deref of `ch` at once.
            let (p, t) = (ch.pos, ch.target);
            ch.pos = p + (want_pos - p) * k;
            ch.target = t + (want_tgt - t) * k;
        }
    });

    pm.task_add("render", 70.0, 0.0, {
        let chase = chase.clone();
        let stats = stats.clone();
        move |pm| {
            let (pos, target) = {
                let ch = chase.borrow();
                (ch.pos, ch.target)
            };
            let view = Mat4::look_at(pos, target, Vec3::UP);
            if let Some(mut frame) = r3d.frame(&window, view, vec3(0.45, 1.0, 0.35)) {
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
