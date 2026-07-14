//! solids — 3D shapes with a fly camera, on `pm_sdl::gpu3d` (SDL3 GPU:
//! real vertex buffers, depth testing, WGSL shaders — see that module
//! for the conventions and the y-flip/winding story).
//!
//!   cargo run --release -p solids
//!
//! Controls: WASD move (camera-relative), E/Q up/down, hold RIGHT mouse
//! and drag (or arrow keys) to look, wheel = move speed, C toggles
//! back-face culling (fly under the ground to see what it does), Esc
//! quits.

use pm::{Mat4, Pm, Vec3, task, vec3};
use pm_sdl::gpu3d::{Renderer3d, bake, box_tris, checker_ground};
use pm_sdl::sdl3;
use sdl3::event::Event;
use sdl3::keyboard::Scancode;
use sdl3::mouse::MouseButton;

const W: u32 = 1100;
const H: u32 = 800;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mesh {
    Cube,
    Tetra,
    Octa,
}

/// A spinning solid; an ordinary pool entity.
struct Solid {
    pos: Vec3,
    scale: f32,
    axis: Vec3,
    angle: f32,
    spin: f32, // rad/s
    mesh: Mesh,
}

#[derive(Clone, Copy)]
struct Cam {
    pos: Vec3,
    yaw: f32,   // 0 looks down +z
    pitch: f32, // + looks up
    speed: f32,
}

impl Default for Cam {
    fn default() -> Self {
        Self {
            pos: vec3(0.0, 2.0, -10.0),
            yaw: 0.0,
            pitch: -0.1,
            speed: 6.0,
        }
    }
}

impl Cam {
    fn forward(self) -> Vec3 {
        let (sy, cy) = self.yaw.sin_cos();
        let (sp, cp) = self.pitch.sin_cos();
        vec3(sy * cp, sp, cy * cp)
    }

    fn right(self) -> Vec3 {
        let (sy, cy) = self.yaw.sin_cos();
        vec3(cy, 0.0, -sy)
    }

    fn view(self) -> Mat4 {
        Mat4::rot_x(self.pitch) * Mat4::rot_y(-self.yaw) * Mat4::translate(-self.pos)
    }
}

/// Live culling toggle (C key) — flip it off and fly under the ground
/// to see what back-face culling does to open surfaces.
struct Cull(bool);

impl Default for Cull {
    fn default() -> Self {
        Cull(true)
    }
}

fn tetra_tris() -> Vec<[Vec3; 3]> {
    let a = vec3(0.0, 0.6, 0.0);
    let b = vec3(-0.5, -0.4, -0.3);
    let c = vec3(0.5, -0.4, -0.3);
    let d = vec3(0.0, -0.4, 0.6);
    vec![[a, c, b], [a, d, c], [a, b, d], [b, c, d]]
}

fn octa_tris() -> Vec<[Vec3; 3]> {
    let (xp, xn) = (vec3(0.6, 0.0, 0.0), vec3(-0.6, 0.0, 0.0));
    let (yp, yn) = (vec3(0.0, 0.6, 0.0), vec3(0.0, -0.6, 0.0));
    let (zp, zn) = (vec3(0.0, 0.0, 0.6), vec3(0.0, 0.0, -0.6));
    vec![
        [yp, zp, xp],
        [yp, xp, zn],
        [yp, zn, xn],
        [yp, xn, zp],
        [yn, xp, zp],
        [yn, zn, xp],
        [yn, xn, zn],
        [yn, zp, xn],
    ]
}

fn spawn_scene(pm: &mut Pm) {
    let solids = pm.pool::<Solid>("solids");
    let mut add = |s: Solid| {
        let id = pm.id_add();
        solids.get_mut().add(id, s);
    };
    for i in 0..8 {
        let a = i as f32 / 8.0 * std::f32::consts::TAU;
        add(Solid {
            pos: vec3(a.cos() * 7.0, 1.0, a.sin() * 7.0),
            scale: 1.6,
            axis: vec3(0.3, 1.0, 0.2).norm(),
            angle: a,
            spin: 0.6,
            mesh: Mesh::Cube,
        });
    }
    add(Solid {
        pos: vec3(0.0, 2.6, 0.0),
        scale: 4.0,
        axis: Vec3::UP,
        angle: 0.0,
        spin: 0.25,
        mesh: Mesh::Tetra,
    });
    for i in 0..5 {
        let a = i as f32 / 5.0 * std::f32::consts::TAU + 0.5;
        add(Solid {
            pos: vec3(a.cos() * 3.5, 3.0 + i as f32 * 0.8, a.sin() * 3.5),
            scale: 1.0,
            axis: vec3(1.0, 0.4, 0.0).norm(),
            angle: a * 2.0,
            spin: 1.4,
            mesh: Mesh::Octa,
        });
    }
}

fn main() {
    let mut pm = Pm::new();
    spawn_scene(&mut pm);
    let solids = pm.pool::<Solid>("solids");
    let cam = pm.single::<Cam>("cam");
    let cull = pm.single::<Cull>("cull");

    let (mut window, mut pump, refresh) = pm_sdl::window(
        "pm solids (gpu) — wasd+eq fly, hold RMB to look, wheel = speed",
        W,
        H,
    );
    let mut r3d = Renderer3d::new(&window).expect("renderer");
    let ground = r3d
        .upload_mesh(&checker_ground(
            14,
            1.0,
            (0.24, 0.27, 0.33),
            (0.16, 0.18, 0.22),
        ))
        .expect("ground");
    let cube = r3d
        .upload_mesh(&bake(
            &box_tris(vec3(-0.5, -0.5, -0.5), vec3(0.5, 0.5, 0.5)),
            (0.36, 0.55, 0.86),
        ))
        .expect("cube");
    let tetra = r3d
        .upload_mesh(&bake(&tetra_tris(), (0.95, 0.78, 0.25)))
        .expect("tetra");
    let octa = r3d
        .upload_mesh(&bake(&octa_tris(), (0.85, 0.35, 0.42)))
        .expect("octa");

    task!(pm, "input", 4.0, [cam, cull], move |pm| {
        let dt = pm.loop_dt();
        let mut c = cam.get_mut();
        for ev in pump.poll_iter() {
            match ev {
                Event::Quit { .. }
                | Event::KeyDown {
                    scancode: Some(Scancode::Escape),
                    ..
                } => pm.loop_quit(),
                Event::KeyDown {
                    scancode: Some(Scancode::C),
                    repeat: false,
                    ..
                } => {
                    let now = !cull.get().0;
                    cull.get_mut().0 = now;
                }
                Event::MouseMotion {
                    xrel,
                    yrel,
                    mousestate,
                    ..
                } if mousestate.is_mouse_button_pressed(MouseButton::Right) => {
                    c.yaw += xrel * 0.004;
                    c.pitch = (c.pitch - yrel * 0.004).clamp(-1.5, 1.5);
                }
                Event::MouseWheel { y, .. } => {
                    c.speed = (c.speed * 1.15_f32.powf(y)).clamp(1.0, 60.0);
                }
                _ => {}
            }
        }
        let k = pump.keyboard_state();
        let held = |sc: Scancode| k.is_scancode_pressed(sc) as i32 as f32;
        c.yaw += (held(Scancode::Right) - held(Scancode::Left)) * 1.8 * dt;
        c.pitch =
            (c.pitch + (held(Scancode::Up) - held(Scancode::Down)) * 1.2 * dt).clamp(-1.5, 1.5);
        let fwd = c.forward();
        let right = c.right();
        let mv = fwd * (held(Scancode::W) - held(Scancode::S))
            + right * (held(Scancode::D) - held(Scancode::A))
            + Vec3::UP * (held(Scancode::E) - held(Scancode::Q));
        let speed = c.speed;
        c.pos += mv.norm() * (speed * dt);
    });

    task!(pm, "spin", 30.0, [solids], move |pm| {
        let dt = pm.loop_dt();
        for (_, mut s) in solids.get_mut().iter_mut() {
            let spin = s.spin;
            s.angle += spin * dt;
        }
    });

    task!(pm, "render", 70.0, [solids, cam, cull], move |pm| {
        let c = *cam.get();
        let culling = cull.get().0;
        let white = (1.0, 1.0, 1.0);
        if let Some(mut frame) = r3d.frame(&window, c.view(), vec3(0.45, 1.0, 0.35)) {
            frame.draw(&ground, Mat4::IDENTITY, white, culling);
            for (_, s) in solids.get().iter() {
                let model =
                    Mat4::translate(s.pos) * Mat4::rot_axis(s.axis, s.angle) * Mat4::scale(s.scale);
                let mesh = match s.mesh {
                    Mesh::Cube => &cube,
                    Mesh::Tetra => &tetra,
                    Mesh::Octa => &octa,
                };
                frame.draw(mesh, model, white, culling);
            }
        }

        if pm.tick() % 30 == 0 {
            let title = format!(
                "pm solids (gpu) — {} solids, {:.0} fps, cull {} (C toggles; wasd+eq fly, RMB look)",
                solids.get().len(),
                1.0 / pm.loop_dt().max(1e-6),
                if culling { "ON" } else { "off" },
            );
            let _ = window.set_title(&title);
        }
    });

    pm.loop_rate = refresh;
    pm.loop_run();
}
