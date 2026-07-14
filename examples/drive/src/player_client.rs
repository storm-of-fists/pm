//! Drive's 3D player client: pm::camera rigs attached to your car
//! entity (cameras are entities related to the car by `CamRig.target`),
//! the whole field rendered through pm_sdl::gpu3d's panini projection.
//!
//! WASD drives; 1/2/3 switch cameras (chase / rear / side, each with its
//! own FOV); P toggles panini; R respawns (a reliable client→server event);
//! Esc quits. Camera switching and the panini toggle go through the
//! `CamManager` single — no `pm` in the per-frame path.

use pm::{
    CamAnchor, CamRig, CamView, Mat4, Pm, Vec3, camera_install, camera_manager, camera_track, task,
    vec3,
};
use pm_sdl::gpu3d::{Frame3, Renderer3d, bake, box_tris, checker_ground, panini_for_fov};
use pm_sdl::sdl3;
use sdl3::event::Event;
use sdl3::keyboard::Scancode;

use crate::bot_client::add_client_tasks;
use crate::common::*;

const W: u32 = 1280;
const H: u32 = 800;

fn car_model(c: &Car) -> Mat4 {
    Mat4::translate(vec3(c.x, 0.0, c.z)) * Mat4::rot_y(c.heading)
}

/// Faked bold: the same string overdrawn at small offsets thickens the
/// coverage (the gpu3d font face is regular-weight only).
fn hud_bold(frame: &mut Frame3, s: &str, x: f32, y: f32, px: f32, col: (u8, u8, u8)) {
    for (dx, dy) in [(0.0, 0.0), (1.4, 0.0), (0.0, 1.4), (1.4, 1.4)] {
        frame.text(s, x + dx, y + dy, px, col);
    }
}

#[allow(dead_code)] // pause-menu stub, not wired up yet
pub struct InGameMenu {
    pub paused: bool,
}

pub fn run() {
    let mut pm = Pm::client(ADDR, 1.0 / FIXED_DT);
    // Same synced pools as the server (order doesn't matter — keyed by name).
    let car = pm.sync_pool::<Car>("car");
    let score = pm.sync_pool::<Score>("score");
    // Transient collision markers, server-spawned and server-expired
    // (ttl_pool): render whatever entries exist, clean up nothing.
    let contact = pm.sync_pool::<Contact>("contact");
    eprintln!("connecting to {ADDR} ...");
    // THE continuous input channel: WASD lands in this pod, sampled and
    // sent at the fixed input cadence.
    let input = pm.input::<Drive>("drive");
    // `draw` is the smoothed view to render: predicted local car,
    // interpolated remotes, maintained by the net module.
    let (pred, draw) = add_client_tasks(&mut pm, &car, &input);
    let net = pm.net();

    // Reliable client→server intent: R flips us back to spawn. Continuous
    // driving rides the unreliable input channel; this one-shot rides the
    // event.
    let respawn = pm.event::<Respawn>("respawn");
    // Install the camera module now so we can capture its manager single
    // for the input task; the rig is mounted later, once we know our car.
    camera_install(&mut pm);
    let cam_mgr = camera_manager(&mut pm);
    let cam_view = pm.single::<CamView>("cam.view");
    let draw_pool = draw.clone();

    let (mut window, mut pump, refresh) = pm_sdl::window(
        "pm drive — wasd, 1-3 cams, p panini, r respawn, esc quits",
        W,
        H,
    );
    let mut r3d = Renderer3d::new(&window).expect("renderer");
    r3d.fog_distance = 160.0;
    let ground = r3d
        .upload_mesh(&checker_ground(
            12,
            8.0,
            (0.22, 0.25, 0.30),
            (0.15, 0.17, 0.21),
        ))
        .expect("ground");
    // Car: body + cabin, authored facing +z, baked white so the peer
    // color arrives as a per-draw tint.
    let body = r3d
        .upload_mesh(&bake(
            &box_tris(vec3(-0.9, 0.15, -1.7), vec3(0.9, 0.95, 1.7)),
            (1.0, 1.0, 1.0),
        ))
        .expect("body");
    let cabin = r3d
        .upload_mesh(&bake(
            &box_tris(vec3(-0.7, 0.95, -1.1), vec3(0.7, 1.55, 0.45)),
            (1.0, 1.0, 1.0),
        ))
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
    // Small ground marker for the projected-path breadcrumbs ahead of
    // each rival car.
    let marker = r3d
        .upload_mesh(&bake(
            &box_tris(vec3(-0.16, 0.02, -0.16), vec3(0.16, 0.34, 0.16)),
            (1.0, 1.0, 1.0),
        ))
        .expect("marker");

    task!(pm, "input", 4.0, [cam_mgr, respawn, input], move |pm| {
        for ev in pump.poll_iter() {
            match ev {
                Event::Quit { .. }
                | Event::KeyDown {
                    scancode: Some(Scancode::Escape),
                    ..
                } => pm.loop_quit(),
                // R: reliable respawn intent to the server.
                Event::KeyDown {
                    scancode: Some(Scancode::R),
                    repeat: false,
                    ..
                } => respawn.send(Respawn::default()),
                // Camera controls go straight through the manager
                // single — no pm.
                Event::KeyDown {
                    scancode: Some(Scancode::P),
                    repeat: false,
                    ..
                } => {
                    cam_mgr.get().toggle_panini();
                }
                Event::KeyDown {
                    scancode: Some(sc @ (Scancode::_1 | Scancode::_2 | Scancode::_3)),
                    repeat: false,
                    ..
                } => {
                    cam_mgr
                        .get()
                        .show_index(sc as usize - Scancode::_1 as usize);
                }
                _ => {}
            }
        }
        let k = pump.keyboard_state();
        let held = |sc: Scancode| k.is_scancode_pressed(sc) as i32 as f32;
        let drift = (held(Scancode::LShift) + held(Scancode::RShift)).min(1.0);
        input.set(Drive {
            thrust: held(Scancode::W) - 0.6 * held(Scancode::S),
            turn: held(Scancode::D) - held(Scancode::A),
            drift,
            bot: 0.0, // human: crisp steering
        });
    });

    // The moment the server tells us which car is ours, rig the ENTITY:
    // camera_track registers the anchor sampler (fed from the smooth-
    // predicted draw state) and hands back a rack; we mount three cameras
    // on it and show the chase. Then this task retires itself.
    task!(pm, "rig_cams", 32.0, [draw, net], move |pm| {
        let Some(id) = net.mine() else {
            return;
        };
        let draw = draw.clone();
        {
            let mut rack = camera_track(pm, id, move |_pm| {
                draw.get_id(id).map(|c| CamAnchor {
                    pos: vec3(c.x, 0.0, c.z),
                    fwd: vec3(c.heading.sin(), 0.0, c.heading.cos()),
                })
            });
            let chase = rack.mount(CamRig::chase());
            rack.mount(CamRig::rear());
            rack.mount(CamRig::side());
            rack.show(chase);
        }
        pm.task_stop("rig_cams");
    });

    let view = cam_view.clone();
    task!(pm, "render", 70.0, [score, net], move |pm| {
        let (view_mat, fov, panini_on) = {
            let v = view.get();
            if v.ready() {
                (v.matrix(), v.fov_deg, v.panini)
            } else {
                (
                    Mat4::look_at(vec3(0.0, 6.0, -ARENA), Vec3::ZERO, Vec3::UP),
                    100.0,
                    v.panini,
                )
            }
        };
        // The active rig drives the FOV; the panini flag rides the
        // house fov→distance curve, or 0 (rectilinear) when toggled
        // off. Both are plain renderer fields, read per frame.
        r3d.fov_deg = fov;
        r3d.panini = if panini_on { panini_for_fov(fov) } else { 0.0 };
        if let Some(mut frame) = r3d.frame(&window, view_mat, vec3(0.45, 1.0, 0.35)) {
            let white = (1.0, 1.0, 1.0);
            frame.draw(&ground, Mat4::IDENTITY, white, true);
            frame.draw(
                &wall_x,
                Mat4::translate(vec3(-ARENA - 0.5, 0.0, 0.0)),
                white,
                true,
            );
            frame.draw(
                &wall_x,
                Mat4::translate(vec3(ARENA + 0.5, 0.0, 0.0)),
                white,
                true,
            );
            frame.draw(
                &wall_z,
                Mat4::translate(vec3(0.0, 0.0, -ARENA - 0.5)),
                white,
                true,
            );
            frame.draw(
                &wall_z,
                Mat4::translate(vec3(0.0, 0.0, ARENA + 0.5)),
                white,
                true,
            );
            for (id, c) in draw_pool.get().iter() {
                let tint = peer_color(id.peer());
                let model = car_model(c);
                frame.draw(&body, model, tint, true);
                frame.draw(
                    &cabin,
                    model,
                    (tint.0 * 0.5, tint.1 * 0.5, tint.2 * 0.5),
                    true,
                );
                // Projected path: replay the shared step forward holding
                // the car's current (replicated) steering. For a bot
                // that steer lags, so this is a genuine lead; for a
                // human it tracks their live turn. `bot: 0` keeps steer
                // fixed across the integration, so it's a clean arc.
                if c.speed.abs() > 1.0 {
                    let hold = Drive {
                        thrust: (1.2 * c.speed / 14.0).clamp(-1.0, 1.0), // hold speed
                        turn: c.steer,
                        drift: 0.0,
                        bot: 0.0,
                    };
                    for (k, (px, pz)) in predict_path(*c, hold, 14, 0.045).into_iter().enumerate() {
                        let fade = 1.0 - k as f32 / 20.0;
                        frame.draw(
                            &marker,
                            Mat4::translate(vec3(px, 0.0, pz)),
                            (tint.0 * fade, tint.1 * fade, tint.2 * fade),
                            true,
                        );
                    }
                }
            }

            // Contact flashes: hot little pylons where a hit was billed.
            // Transient facts straight off the synced pool — the server's
            // TTL removes them, so existence == recency.
            for (_, c) in contact.get().iter() {
                frame.draw(
                    &marker,
                    Mat4::translate(vec3(c.x, 0.0, c.z)) * Mat4::scale(2.2),
                    (1.0, 0.45, 0.10),
                    true,
                );
            }

            // HUD: one bold line, top-middle — total score (white) with
            // the live rate to its right (green gaining, red after a hit).
            // Read RAW from the authoritative `score` pool — server-owned,
            // never predicted — so the number and its rate can't disagree
            // with each other or lag the truth.
            let mine = net.mine();
            let (points, rate) = match mine.and_then(|id| score.get_id(id).map(|s| *s)) {
                Some(s) => (s.points, s.rate),
                None => (0.0, 0.0),
            };

            let spx = 64.0;
            let gap = 30.0;
            let score_txt = format!("{}", points as i32);
            let rate_txt = format!("{rate:+.0}");
            let sw = frame.text_width(&score_txt, spx);
            let rw = frame.text_width(&rate_txt, spx);
            let x0 = (W as f32 - (sw + gap + rw)) / 2.0;
            let rcol = if rate >= 0.0 {
                (90, 225, 110)
            } else {
                (240, 80, 80)
            };
            hud_bold(&mut frame, &score_txt, x0, 14.0, spx, (245, 245, 245));
            hud_bold(&mut frame, &rate_txt, x0 + sw + gap, 14.0, spx, rcol);
        }
        if pm.tick() % 30 == 0 {
            let speed = pred
                .get()
                .state()
                .map(|c| c.speed * 3.6 / 1.6)
                .unwrap_or(0.0); // ~mph, for flavor
            let title = format!(
                "pm drive — peer {}  {:.0} mph  rtt {:.0} ms  corrections {}  (wasd, esc)",
                net.peer(),
                speed.abs(),
                net.rtt_ms(),
                pred.get().corrections,
            );
            let _ = window.set_title(&title);
        }
    });

    // Display refresh paces the loop (WSLg ignores vsync; see solids).
    pm.loop_rate = refresh;
    pm.run().expect("connect");
}
