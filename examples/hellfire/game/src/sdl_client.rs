//! Hellfire SDL3 player client: window, WASD + mouse-aim input, camera
//! zoom (wheel) with player follow, sprite rendering with hot-reload,
//! TTF HUD, lobby screen, and an F1 debug overlay.

use pm::{Cooldown, Hysteresis, Pm, task, vec2};
use pm_sdl::sdl3::event::Event;
use pm_sdl::sdl3::keyboard::Scancode;
use pm_sdl::sdl3::mouse::MouseButton;
use pm_sdl::sdl3::pixels::Color;
use pm_sdl::sdl3::render::FRect;
use pm_sdl::{Font, Sprite};

use crate::client::{Camera, Ui, add_client_tasks, client_pools};
use crate::common::*;

fn resource(name: &str) -> String {
    format!("{}/resources/{name}", env!("CARGO_MANIFEST_DIR"))
}

fn roster_name(r: &Roster) -> String {
    let end = r.name.iter().position(|&b| b == 0).unwrap_or(r.name.len());
    String::from_utf8_lossy(&r.name[..end]).into_owned()
}

pub fn run() {
    let mut pm = Pm::client(ADDR, 60.0);
    let pools = client_pools(&mut pm);
    eprintln!("connecting to {ADDR} ...");
    let name = std::env::var("HELLFIRE_NAME").unwrap_or_else(|_| "player".into());
    let net = add_client_tasks(&mut pm, &pools, name);

    let (window, mut pump, refresh) = pm_sdl::window("hellfire", W as u32, H as u32);
    let mut canvas = window.into_canvas();

    let cam = pm.single::<Camera>("camera");
    let ui = pm.single::<Ui>("ui");
    // THE continuous input channel plus the discrete intents — the same
    // channel set the server (and every other client) registers.
    let input = pm.input::<InputCmd>("input");
    let starts = pm.event::<Start>("start");
    let restarts = pm.event::<Restart>("restart");

    // Input + camera: wheel zooms (toward 0.5x..3x), camera follows your
    // player, mouse aim goes through the inverse camera transform so it
    // stays correct at any zoom.
    let player_draw = pools.player_draw.clone();
    let status = pools.status.clone();
    task!(pm, "input", 4.0, [cam, ui, net], move |pm| {
        let in_lobby = status.get().flags & FLAG_STARTED == 0;
        for ev in pump.poll_iter() {
            match ev {
                Event::Quit { .. }
                | Event::KeyDown {
                    scancode: Some(Scancode::Escape),
                    ..
                } => pm.loop_quit(),
                Event::KeyDown {
                    scancode: Some(Scancode::R),
                    ..
                } => {
                    restarts.send(Restart::default());
                }
                Event::KeyDown {
                    scancode: Some(Scancode::Return),
                    ..
                } if in_lobby => {
                    starts.send(Start::default());
                }
                Event::KeyDown {
                    scancode: Some(Scancode::F1),
                    ..
                } => {
                    let mut u = ui.get_mut();
                    u.show_debug = !u.show_debug;
                }
                Event::MouseWheel { y, .. } => {
                    let mut cam = cam.get_mut();
                    cam.target_zoom = (cam.target_zoom * 1.15f32.powf(y)).clamp(0.5, 3.0);
                }
                _ => {}
            }
        }

        // Camera dynamics: ease zoom, follow own player (the entity
        // the server marked ours, read from the smoothed draw pool).
        let dt = pm.loop_dt();
        let me = net
            .mine()
            .and_then(|id| player_draw.get_id(id).map(|p| p.pos));
        {
            let mut cam = cam.get_mut();
            let dz = cam.target_zoom - cam.zoom;
            cam.zoom += dz * (10.0 * dt).min(1.0);
            let target = me.unwrap_or(vec2(W * 0.5, H * 0.5));
            let dc = target - cam.center;
            cam.center += dc * (6.0 * dt).min(1.0);
        }

        let k = pump.keyboard_state();
        let held = |sc: Scancode| k.is_scancode_pressed(sc);
        let m = pump.mouse_state();
        let aim = cam.get().to_world(m.x(), m.y());
        input.set(InputCmd {
            dx: (held(Scancode::D) as i32 - held(Scancode::A) as i32) as f32,
            dy: (held(Scancode::S) as i32 - held(Scancode::W) as i32) as f32,
            ax: aim.x,
            ay: aim.y,
            buttons: if m.is_mouse_button_pressed(MouseButton::Left) || held(Scancode::Space) {
                BTN_SHOOT
            } else {
                0
            },
        });
    });

    // Render: all world drawing goes through the camera transform; HUD
    // and overlay text draw in screen space with the TTF font.
    let mut front = Sprite::load(&canvas, resource("connor-front.png"));
    let mut back = Sprite::load(&canvas, resource("connor-back.png"));
    let mut font = match Font::load_default() {
        Ok(f) => Some(f),
        Err(e) => {
            eprintln!("[client] no font, HUD text disabled: {e}");
            None
        }
    };
    let mut facing_back: [Hysteresis<bool>; 8] =
        std::array::from_fn(|_| Hysteresis::new(false, 0.15));
    let mut prev_y = [0.0f32; 8];
    let mut reload = Cooldown::new(1.0);
    let mut fps = 60.0f32;
    task!(pm, "render", 70.0, [cam, ui, net], move |pm| {
        let dt = pm.loop_dt();
        fps += ((1.0 / dt.max(1e-6)) - fps) * 0.05;
        if reload.ready(dt) {
            for s in [&mut front, &mut back] {
                if s.changed() {
                    s.reload(&canvas);
                    eprintln!("[client] sprite reloaded");
                }
            }
        }

        let st = pools.status.get();
        let started = st.flags & FLAG_STARTED != 0;
        let over = st.flags & FLAG_GAME_OVER != 0;
        let cam = *cam.get();
        let z = cam.zoom;

        let c = &mut canvas;
        c.set_draw_color(Color::RGB(14, 14, 20));
        c.clear();
        c.set_draw_color(Color::RGB(60, 64, 80));
        let (bx, by) = cam.to_screen(vec2(0.0, 0.0));
        let _ = c.draw_rect(FRect::new(bx, by, W * z, H * z));

        for (_, m) in pools.monster_draw.get().iter() {
            c.set_draw_color(Color::RGB(m.color[0], m.color[1], m.color[2]));
            let s = m.size * z;
            let (x, y) = cam.to_screen(m.pos);
            let _ = c.fill_rect(FRect::new(x - s * 0.5, y - s * 0.5, s, s));
        }
        for (_, b) in pools.bullet_draw.get().iter() {
            c.set_draw_color(if b.player_owned == 1 {
                Color::RGB(255, 255, 160)
            } else {
                Color::RGB(255, 90, 90)
            });
            let s = b.size * 2.0 * z;
            let (x, y) = cam.to_screen(b.pos);
            let _ = c.fill_rect(FRect::new(x - s * 0.5, y - s * 0.5, s, s));
        }

        let mine = net.mine();
        let my_peer = net.peer() as u32; // roster rows are peer-keyed
        for (id, p) in pools.player_draw.get().iter() {
            if p.alive == 0 {
                continue;
            }
            // Sprite/hysteresis slot by controlling player — the header's
            // ownership table, not a pod field (id.peer() is recycling).
            let i = (net.owner_of(id).unwrap_or(1).max(1) as usize - 1) % 8;
            // Walking up-screen shows the back sprite; hysteresis
            // kills flicker at the turn point.
            let dy = p.pos.y - prev_y[i];
            prev_y[i] = p.pos.y;
            facing_back[i].update(dt);
            if dy.abs() > 0.3 {
                facing_back[i].set(dy < 0.0);
            }
            let sprite = if facing_back[i].get() { &back } else { &front };
            let (x, y) = cam.to_screen(p.pos);
            let size = PLAYER_SIZE * z;
            if sprite.loaded() {
                sprite.draw_centered(c, x, y, size);
            } else {
                c.set_draw_color(Color::RGB(p.color[0], p.color[1], p.color[2]));
                let _ = c.fill_rect(FRect::new(
                    x - size * 0.4,
                    y - size * 0.4,
                    size * 0.8,
                    size * 0.8,
                ));
            }
            // HP bar, own player's in green.
            let bw = size * 0.9;
            let hx = x - bw * 0.5;
            let hy = y - size * 0.62;
            c.set_draw_color(Color::RGB(40, 40, 48));
            let _ = c.fill_rect(FRect::new(hx, hy, bw, 5.0));
            c.set_draw_color(if mine == Some(id) {
                Color::RGB(120, 255, 120)
            } else {
                Color::RGB(p.color[0], p.color[1], p.color[2])
            });
            let _ = c.fill_rect(FRect::new(hx, hy, bw * (p.hp / PLAYER_HP).max(0.0), 5.0));
        }

        // --- screen-space text ------------------------------------
        if let Some(f) = font.as_mut() {
            if started {
                // Own hp reads RAW from the authoritative pool by id —
                // server-owned state, never smoothed.
                let me_hp = mine
                    .and_then(|id| pools.player.get_id(id).map(|p| p.hp))
                    .unwrap_or(0.0);
                let hud = format!(
                    "score {}   kills {}   level {}   hp {:.0}",
                    st.score,
                    st.kills,
                    st.level + 1,
                    me_hp,
                );
                f.draw(c, &hud, 12.0, 8.0, 18.0, (230, 230, 235));

                if st.level_flash > 0.0 {
                    let text = format!("LEVEL {}", st.level + 1);
                    let w = f.measure(&text, 44.0);
                    f.draw(c, &text, (W - w) * 0.5, H * 0.30, 44.0, (255, 220, 120));
                }
            } else {
                // Lobby: title, roster, start hint.
                c.set_draw_color(Color::RGBA(0, 0, 0, 120));
                let _ = c.fill_rect(FRect::new(0.0, 0.0, W, H));
                let tw = f.measure("HELLFIRE", 64.0);
                f.draw(c, "HELLFIRE", (W - tw) * 0.5, 110.0, 64.0, (255, 120, 60));
                let mut y = 240.0;
                f.draw(c, "pilots:", W * 0.5 - 120.0, y, 20.0, (160, 160, 170));
                y += 30.0;
                for (_, r) in pools.roster.get().iter() {
                    let mut label = roster_name(r);
                    if r.peer == my_peer {
                        label.push_str("  (you)");
                    }
                    f.draw(c, &label, W * 0.5 - 120.0, y, 20.0, (230, 230, 235));
                    y += 26.0;
                }
                let hint = "ENTER to start    wasd move    mouse aims    wheel zooms";
                let hw = f.measure(hint, 20.0);
                f.draw(c, hint, (W - hw) * 0.5, H - 120.0, 20.0, (200, 200, 120));
            }

            if over {
                c.set_draw_color(Color::RGBA(0, 0, 0, 140));
                let _ = c.fill_rect(FRect::new(0.0, 0.0, W, H));
                let text = if st.flags & FLAG_WIN != 0 {
                    "YOU WIN"
                } else {
                    "GAME OVER"
                };
                let w = f.measure(text, 56.0);
                f.draw(c, text, (W - w) * 0.5, H * 0.36, 56.0, (255, 90, 90));
                let sub = format!("score {}   R to restart", st.score);
                let sw = f.measure(&sub, 22.0);
                f.draw(
                    c,
                    &sub,
                    (W - sw) * 0.5,
                    H * 0.36 + 70.0,
                    22.0,
                    (220, 220, 225),
                );
            }

            // F1 debug overlay: client loop + net + server dbg + tasks.
            if ui.get().show_debug {
                c.set_draw_color(Color::RGBA(0, 0, 0, 170));
                let _ = c.fill_rect(FRect::new(8.0, 34.0, 360.0, 250.0));
                let dbg = pools.dbg.get();
                let mut lines = vec![
                    format!("fps {fps:.0}   frame {:.2} ms", dt * 1000.0),
                    format!("rtt {:.0} ms   snapshots {}", net.rtt_ms(), net.snapshots()),
                    format!(
                        "draw: {} monsters  {} bullets  {} players",
                        pools.monster_draw.get().len(),
                        pools.bullet_draw.get().len(),
                        pools.player_draw.get().len(),
                    ),
                    format!(
                        "server: {} monsters  {} bullets  tick {:.2} ms",
                        dbg.monsters, dbg.bullets, dbg.tick_ms,
                    ),
                    format!("zoom {:.2}", z),
                    String::new(),
                ];
                for (name, stat) in pm.task_stats().into_iter().take(5) {
                    if stat.calls > 0 {
                        lines.push(format!(
                            "{:10} {:7.1} us avg  {:5.1} ms max",
                            name,
                            stat.ns_total as f32 / stat.calls as f32 / 1000.0,
                            stat.ns_max as f32 / 1e6,
                        ));
                    }
                }
                let mut ty = 40.0;
                for line in lines {
                    f.draw(c, &line, 16.0, ty, 15.0, (170, 240, 170));
                    ty += 20.0;
                }
            }
        }

        c.present();
    });

    // Display refresh paces the loop (WSLg ignores vsync; input still
    // goes out at a fixed 60 Hz via the net module's send cadence).
    pm.loop_rate = refresh;
    pm.run().expect("connect");
}
