//! Hogs' 3D player client: chase-cam truck, the horde rendered off the
//! interp draw pool, hit/kill/bite markers straight off the TTL'd synced
//! pool, and a local aim line (client-side only — misses replicate
//! nothing, so your own gun draws its own feedback).
//!
//! WASD drives; SPACE fires; 1/2/3 switch cameras; P toggles panini;
//! R respawns; Esc quits.

use pm::{
    CamAnchor, CamRig, CamView, Mat4, Pm, Vec3, camera_install, camera_manager, camera_track, vec3,
};
use pm_sdl::gpu3d::{Frame3, Renderer3d, bake, box_tris, checker_ground, panini_for_fov};
use pm_sdl::sdl3;
use sdl3::event::Event;
use sdl3::keyboard::Scancode;

use crate::bot_client::client_setup;
use crate::common::*;

const W: u32 = 1280;
const H: u32 = 800;

fn truck_model(t: &Truck) -> Mat4 {
    Mat4::translate(vec3(t.x, 0.0, t.z)) * Mat4::rot_y(t.heading)
}

fn hog_model(h: &Hog) -> Mat4 {
    Mat4::translate(vec3(h.x, 0.0, h.z)) * Mat4::rot_y(h.heading)
}

/// Faked bold: overdraw at small offsets (gpu3d font is regular-only).
fn hud_bold(frame: &mut Frame3, s: &str, x: f32, y: f32, px: f32, col: (u8, u8, u8)) {
    for (dx, dy) in [(0.0, 0.0), (1.4, 0.0), (0.0, 1.4), (1.4, 1.4)] {
        frame.text(s, x + dx, y + dy, px, col);
    }
}

pub fn run() {
    let mut pm = Pm::client(ADDR, 1.0 / FIXED_DT);
    let w = client_setup(&mut pm);
    eprintln!("connecting to {ADDR} ...");
    let net = pm.net();

    camera_install(&mut pm);
    let cam_mgr = camera_manager(&mut pm);
    let cam_view = pm.single::<CamView>("cam.view");

    let (mut window, mut pump, refresh) = pm_sdl::window(
        "pm hogs — wasd drives, space fires, 1-3 cams, r respawn, esc",
        W,
        H,
    );
    let mut r3d = Renderer3d::new(&window).expect("renderer");
    r3d.fog_distance = 200.0;
    let ground = r3d
        .upload_mesh(&checker_ground(
            14,
            8.0,
            (0.20, 0.24, 0.18), // scrubland, not asphalt
            (0.15, 0.18, 0.13),
        ))
        .expect("ground");
    // Truck: body + cabin, authored facing +z, baked white so the peer
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
    // Hog: low mean slab + a snout, baked white and tinted by hp.
    let hog_body = r3d
        .upload_mesh(&bake(
            &box_tris(vec3(-0.55, 0.1, -0.7), vec3(0.55, 0.8, 0.7)),
            (1.0, 1.0, 1.0),
        ))
        .expect("hog body");
    let hog_snout = r3d
        .upload_mesh(&bake(
            &box_tris(vec3(-0.28, 0.2, 0.7), vec3(0.28, 0.6, 1.05)),
            (1.0, 1.0, 1.0),
        ))
        .expect("hog snout");
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
    // Small ground marker: aim-line breadcrumbs and impact flashes.
    let marker = r3d
        .upload_mesh(&bake(
            &box_tris(vec3(-0.16, 0.02, -0.16), vec3(0.16, 0.34, 0.16)),
            (1.0, 1.0, 1.0),
        ))
        .expect("marker");

    pm.task_add("input", 4.0, 0.0, {
        let cam_mgr = cam_mgr.clone();
        let respawn = w.respawn.clone();
        let input = w.input.clone();
        move |pm| {
            for ev in pump.poll_iter() {
                match ev {
                    Event::Quit { .. }
                    | Event::KeyDown {
                        scancode: Some(Scancode::Escape),
                        ..
                    } => pm.loop_quit(),
                    Event::KeyDown {
                        scancode: Some(Scancode::R),
                        repeat: false,
                        ..
                    } => respawn.send(Respawn::default()),
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
            input.set(Drive {
                thrust: held(Scancode::W) - 0.6 * held(Scancode::S),
                turn: held(Scancode::D) - held(Scancode::A),
                fire: held(Scancode::Space),
                bot: 0.0, // human: crisp steering
            });
        }
    });

    // Rig cameras on our truck the moment the server tells us which one
    // is ours, then retire this task (drive's pattern).
    pm.task_add("rig_cams", 32.0, 0.0, {
        let draw = w.truck_draw.clone();
        let net = net.clone();
        move |pm| {
            let Some(id) = net.mine() else {
                return;
            };
            let draw = draw.clone();
            {
                let mut rack = camera_track(pm, id, move |_pm| {
                    draw.get().get(id).copied().map(|t| CamAnchor {
                        pos: vec3(t.x, 0.0, t.z),
                        fwd: vec3(t.heading.sin(), 0.0, t.heading.cos()),
                    })
                });
                let chase = rack.mount(CamRig::chase());
                rack.mount(CamRig::rear());
                rack.mount(CamRig::side());
                rack.show(chase);
            }
            pm.task_stop("rig_cams");
        }
    });

    pm.task_add("render", 70.0, 0.0, {
        let view = cam_view.clone();
        let net = net.clone();
        move |pm| {
            let (view_mat, fov, panini_on) = {
                let v = view.get();
                if v.ready() {
                    (v.matrix(), v.fov_deg, v.panini)
                } else {
                    (
                        Mat4::look_at(vec3(0.0, 8.0, -ARENA), Vec3::ZERO, Vec3::UP),
                        100.0,
                        v.panini,
                    )
                }
            };
            r3d.fov_deg = fov;
            r3d.panini = if panini_on { panini_for_fov(fov) } else { 0.0 };
            if let Some(mut frame) = r3d.frame(&window, view_mat, vec3(0.45, 1.0, 0.35)) {
                let white = (1.0, 1.0, 1.0);
                frame.draw(&ground, Mat4::IDENTITY, white, true);
                for (m, off) in [
                    (&wall_x, vec3(-ARENA - 0.5, 0.0, 0.0)),
                    (&wall_x, vec3(ARENA + 0.5, 0.0, 0.0)),
                    (&wall_z, vec3(0.0, 0.0, -ARENA - 0.5)),
                    (&wall_z, vec3(0.0, 0.0, ARENA + 0.5)),
                ] {
                    frame.draw(m, Mat4::translate(off), white, true);
                }

                // Trucks: smoothed view (predicted self, interp'd others).
                let mine = net.mine();
                for (id, t) in w.truck_draw.get().iter() {
                    let tint = peer_color(id.peer());
                    let model = truck_model(t);
                    frame.draw(&body, model, tint, true);
                    frame.draw(
                        &cabin,
                        model,
                        (tint.0 * 0.5, tint.1 * 0.5, tint.2 * 0.5),
                        true,
                    );
                    // Own truck: local aim line out to gun range —
                    // client-side feedback, nothing on the wire.
                    if mine == Some(id) {
                        let (dx, dz) = (t.heading.sin(), t.heading.cos());
                        let mut d = 4.0;
                        while d < GUN_RANGE {
                            let fade = 1.0 - d / GUN_RANGE;
                            frame.draw(
                                &marker,
                                Mat4::translate(vec3(t.x + dx * d, 0.0, t.z + dz * d))
                                    * Mat4::scale(0.7),
                                (tint.0 * fade, tint.1 * fade, tint.2 * fade),
                                true,
                            );
                            d += 3.5;
                        }
                    }
                }

                // The horde, off its interp pool; biomod green fades to
                // livid red as hp drops.
                for (_, h) in w.hog_draw.get().iter() {
                    let hp = (h.hp / HOG_HP).clamp(0.0, 1.0);
                    let tint = (
                        0.45 + 0.4 * (1.0 - hp),
                        0.42 * hp + 0.10,
                        0.28 * hp + 0.06,
                    );
                    let model = hog_model(h);
                    frame.draw(&hog_body, model, tint, true);
                    frame.draw(
                        &hog_snout,
                        model,
                        (tint.0 * 0.7, tint.1 * 0.7, tint.2 * 0.7),
                        true,
                    );
                }

                // Impact facts off the TTL'd pool: existence == recency.
                for (_, c) in w.impact.get().iter() {
                    let (scale, col) = if c.kind == IMPACT_KILL {
                        (3.0, (1.0, 0.9, 0.30))
                    } else if c.kind == IMPACT_BITE {
                        (2.2, (1.0, 0.20, 0.15))
                    } else {
                        (1.4, (1.0, 0.55, 0.10))
                    };
                    frame.draw(
                        &marker,
                        Mat4::translate(vec3(c.x, 0.0, c.z)) * Mat4::scale(scale),
                        col,
                        true,
                    );
                }

                // HUD: team score big, wave + horde count under it — all
                // read RAW off the synced single (server-owned, never
                // predicted).
                let sb = w.hunt.get();
                let spx = 56.0;
                let score_txt = format!("{}", sb.points as i32);
                let sw = frame.text_width(&score_txt, spx);
                hud_bold(
                    &mut frame,
                    &score_txt,
                    (W as f32 - sw) / 2.0,
                    12.0,
                    spx,
                    (245, 245, 245),
                );
                let info = format!("wave {}   hogs {}", sb.wave, sb.alive);
                let iw = frame.text_width(&info, 22.0);
                hud_bold(
                    &mut frame,
                    &info,
                    (W as f32 - iw) / 2.0,
                    12.0 + spx + 8.0,
                    22.0,
                    (200, 210, 190),
                );
            }
            if pm.tick() % 30 == 0 {
                let speed = w
                    .pred
                    .get()
                    .state()
                    .map(|t| t.speed * 3.6 / 1.6)
                    .unwrap_or(0.0);
                let title = format!(
                    "pm hogs — peer {}  {:.0} mph  rtt {:.0} ms  corrections {}  (wasd+space)",
                    net.peer(),
                    speed.abs(),
                    net.rtt_ms(),
                    w.pred.get().corrections,
                );
                let _ = window.set_title(&title);
            }
        }
    });

    // Display refresh paces the loop (WSLg ignores vsync; see solids).
    pm.loop_rate = refresh;
    pm.run().expect("connect");
}
