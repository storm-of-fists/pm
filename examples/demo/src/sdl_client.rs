//! SDL3 player client: real window, real held-key input (no terminal
//! latch workarounds), same shared netcode as the terminal client —
//! prediction, reconciliation, and dead reckoning all come from
//! `add_client_tasks`. The pm loop drives SDL; there is no second loop.

use pm_sdl::sdl3;
use pm::{NetClient, Pm};
use pm_sdl::sdl3::event::Event;
use pm_sdl::sdl3::keyboard::Scancode;
use pm_sdl::sdl3::pixels::Color;
use pm_sdl::sdl3::render::{FPoint, FRect};

use super::{ADDR, Car, CurCmd, Drive, Stats, WORLD, add_client_tasks, client_connect};

const SIZE: f32 = 800.0;
const CAR: f32 = 18.0;

fn to_screen(x: f32, y: f32) -> (f32, f32) {
    ((x / WORLD) * (SIZE / 2.0 - 20.0) + SIZE / 2.0, SIZE / 2.0 - (y / WORLD) * (SIZE / 2.0 - 20.0))
}

pub fn run() {
    let mut pm = Pm::new();
    let car = pm.pool::<Car>("car");
    let draw = pm.pool::<Car>("car_draw");

    let mut net = NetClient::new();
    net.pool_sync("car", &car);
    let quic = client_connect(&net);
    eprintln!("connecting to {ADDR} (sdl) ...");
    add_client_tasks(&mut pm, quic, net, &car, &draw);

    let sdl = sdl3::init().expect("sdl init");
    let video = sdl.video().expect("sdl video");
    let window = video
        .window("pm demo — you are yellow (wasd, esc quits)", SIZE as u32, SIZE as u32)
        .position_centered()
        .build()
        .expect("window");
    let mut canvas = window.into_canvas();
    let mut pump = sdl.event_pump().expect("event pump");

    let cmd = pm.single::<CurCmd>("cmd");
    let stats = pm.single::<Stats>("stats");

    // Real key-up events at last: held state read once per tick.
    pm.task_add("input", 4.0, {
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
            let held = |sc: Scancode| k.is_scancode_pressed(sc);
            cmd.borrow_mut().0 = Drive {
                thrust: if held(Scancode::W) {
                    1.0
                } else if held(Scancode::S) {
                    -0.7
                } else {
                    0.0
                },
                turn: if held(Scancode::A) {
                    1.0
                } else if held(Scancode::D) {
                    -1.0
                } else {
                    0.0
                },
            };
        }
    });

    pm.task_add("render", 50.0, {
        let draw = draw.clone();
        let stats = stats.clone();
        move |pm| {
            let s = stats.borrow();
            canvas.set_draw_color(Color::RGB(16, 18, 24));
            canvas.clear();
            canvas.set_draw_color(Color::RGB(60, 64, 80));
            let m = 20.0;
            let _ = canvas.draw_rect(FRect::new(m, m, SIZE - 2.0 * m, SIZE - 2.0 * m));
            for (id, c) in draw.borrow().iter() {
                let (sx, sy) = to_screen(c.x, c.y);
                let mine = Some(id) == s.mine;
                canvas.set_draw_color(if mine {
                    Color::RGB(250, 210, 40)
                } else {
                    Color::RGB(90, 140, 220)
                });
                let _ =
                    canvas.fill_rect(FRect::new(sx - CAR / 2.0, sy - CAR / 2.0, CAR, CAR));
                // Heading line.
                let (hx, hy) = (c.heading.cos(), -c.heading.sin());
                canvas.set_draw_color(Color::RGB(240, 240, 240));
                let _ = canvas.draw_line(
                    FPoint::new(sx, sy),
                    FPoint::new(sx + hx * CAR, sy + hy * CAR),
                );
            }
            canvas.present();

            if pm.tick() % 30 == 0 {
                let title = format!(
                    "pm demo — peer {}  rtt {:.0}ms  corrections {}  (wasd, esc quits)",
                    pm.local_peer, s.rtt_ms, s.corrections,
                );
                let _ = canvas.window_mut().set_title(&title);
            }
        }
    });

    pm.loop_rate = 60;
    pm.loop_run();
}
