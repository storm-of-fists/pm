//! Hogs' 3D player client: chase-cam truck with a mouse-aimed turret,
//! the horde and the bullet tracers rendered off their interp draw
//! pools, hit/kill/bite markers straight off the TTL'd synced pool, and
//! a local aim line under the turret (client-side only feedback).
//!
//! WASD drives; hold RIGHT mouse and move to swing the turret AND the
//! camera (both ease back to dead ahead on release); LEFT mouse (or
//! SPACE) fires; SHIFT boosts — watch the heat bar, 1.0 is an
//! explosion; 1/2/3 switch cameras; P toggles panini; R respawns; Esc
//! quits.

use pm::{
    CamAnchor, CamRig, CamView, Mat4, Pm, Vec3, camera_install, camera_manager, camera_track, vec3,
};
use pm_sdl::gpu3d::{Frame3, Renderer3d, bake, box_tris, checker_ground, panini_for_fov};
use pm_sdl::sdl3;
use sdl3::event::Event;
use sdl3::keyboard::Scancode;
use sdl3::mouse::MouseButton;

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
    // Sounds ride the same replicated facts the renderer draws — see sfx.rs.
    crate::sfx::install(&mut pm, &w);

    camera_install(&mut pm);
    let cam_mgr = camera_manager(&mut pm);
    let cam_view = pm.single::<CamView>("cam.view");

    let (mut window, mut pump, refresh) = pm_sdl::window(
        "pm hogs — wasd drives, rmb aims, lmb fires, 1-3 cams, r respawn, esc",
        W,
        H,
    );
    let mut r3d = Renderer3d::new(&window).expect("renderer");
    r3d.fog_distance = 320.0;
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
    // Turret barrel, authored facing +z on the cab roof — drawn with its
    // own rot_y(heading + aim), so everyone sees turrets swing.
    let barrel = r3d
        .upload_mesh(&bake(
            &box_tris(vec3(-0.12, 1.45, -0.35), vec3(0.12, 1.72, 1.9)),
            (1.0, 1.0, 1.0),
        ))
        .expect("barrel");
    // Bullet tracer: a stretched sliver at gun height.
    let tracer = r3d
        .upload_mesh(&bake(
            &box_tris(vec3(-0.07, 1.35, -0.65), vec3(0.07, 1.55, 0.65)),
            (1.0, 1.0, 1.0),
        ))
        .expect("tracer");
    // ONE white unit cube (base centered on the origin, 1 unit tall)
    // stretched per draw by Mat4::scale_xyz — buildings and walls are
    // all just this box (safe here: axis-aligned scaling keeps an
    // axis-aligned box's normals exact).
    let cube = r3d
        .upload_mesh(&bake(
            &box_tris(vec3(-0.5, 0.0, -0.5), vec3(0.5, 1.0, 0.5)),
            (1.0, 1.0, 1.0),
        ))
        .expect("cube");
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
    // Small ground marker: aim-line breadcrumbs and impact flashes.
    let marker = r3d
        .upload_mesh(&bake(
            &box_tris(vec3(-0.16, 0.02, -0.16), vec3(0.16, 0.34, 0.16)),
            (1.0, 1.0, 1.0),
        ))
        .expect("marker");

    // Turret angle is CLIENT state: holding RMB accumulates mouse-x into
    // it, releasing eases it back to dead ahead. The server only ever
    // sees the absolute angle in each command frame — the animation
    // replays exactly, so prediction never corrects over it.
    let mut aim = 0.0f32;

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
                    Event::MouseMotion {
                        xrel, mousestate, ..
                    } if mousestate.is_mouse_button_pressed(MouseButton::Right) => {
                        aim = (aim + xrel * 0.005).clamp(-AIM_MAX, AIM_MAX);
                    }
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
            let m = pump.mouse_state();
            if !m.is_mouse_button_pressed(MouseButton::Right) {
                // Released: exponential ease back to the front — the
                // smooth snap. Kill the tail so it truly zeroes.
                aim *= (-9.0 * pm.loop_dt()).exp();
                if aim.abs() < 0.005 {
                    aim = 0.0;
                }
            }
            let lmb = m.is_mouse_button_pressed(MouseButton::Left) as i32 as f32;
            input.set(Drive {
                thrust: held(Scancode::W) - 0.6 * held(Scancode::S),
                turn: held(Scancode::D) - held(Scancode::A),
                fire: lmb.max(held(Scancode::Space)),
                aim,
                boost: held(Scancode::LShift),
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
                    draw.get_id(id).map(|t| {
                        // The anchor faces where the TURRET points, not
                        // where the truck drives: holding RMB swings the
                        // camera with the gun (and the ease-back swings
                        // it home), while plain steering under a held
                        // aim no longer drags the view around.
                        let dir = t.heading + t.aim;
                        CamAnchor {
                            pos: vec3(t.x, 0.0, t.z),
                            fwd: vec3(dir.sin(), 0.0, dir.cos()),
                        }
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
                // Walls and buildings: the unit cube, stretched. Walls
                // overlap at the corners rather than mitering — cheap.
                let span = 2.0 * ARENA + 1.6;
                for (x, z, sx, sz) in [
                    (-ARENA - 0.5, 0.0, 0.8, span),
                    (ARENA + 0.5, 0.0, 0.8, span),
                    (0.0, -ARENA - 0.5, span, 0.8),
                    (0.0, ARENA + 0.5, span, 0.8),
                ] {
                    frame.draw(
                        &cube,
                        Mat4::translate(vec3(x, 0.0, z)) * Mat4::scale_xyz(sx, 1.4, sz),
                        (0.45, 0.40, 0.50),
                        true,
                    );
                }
                for (i, &(x, z, hw, hd, h)) in BUILDINGS.iter().enumerate() {
                    let g = 0.36 + 0.06 * ((i % 3) as f32);
                    frame.draw(
                        &cube,
                        Mat4::translate(vec3(x, 0.0, z)) * Mat4::scale_xyz(2.0 * hw, h, 2.0 * hd),
                        (g, g * 0.96, g * 1.06),
                        true,
                    );
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
                    // Turret: same origin, rotated by heading + aim —
                    // replicated state, so every peer's swing is visible.
                    let dir = t.heading + t.aim;
                    frame.draw(
                        &barrel,
                        Mat4::translate(vec3(t.x, 0.0, t.z)) * Mat4::rot_y(dir),
                        (tint.0 * 0.35, tint.1 * 0.35, tint.2 * 0.35),
                        true,
                    );
                    // Own truck: local aim line under the turret, cut
                    // short by buildings — client-side feedback only,
                    // nothing on the wire.
                    if mine == Some(id) {
                        let (dx, dz) = (dir.sin(), dir.cos());
                        let mut d = 4.0;
                        while d < GUN_RANGE {
                            let (px, pz) = (t.x + dx * d, t.z + dz * d);
                            if in_building(px, pz, 0.0) {
                                break;
                            }
                            let fade = 1.0 - d / GUN_RANGE;
                            frame.draw(
                                &marker,
                                Mat4::translate(vec3(px, 0.0, pz)) * Mat4::scale(0.7),
                                (tint.0 * fade, tint.1 * fade, tint.2 * fade),
                                true,
                            );
                            d += 3.5;
                        }
                    }
                }

                // Bullets, off their interp pool: hot tracer slivers.
                for (_, bl) in w.bullet_draw.get().iter() {
                    frame.draw(
                        &tracer,
                        Mat4::translate(vec3(bl.x, 0.0, bl.z)) * Mat4::rot_y(bl.heading),
                        (1.0, 0.92, 0.45),
                        true,
                    );
                }

                // The horde, off its interp pool; biomod green fades to
                // livid red as hp drops.
                for (_, h) in w.hog_draw.get().iter() {
                    let hp = (h.hp / HOG_HP).clamp(0.0, 1.0);
                    let tint = (0.45 + 0.4 * (1.0 - hp), 0.42 * hp + 0.10, 0.28 * hp + 0.06);
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
                    let (scale, col) = if c.kind == IMPACT_BOOM {
                        (5.0, (1.0, 0.45, 0.05))
                    } else if c.kind == IMPACT_KILL {
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

                // HP + boost heat, bottom left, as real bars
                // (`frame.rect`, riding the text compute pass). hp is
                // server truth read RAW off its synced pool — never
                // predicted, so a bite shows exactly when the server
                // says so. heat is predicted state, so the meter moves
                // the instant shift goes down, no round trip.
                if let Some(id) = mine {
                    let hp = w
                        .health
                        .get_id(id)
                        .map_or(0.0, |v| (v.hp / TRUCK_HP).clamp(0.0, 1.0));
                    let heat = w.pred.get().state().map_or(0.0, |t| t.heat.clamp(0.0, 1.0));
                    let hp_col = (
                        (60.0 + 195.0 * (1.0 - hp)) as u8,
                        (60.0 + 175.0 * hp) as u8,
                        60,
                    );
                    let heat_col = if heat > 0.75 {
                        (255, 60, 40) // about to cook off
                    } else {
                        (235, 175, 70)
                    };
                    let (x, bw, bh) = (24.0, 240.0, 16.0);
                    let y = H as f32 - 76.0;
                    for (row, fill, col, label) in
                        [(0.0, hp, hp_col, "hp"), (1.0, heat, heat_col, "heat")]
                    {
                        let ry = y + row * (bh + 10.0);
                        frame.rect(x, ry, bw, bh, (12, 14, 12), 0.6);
                        frame.rect(x + 2.0, ry + 2.0, (bw - 4.0) * fill, bh - 4.0, col, 0.95);
                        frame.text(label, x + bw + 10.0, ry - 2.0, 18.0, (215, 220, 210));
                    }
                }
            }
            if pm.tick() % 30 == 0 {
                let speed = w
                    .pred
                    .get()
                    .state()
                    .map(|t| t.speed * 3.6 / 1.6)
                    .unwrap_or(0.0);
                let title = format!(
                    "pm hogs — peer {}  {:.0} mph  rtt {:.0} ms  corrections {}  (wasd + mouse)",
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
