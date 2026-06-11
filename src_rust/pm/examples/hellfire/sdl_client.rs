//! Hellfire SDL3 player client: window, WASD + mouse-aim input, sprite
//! rendering with hot-reload. World coordinates equal window coordinates
//! (900x700, y down) — same convention as the C++ original.

use pm::{Cooldown, Hysteresis, Pm, QuicClient, Sprite, Task, TaskError};
use sdl3::event::Event;
use sdl3::keyboard::Scancode;
use sdl3::mouse::MouseButton;
use sdl3::pixels::Color;
use sdl3::render::FRect;
use sdl3::render::Canvas;
use sdl3::video::Window;

use crate::client::{BTN_RESTART, CurInput, NetStats, add_client_tasks, client_pools, current_status};
use crate::common::*;

fn resource(name: &str) -> String {
    format!("{}/examples/hellfire/resources/{name}", env!("CARGO_MANIFEST_DIR"))
}

/// The render task: owns the canvas and sprites as fields (this is what
/// the Task lifecycle is for — `start` loads assets, `run` draws,
/// hot-reload polls once a second).
struct RenderTask {
    canvas: Canvas<Window>,
    front: Option<Sprite>,
    back: Option<Sprite>,
    facing_back: [Hysteresis<bool>; 8],
    prev_y: [f32; 8],
    reload: Cooldown,
    title: Cooldown,
    pools: crate::client::Pools,
    stats: pm::Single<NetStats>,
}

impl Task for RenderTask {
    fn start(&mut self, _pm: &mut Pm) -> Result<(), TaskError> {
        self.front = Some(Sprite::load(&self.canvas, resource("connor-front.png")));
        self.back = Some(Sprite::load(&self.canvas, resource("connor-back.png")));
        Ok(())
    }

    fn run(&mut self, pm: &mut Pm) -> Result<(), TaskError> {
        let dt = pm.loop_dt();
        if self.reload.ready(dt) {
            for s in [self.front.as_mut(), self.back.as_mut()].into_iter().flatten() {
                if s.changed() {
                    s.reload(&self.canvas);
                    eprintln!("[client] sprite reloaded");
                }
            }
        }

        let st = current_status(&self.pools.status);
        let over = st.flags & FLAG_GAME_OVER != 0;

        let c = &mut self.canvas;
        c.set_draw_color(Color::RGB(14, 14, 20));
        c.clear();
        c.set_draw_color(Color::RGB(60, 64, 80));
        let _ = c.draw_rect(FRect::new(0.0, 0.0, W, H));

        for (_, m) in self.pools.monster_draw.borrow().iter() {
            c.set_draw_color(Color::RGB(m.color[0], m.color[1], m.color[2]));
            let s = m.size;
            let _ = c.fill_rect(FRect::new(m.pos.x - s * 0.5, m.pos.y - s * 0.5, s, s));
        }
        for (_, b) in self.pools.bullet_draw.borrow().iter() {
            c.set_draw_color(if b.player_owned == 1 {
                Color::RGB(255, 255, 160)
            } else {
                Color::RGB(255, 90, 90)
            });
            let s = b.size * 2.0;
            let _ = c.fill_rect(FRect::new(b.pos.x - s * 0.5, b.pos.y - s * 0.5, s, s));
        }

        let my_peer = self.stats.borrow().peer as u32;
        for (_, p) in self.pools.player_draw.borrow().iter() {
            if p.alive == 0 {
                continue;
            }
            let i = (p.peer.max(1) as usize - 1) % 8;
            // Walking up-screen shows the back sprite; hysteresis kills
            // flicker at the turn point.
            let dy = p.pos.y - self.prev_y[i];
            self.prev_y[i] = p.pos.y;
            self.facing_back[i].update(dt);
            if dy.abs() > 0.3 {
                self.facing_back[i].set(dy < 0.0);
            }
            let sprite = if self.facing_back[i].get() { &self.back } else { &self.front };
            if let Some(s) = sprite.as_ref().filter(|s| s.loaded()) {
                s.draw_centered(c, p.pos.x, p.pos.y, PLAYER_SIZE);
            } else {
                c.set_draw_color(Color::RGB(p.color[0], p.color[1], p.color[2]));
                let _ = c.fill_rect(FRect::new(
                    p.pos.x - PLAYER_SIZE * 0.4,
                    p.pos.y - PLAYER_SIZE * 0.4,
                    PLAYER_SIZE * 0.8,
                    PLAYER_SIZE * 0.8,
                ));
            }
            // HP bar, own player ringed in their color.
            let bw = PLAYER_SIZE * 0.9;
            let bx = p.pos.x - bw * 0.5;
            let by = p.pos.y - PLAYER_SIZE * 0.62;
            c.set_draw_color(Color::RGB(40, 40, 48));
            let _ = c.fill_rect(FRect::new(bx, by, bw, 5.0));
            c.set_draw_color(if p.peer == my_peer {
                Color::RGB(120, 255, 120)
            } else {
                Color::RGB(p.color[0], p.color[1], p.color[2])
            });
            let _ = c.fill_rect(FRect::new(bx, by, bw * (p.hp / PLAYER_HP).max(0.0), 5.0));
        }

        if over {
            c.set_draw_color(Color::RGBA(0, 0, 0, 140));
            let _ = c.fill_rect(FRect::new(0.0, 0.0, W, H));
        }
        c.present();

        if self.title.ready(dt) {
            let s = self.stats.borrow();
            let me = self
                .pools
                .player
                .borrow()
                .iter()
                .find(|(_, p)| p.peer == my_peer)
                .map(|(_, p)| p.hp)
                .unwrap_or(0.0);
            let state = if st.flags & FLAG_WIN != 0 {
                "YOU WIN — R restarts"
            } else if over {
                "GAME OVER — R restarts"
            } else if st.level_flash > 0.0 {
                "LEVEL UP"
            } else {
                ""
            };
            let title = format!(
                "hellfire — score {}  kills {}  level {}  hp {:.0}  rtt {:.0}ms  {}",
                st.score,
                st.kills,
                st.level + 1,
                me,
                s.rtt_ms,
                state,
            );
            let _ = c.window_mut().set_title(&title);
        }
        Ok(())
    }
}

pub fn run() {
    let mut pm = Pm::new();
    let (pools, net) = client_pools(&mut pm);
    let quic = QuicClient::connect(ADDR, &net.schema()).expect("connect");
    eprintln!("connecting to {ADDR} ...");
    let name = std::env::var("HELLFIRE_NAME").unwrap_or_else(|_| "player".into());
    add_client_tasks(&mut pm, quic, net, &pools, name);

    let sdl = sdl3::init().expect("sdl init");
    let video = sdl.video().expect("sdl video");
    let window = video
        .window("hellfire — wasd moves, mouse aims, esc quits", W as u32, H as u32)
        .position_centered()
        .build()
        .expect("window");
    let canvas = window.into_canvas();
    let mut pump = sdl.event_pump().expect("event pump");

    let cmd = pm.single::<CurInput>("cmd");
    let stats = pm.single::<NetStats>("net_stats");

    pm.task_fn("input", 4.0, {
        let cmd = cmd.clone();
        move |pm| {
            let mut restart = false;
            for ev in pump.poll_iter() {
                match ev {
                    Event::Quit { .. }
                    | Event::KeyDown { scancode: Some(Scancode::Escape), .. } => pm.loop_quit(),
                    Event::KeyDown { scancode: Some(Scancode::R), .. } => restart = true,
                    _ => {}
                }
            }
            let k = pump.keyboard_state();
            let held = |sc: Scancode| k.is_scancode_pressed(sc);
            let m = pump.mouse_state();
            let mut c = cmd.borrow_mut();
            let keep = c.0.buttons & BTN_RESTART; // until the net task sends it
            c.0 = InputCmd {
                dx: (held(Scancode::D) as i32 - held(Scancode::A) as i32) as f32,
                dy: (held(Scancode::S) as i32 - held(Scancode::W) as i32) as f32,
                ax: m.x(),
                ay: m.y(),
                buttons: keep
                    | if m.is_mouse_button_pressed(MouseButton::Left)
                        || held(Scancode::Space)
                    {
                        BTN_SHOOT
                    } else {
                        0
                    }
                    | if restart { BTN_RESTART } else { 0 },
            };
        }
    });

    pm.task_add(
        "render",
        70.0,
        RenderTask {
            canvas,
            front: None,
            back: None,
            facing_back: std::array::from_fn(|_| Hysteresis::new(false, 0.15)),
            prev_y: [0.0; 8],
            reload: Cooldown::new(1.0),
            title: Cooldown::new(0.25),
            pools,
            stats,
        },
    );

    pm.loop_rate = 60;
    pm.loop_run();
}
