//! Hogs' 3D player client: chase-cam truck OR helicopter, the horde and
//! the bullet tracers rendered off their interp draw pools, hit/kill/
//! bite markers straight off the TTL'd synced pool, and a local aim
//! line (client-side only feedback).
//!
//! TRUCK: WASD drives; hold RIGHT mouse to swing the turret AND the
//! camera (both ease back on release); LEFT mouse (or SPACE) fires;
//! SHIFT boosts — watch the heat bar, 1.0 is an explosion.
//! HELI: W/S pitches the nose (nose down = forward), A/D yaws, SPACE
//! climbs, CTRL descends, LEFT mouse fires the nose gun — dive to
//! strafe. Hogs can leap at a low hover; climb and they lose you.
//! H respawns as the heli, T as the truck, R as whatever you are;
//! 1/2/3 switch cameras; P toggles panini; Esc quits.
//!
//! The per-vehicle KEY CONTEXTS below (same keys, different Drive
//! fields) are hand-rolled — this is the seam the input-map subsystem
//! will eventually own.

use pm::{
    CamAnchor, CamRig, CamView, Mat4, Pm, Vec3, camera_install, camera_manager, camera_track, vec3,
};
use pm_sdl::gpu3d::{Frame3, Instance3, Renderer3d, bake, box_tris, checker_ground, panini_for_fov};
use pm_sdl::sdl3;
use sdl3::event::Event;
use sdl3::keyboard::Scancode;
use sdl3::mouse::MouseButton;

use crate::bot_client::client_setup;
use crate::common::*;

const W: u32 = 1280;
const H: u32 = 800;

fn hog_model(h: &Hog) -> Mat4 {
    Mat4::translate(vec3(h.x, 0.0, h.z)) * Mat4::rot_y(h.heading)
}

/// A dead hog, tumbling: CLIENT-SIDE presentation physics (GPU/CPU
/// particle-tier — nothing on the wire). A kill is an entity REMOVAL on
/// the wire; each client turns that edge into its own ragdoll from the
/// hog's last replicated state. Clients may disagree on exactly how a
/// corpse tumbles — cosmetic, so nobody cares; that's the line between
/// server physics (gameplay) and client physics (feel).
struct Corpse {
    x: f32,
    y: f32,
    z: f32,
    vx: f32,
    vy: f32,
    vz: f32,
    yaw: f32,
    /// Tumble: angle + rate about the horizontal axis across the launch.
    ang: f32,
    spin: f32,
    t: f32,
}

const CORPSE_LIFE: f32 = 2.6;

impl Corpse {
    fn from_hog(h: &Hog, rng: &mut pm::Rng) -> Corpse {
        Corpse {
            x: h.x,
            y: 0.45,
            z: h.z,
            // Launched along its final run plus a pop upward — reads as
            // the killing shot bowling it over.
            vx: h.heading.sin() * h.speed * 0.9 + rng.rfr(-1.5, 1.5),
            vy: rng.rfr(4.0, 7.0),
            vz: h.heading.cos() * h.speed * 0.9 + rng.rfr(-1.5, 1.5),
            yaw: h.heading,
            ang: 0.0,
            spin: rng.rfr(5.0, 11.0) * if rng.rf() < 0.5 { -1.0 } else { 1.0 },
            t: 0.0,
        }
    }

    /// Ballistic + damped ground bounces; true while still visible.
    fn step(&mut self, dt: f32) -> bool {
        self.t += dt;
        self.vy -= 22.0 * dt;
        self.x += self.vx * dt;
        self.y += self.vy * dt;
        self.z += self.vz * dt;
        self.ang += self.spin * dt;
        if self.y < 0.25 {
            self.y = 0.25;
            if self.vy < -1.0 {
                self.vy = -self.vy * 0.35; // bounce, losing most of it
                self.vx *= 0.55;
                self.vz *= 0.55;
                self.spin *= 0.5;
            } else {
                self.vy = 0.0;
                self.vx *= 0.82;
                self.vz *= 0.82;
                self.spin *= 0.8; // settle
            }
        }
        self.t < CORPSE_LIFE
    }

    fn model(&self) -> Mat4 {
        // Tumble about the horizontal axis perpendicular to the launch
        // direction — a forward somersault, not a spinning top.
        Mat4::translate(vec3(self.x, self.y, self.z))
            * Mat4::rot_y(self.yaw)
            * Mat4::rot_x(self.ang)
    }
}

/// Faked bold: overdraw at small offsets (gpu3d font is regular-only).
fn hud_bold(frame: &mut Frame3, s: &str, x: f32, y: f32, px: f32, col: (u8, u8, u8)) {
    for (dx, dy) in [(0.0, 0.0), (1.4, 0.0), (0.0, 1.4), (1.4, 1.4)] {
        frame.text(s, x + dx, y + dy, px, col);
    }
}

pub fn run(flags: Flags) {
    let mut pm = Pm::client(ADDR, 1.0 / FIXED_DT);
    if flags.link != (0.0, 0.0) {
        pm.link_lag(flags.link.0, flags.link.1);
        eprintln!(
            "[hogs] simulated link: {} ms one-way, {:.1}% loss",
            flags.link.0,
            flags.link.1 * 100.0
        );
    }
    let w = client_setup(&mut pm);
    eprintln!("connecting to {ADDR} ...");
    let net = pm.net();
    // Live-tunable knobs (day length today) — seeded from flags here,
    // written by the telemetry node when a monitor turns the dial.
    let tune = pm.single::<Tune>("hogs.tune");
    tune.get_mut().day_secs = flags.day;
    // The telemetry node: watch (and tune) this session from pm-watch
    // or pm-mon. See telemetry.rs.
    crate::telemetry::install(&mut pm, &w, &flags);
    // The cosmetic gun's client-LOCAL pool (never synced): your own
    // shot's tracer + bang at the CLICK; see the input task below.
    let tracer_local = pm.pool::<Tracer>("tracer.local");
    // Sounds ride the same replicated facts the renderer draws — plus
    // the local tracer births for our instant bang. See sfx.rs.
    crate::sfx::install(&mut pm, &w, &tracer_local);

    camera_install(&mut pm);
    let cam_mgr = camera_manager(&mut pm);
    let cam_view = pm.single::<CamView>("cam.view");

    let (mut window, mut pump, refresh) = pm_sdl::window(
        &format!("pm hogs [{}] — wasd drives, rmb aims, lmb fires, 1-3 cams, r respawn, esc", pm::BUILD_ID),
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
    // Bullet tracer: a stretched sliver, centered — bullets carry their
    // own altitude now, so the height rides the translate.
    let tracer_mesh = r3d
        .upload_mesh(&bake(
            &box_tris(vec3(-0.07, -0.1, -0.65), vec3(0.07, 0.1, 0.65)),
            (1.0, 1.0, 1.0),
        ))
        .expect("tracer");
    // Heli: cabin pod + tail boom + skid plate + one rotor blade (drawn
    // spinning by tick), authored facing +z, baked white for peer tint.
    // The full quat attitude arrives via Body::model(), so the whole
    // airframe pitches into dives and banks into turns.
    let heli_body = r3d
        .upload_mesh(&bake(
            &box_tris(vec3(-0.9, 0.35, -1.3), vec3(0.9, 1.6, 1.6)),
            (1.0, 1.0, 1.0),
        ))
        .expect("heli body");
    let heli_tail = r3d
        .upload_mesh(&bake(
            &box_tris(vec3(-0.16, 0.85, -3.6), vec3(0.16, 1.35, -1.3)),
            (1.0, 1.0, 1.0),
        ))
        .expect("heli tail");
    let heli_skid = r3d
        .upload_mesh(&bake(
            &box_tris(vec3(-1.0, 0.0, -1.3), vec3(1.0, 0.2, 1.4)),
            (1.0, 1.0, 1.0),
        ))
        .expect("heli skid");
    let heli_rotor = r3d
        .upload_mesh(&bake(
            &box_tris(vec3(-3.1, 1.72, -0.16), vec3(3.1, 1.86, 0.16)),
            (1.0, 1.0, 1.0),
        ))
        .expect("heli rotor");
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
    // Blob shadow: a unit disc, drawn EMISSIVE dark so no light can wash
    // it out — on a flat arena this is 80% of what real shadows buy:
    // things stop floating. Scaled per entity; the heli's shrinks with
    // altitude.
    let shadow = {
        let n = 14;
        let tris: Vec<[Vec3; 3]> = (0..n)
            .map(|i| {
                let t = std::f32::consts::TAU;
                let a = i as f32 / n as f32 * t;
                let b = (i + 1) as f32 / n as f32 * t;
                [
                    vec3(0.0, 0.0, 0.0),
                    vec3(a.cos(), 0.0, a.sin()),
                    vec3(b.cos(), 0.0, b.sin()),
                ]
            })
            .collect();
        r3d.upload_mesh(&bake(&tris, (1.0, 1.0, 1.0))).expect("shadow")
    };

    // Turret angle is CLIENT state: holding RMB accumulates mouse-x into
    // it, releasing eases it back to dead ahead. The server only ever
    // sees the absolute angle in each command frame — the animation
    // replays exactly, so prediction never corrects over it.
    let mut aim = 0.0f32;

    // The cosmetic gun: your bang and tracer at the CLICK, from the
    // PREDICTED muzzle — the authoritative bullet still round-trips for
    // hits and for everyone else's eyes, and `Bullet::owner` hides our
    // late twins from the draw. Cooldown mirrors the server's; a tick
    // of drift is cosmetic-only.
    let mut gun_cd = 0.0f32;

    pm.task_add("input", 4.0, 0.0, {
        let cam_mgr = cam_mgr.clone();
        let respawn = w.respawn.clone();
        let input = w.input.clone();
        let pred = w.pred.clone();
        let pred_heli = w.pred_heli.clone();
        let tracer = tracer_local.clone();
        let params = w.params.clone();
        move |pm| {
            // The live predictor answers "am I flying?" — it decides
            // which KEY CONTEXT fills the (shared) input pod below.
            let flying = pred_heli.get().state().is_some();
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
                    } => respawn.send(Respawn {
                        vehicle: if flying { VEH_HELI } else { VEH_TRUCK },
                    }),
                    Event::KeyDown {
                        scancode: Some(Scancode::H),
                        repeat: false,
                        ..
                    } => respawn.send(Respawn { vehicle: VEH_HELI }),
                    Event::KeyDown {
                        scancode: Some(Scancode::T),
                        repeat: false,
                        ..
                    } => respawn.send(Respawn { vehicle: VEH_TRUCK }),
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
            let cmd = if flying {
                Drive {
                    thrust: 0.0,
                    turn: held(Scancode::D) - held(Scancode::A),
                    fire: lmb, // SPACE is the collective up here
                    aim,
                    boost: 0.0,
                    bot: 0.0,
                    pitch: held(Scancode::W) - held(Scancode::S),
                    lift: held(Scancode::Space) - held(Scancode::LCtrl),
                }
            } else {
                Drive {
                    thrust: held(Scancode::W) - 0.6 * held(Scancode::S),
                    turn: held(Scancode::D) - held(Scancode::A),
                    fire: lmb.max(held(Scancode::Space)),
                    aim,
                    boost: held(Scancode::LShift),
                    bot: 0.0, // human: crisp steering
                    pitch: 0.0,
                    lift: 0.0,
                }
            };
            input.set(cmd);
            // Cosmetic gun: fire the local tracer the same frame the
            // trigger reads down. The muzzle comes off whichever
            // predictor is live — the pose the server will agree with.
            gun_cd = (gun_cd - pm.loop_dt()).max(0.0);
            if cmd.fire > 0.5 && gun_cd <= 0.0 {
                let muzzle = pred
                    .get()
                    .state()
                    .map(|t| truck_muzzle(&t))
                    .or_else(|| pred_heli.get().state().map(|h| heli_muzzle(&h)));
                if let Some((x, y, z, heading, pitch)) = muzzle {
                    gun_cd = params.get().gun_cd;
                    let id = pm.id_add();
                    tracer.get_mut().add(
                        id,
                        Tracer {
                            x,
                            y,
                            z,
                            heading,
                            pitch,
                            left: params.get().gun_range,
                        },
                    );
                }
            }
        }
    });

    // Fly the local tracers on the render clock; they die on the same
    // walls the real bullet does (minus hogs — the server's Impact
    // flash is still the word on hits).
    pm.task_add("tracer.step", 30.0, 0.0, {
        let tracer = tracer_local.clone();
        let params = w.params.clone();
        move |pm| {
            let dt = pm.loop_dt();
            let spd = params.get().bullet_speed;
            let dead: Vec<_> = {
                let mut trs = tracer.get_mut();
                let ids: Vec<_> = trs.iter().map(|(id, _)| id).collect();
                ids.into_iter()
                    .filter(|&id| {
                        trs.get_mut(id).is_some_and(|mut tr| !tracer_step(&mut tr, dt, spd))
                    })
                    .collect()
            };
            for id in dead {
                pm.id_remove(id);
            }
        }
    });

    // Rig cameras the moment the server tells us which entity is ours,
    // then retire this task (drive's pattern). The SAMPLER stays alive
    // and dynamic: a vehicle swap is a fresh entity (new id), so it
    // re-resolves `mine()` every tick and reads whichever draw pool
    // holds it — one rack survives every swap.
    pm.task_add("rig_cams", 32.0, 0.0, {
        let truck_draw = w.truck_draw.clone();
        let heli_draw = w.heli_draw.clone();
        let net = net.clone();
        move |pm| {
            let Some(id) = net.mine() else {
                return;
            };
            let truck_draw = truck_draw.clone();
            let heli_draw = heli_draw.clone();
            let net = net.clone();
            {
                let mut rack = camera_track(pm, id, move |_pm| {
                    let cur = net.mine()?;
                    if let Some(t) = truck_draw.get_id(cur).map(|t| *t) {
                        // The anchor faces where the TURRET points, not
                        // where the truck drives: holding RMB swings the
                        // camera with the gun (and the ease-back swings
                        // it home), while plain steering under a held
                        // aim no longer drags the view around.
                        let dir = t.heading() + t.aim;
                        Some(CamAnchor {
                            pos: t.body.pos,
                            fwd: vec3(dir.sin(), 0.0, dir.cos()),
                        })
                    } else {
                        // Heli: follow the yaw, level — the airframe
                        // pitches and banks under a steady camera.
                        heli_draw.get_id(cur).map(|h| *h).map(|h| {
                            let yaw = h.body.yaw();
                            CamAnchor {
                                pos: h.body.pos,
                                fwd: vec3(yaw.sin(), 0.0, yaw.cos()),
                            }
                        })
                    }
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
        let tracer = tracer_local.clone();
        let tune = tune.clone();
        // Death-edge tracking for ragdolls: last tick's raw replicas, so
        // a removal still has the state to launch a corpse from.
        let mut prev_hogs: std::collections::HashMap<pm::Id, Hog> = Default::default();
        let mut corpses: Vec<Corpse> = Vec::new();
        // Muzzle flashes / explosions as short-lived point lights:
        // births on the local tracer pool (our click, 0 ms), remote
        // bullets, and boom impacts. (pos, seconds left, radius, color).
        let mut flash_tracer = pm::Births::default();
        let mut flash_bullet = pm::Births::default();
        let mut flash_boom = pm::Births::default();
        let mut flashes: Vec<(Vec3, f32, f32, (f32, f32, f32))> = Vec::new();
        move |pm| {
            // Spawn corpses off this frame's removals, then step them on
            // the render clock (client-local physics — cosmetic only).
            {
                let now: std::collections::HashMap<pm::Id, Hog> =
                    w.hog.get().iter().map(|(id, h)| (id, *h)).collect();
                let mut rng = pm::Rng::new(pm.tick().wrapping_mul(0x2545_F491) | 1);
                for (id, h) in &prev_hogs {
                    if !now.contains_key(id) {
                        corpses.push(Corpse::from_hog(h, &mut rng));
                    }
                }
                prev_hogs = now;
                let dt = pm.loop_dt();
                corpses.retain_mut(|c| c.step(dt));
            }
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

            // --- time of day: the sun is a pure function of the tick —
            // zero wire cost, and every session opens at dawn. One full
            // cycle every `day=` seconds (480 default; live-tunable via
            // the telemetry day_secs knob), biased sine keeps night
            // short.
            let tau = std::f32::consts::TAU;
            let day_secs = tune.get().day_secs.max(10.0);
            let ang = (pm.tick() as f32 * FIXED_DT / day_secs).fract() * tau;
            let elev = ang.sin() * 0.9 + 0.35; // -0.55 deep night .. 1.25 noon
            let lift = elev.clamp(0.0, 1.0);
            let up_ramp = (elev / 0.12).clamp(0.0, 1.0); // dawn/dusk ramp
            let warm = (1.0 - lift) * (1.0 - lift); // horizon redness while up
            let sun_str = 0.75 * up_ramp;
            r3d.sun_color = (
                sun_str * (0.95 + 0.05 * lift),
                sun_str * (0.62 + 0.33 * lift),
                sun_str * (0.42 + 0.53 * lift),
            );
            // Hemisphere ambient: blue-grey day; a moonlit floor at
            // night so the horde stays readable (playability > realism).
            let l = |night: f32, day: f32| night + (day - night) * up_ramp;
            r3d.ambient_sky = (l(0.10, 0.34), l(0.12, 0.38), l(0.20, 0.46));
            r3d.ambient_ground = (l(0.055, 0.30), l(0.06, 0.27), l(0.10, 0.24));
            // The horizon (clear + fog color) follows: day blue, dusk
            // ember, night near-black.
            r3d.clear_color = (
                l(0.015, 0.30 + 0.25 * warm),
                l(0.02, 0.38 - 0.08 * warm),
                l(0.05, 0.50 - 0.20 * warm),
            );
            let az = ang * 0.6 + 0.8; // lazy azimuth drift — aesthetics only
            let sun_dir = vec3(az.cos(), (elev + 0.1).max(0.06), az.sin());

            // --- point lights. Flashes first (they're the drama and
            // they're brief), then headlights nearest-first — the engine
            // keeps the first MAX_LIGHTS pushes and drops the rest.
            let me = net.peer() as f32;
            for id in flash_tracer.drain(&tracer.get()) {
                if let Some(tr) = tracer.get_id(id) {
                    flashes.push((vec3(tr.x, tr.y, tr.z), 0.07, 11.0, (1.3, 1.0, 0.55)));
                }
            }
            for id in flash_bullet.drain(&w.bullet.get()) {
                if let Some(b) = w.bullet.get_id(id)
                    && b.owner != me
                {
                    flashes.push((vec3(b.x, b.y, b.z), 0.07, 11.0, (1.3, 1.0, 0.55)));
                }
            }
            for id in flash_boom.drain(&w.impact.get()) {
                if let Some(c) = w.impact.get_id(id)
                    && c.kind == IMPACT_BOOM
                {
                    flashes.push((vec3(c.x, 1.2, c.z), 0.22, 26.0, (1.6, 0.7, 0.25)));
                }
            }
            {
                let dt = pm.loop_dt();
                flashes.retain_mut(|f| {
                    f.1 -= dt;
                    f.1 > 0.0
                });
            }
            for &(p, _, radius, color) in &flashes {
                r3d.point_light(p, color, radius);
            }
            // Headlights: a warm pool ahead of every truck's bumper, a
            // work light on every heli nose. Invisible by day (the sun
            // swamps them), free drama at dusk. Nearest trucks first so
            // they win what's left of the light budget.
            let (ex, ez) = w
                .pred
                .get()
                .state()
                .map(|t| (t.body.pos.x, t.body.pos.z))
                .or_else(|| {
                    w.pred_heli
                        .get()
                        .state()
                        .map(|h| (h.body.pos.x, h.body.pos.z))
                })
                .unwrap_or((0.0, 0.0));
            let mut lamps: Vec<(f32, Vec3)> = Vec::new();
            for (_, t) in w.truck_draw.get().iter() {
                let f = t.body.fwd();
                let p = t.body.pos + f * 4.2;
                let d2 = (p.x - ex) * (p.x - ex) + (p.z - ez) * (p.z - ez);
                lamps.push((d2, vec3(p.x, 0.5, p.z)));
            }
            for (_, hl) in w.heli_draw.get().iter() {
                let f = hl.body.fwd();
                let p = hl.body.pos + f * 2.5;
                let d2 = (p.x - ex) * (p.x - ex) + (p.z - ez) * (p.z - ez);
                lamps.push((d2, vec3(p.x, (p.y - 0.4).max(0.4), p.z)));
            }
            lamps.sort_by(|a, b| a.0.total_cmp(&b.0));
            for (_, p) in lamps {
                r3d.point_light(p, (0.75, 0.70, 0.55), 15.0);
            }

            // The horde, corpses, and tracers as INSTANCE batches — one
            // draw call each inside the frame, instead of three uniform
            // pushes per hog (the measured 32 ms @ 200 hogs cliff).
            // Staged before frame(): instance data uploads in a copy
            // pass ahead of the render pass.
            let gun_range = w.params.get().gun_range;
            let mut hog_shadows: Vec<Instance3> = Vec::new();
            let mut hog_bodies: Vec<Instance3> = Vec::new();
            let mut hog_snouts: Vec<Instance3> = Vec::new();
            for (_, h) in w.hog_draw.get().iter() {
                // Biomod green fades to livid red as hp drops.
                let hp = (h.hp / HOG_HP).clamp(0.0, 1.0);
                let tint = (0.45 + 0.4 * (1.0 - hp), 0.42 * hp + 0.10, 0.28 * hp + 0.06);
                let model = hog_model(h);
                hog_shadows.push(Instance3::emissive(
                    Mat4::translate(vec3(h.x, 0.02, h.z)) * Mat4::scale(0.95),
                    (0.035, 0.04, 0.035),
                ));
                hog_bodies.push(Instance3::new(model, tint));
                hog_snouts.push(Instance3::new(
                    model,
                    (tint.0 * 0.7, tint.1 * 0.7, tint.2 * 0.7),
                ));
            }
            // Ragdoll corpses ride the same meshes — same batches,
            // their own fading tints.
            for c in &corpses {
                let fade = (1.0 - c.t / CORPSE_LIFE).clamp(0.0, 1.0);
                let tint = (0.30 + 0.25 * fade, 0.16 * fade + 0.08, 0.10 * fade + 0.05);
                let model = c.model();
                hog_bodies.push(Instance3::new(model, tint));
                hog_snouts.push(Instance3::new(
                    model,
                    (tint.0 * 0.7, tint.1 * 0.7, tint.2 * 0.7),
                ));
            }
            // Tracers: everyone else's bullets (ours are hidden — the
            // local cosmetic pool drew them at the click) plus the local
            // pool, one emissive batch.
            let mut tracer_inst: Vec<Instance3> = Vec::new();
            for (_, bl) in w.bullet_draw.get().iter() {
                if bl.owner == me {
                    continue;
                }
                tracer_inst.push(Instance3::emissive(
                    Mat4::translate(vec3(bl.x, bl.y, bl.z))
                        * Mat4::rot_y(bl.heading)
                        * Mat4::rot_x(-bl.pitch),
                    (1.0, 0.92, 0.45),
                ));
            }
            for (_, tr) in tracer.get().iter() {
                tracer_inst.push(Instance3::emissive(
                    Mat4::translate(vec3(tr.x, tr.y, tr.z))
                        * Mat4::rot_y(tr.heading)
                        * Mat4::rot_x(-tr.pitch),
                    (1.0, 0.92, 0.45),
                ));
            }
            let hog_shadow_b = r3d.instances(&hog_shadows);
            let hog_body_b = r3d.instances(&hog_bodies);
            let hog_snout_b = r3d.instances(&hog_snouts);
            let tracer_b = r3d.instances(&tracer_inst);

            if let Some(mut frame) = r3d.frame(&window, view_mat, sun_dir) {
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
                    // Tint by controlling player via the replicated
                    // ownership table — id.peer() is recycling, not control.
                    let tint = peer_color(net.owner_of(id).unwrap_or(0));
                    let model = t.body.model();
                    frame.draw_emissive(
                        &shadow,
                        Mat4::translate(vec3(t.body.pos.x, 0.02, t.body.pos.z)) * Mat4::scale(1.7),
                        (0.035, 0.04, 0.035),
                        false,
                    );
                    frame.draw(&body, model, tint, true);
                    frame.draw(
                        &cabin,
                        model,
                        (tint.0 * 0.5, tint.1 * 0.5, tint.2 * 0.5),
                        true,
                    );
                    // Turret: same origin, rotated by heading + aim —
                    // replicated state, so every peer's swing is visible.
                    let dir = t.heading() + t.aim;
                    frame.draw(
                        &barrel,
                        Mat4::translate(t.body.pos) * Mat4::rot_y(dir),
                        (tint.0 * 0.35, tint.1 * 0.35, tint.2 * 0.35),
                        true,
                    );
                    // Own truck: local aim line under the turret, cut
                    // short by buildings — client-side feedback only,
                    // nothing on the wire.
                    if mine == Some(id) {
                        let (dx, dz) = (dir.sin(), dir.cos());
                        let mut d = 4.0;
                        while d < gun_range {
                            let (px, pz) = (t.body.pos.x + dx * d, t.body.pos.z + dz * d);
                            if in_building(px, pz, 0.0) {
                                break;
                            }
                            let fade = 1.0 - d / gun_range;
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

                // Helis: the full quat attitude rides Body::model(), so
                // the airframe noses into dives and banks into turns;
                // the rotor spins in model space on the render clock.
                let spin = Mat4::rot_y(pm.tick() as f32 * 0.7);
                for (id, hl) in w.heli_draw.get().iter() {
                    let tint = peer_color(net.owner_of(id).unwrap_or(0));
                    let model = hl.body.model();
                    // Shadow shrinks and stays put as the heli climbs —
                    // the cheap read on "how high am I".
                    let s = 1.9 * (1.0 - hl.body.pos.y / (HELI_CEIL * 1.7)).max(0.2);
                    frame.draw_emissive(
                        &shadow,
                        Mat4::translate(vec3(hl.body.pos.x, 0.03, hl.body.pos.z))
                            * Mat4::scale(s),
                        (0.035, 0.04, 0.035),
                        false,
                    );
                    frame.draw(&heli_body, model, tint, true);
                    frame.draw(
                        &heli_tail,
                        model,
                        (tint.0 * 0.6, tint.1 * 0.6, tint.2 * 0.6),
                        true,
                    );
                    frame.draw(&heli_skid, model, (0.2, 0.2, 0.22), true);
                    frame.draw(&heli_rotor, model * spin, (0.25, 0.25, 0.28), true);
                    // Own heli: breadcrumb the nose gun's ground
                    // intersection — where a dive is actually aimed.
                    if mine == Some(id) {
                        let (yaw, pitch, _) = hl.body.rot.to_yaw_pitch_roll();
                        if pitch > 0.02 {
                            let reach = ((hl.body.pos.y - 0.2) / pitch.tan()).min(gun_range);
                            let (px, pz) = (
                                hl.body.pos.x + yaw.sin() * reach,
                                hl.body.pos.z + yaw.cos() * reach,
                            );
                            frame.draw(
                                &marker,
                                Mat4::translate(vec3(px, 0.0, pz)) * Mat4::scale(1.2),
                                tint,
                                true,
                            );
                        }
                    }
                }

                // Tracers, corpses, and the horde: the instance batches
                // staged above frame() — four draw calls for the whole
                // crowd (tint/emissive ride per instance).
                frame.draw_instanced(&tracer_mesh, tracer_b, true);
                frame.draw_instanced(&shadow, hog_shadow_b, false);
                frame.draw_instanced(&hog_body, hog_body_b, true);
                frame.draw_instanced(&hog_snout, hog_snout_b, true);

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
                    // Emissive: a flash is light, not a lit thing.
                    frame.draw_emissive(
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
                    let hp_col = (
                        (60.0 + 195.0 * (1.0 - hp)) as u8,
                        (60.0 + 175.0 * hp) as u8,
                        60,
                    );
                    // Second bar is per-vehicle: boost heat in a truck,
                    // altitude in the heli (both predicted state — live,
                    // no round trip).
                    let (fill, col, label) = if let Some(hl) = w.pred_heli.get().state() {
                        let alt = (hl.body.pos.y / HELI_CEIL).clamp(0.0, 1.0);
                        (alt, (120, 190, 235), "alt")
                    } else {
                        let heat = w.pred.get().state().map_or(0.0, |t| t.heat.clamp(0.0, 1.0));
                        let heat_col = if heat > 0.75 {
                            (255, 60, 40) // about to cook off
                        } else {
                            (235, 175, 70)
                        };
                        (heat, heat_col, "heat")
                    };
                    let (x, bw, bh) = (24.0, 240.0, 16.0);
                    let y = H as f32 - 76.0;
                    for (row, fill, col, label) in
                        [(0.0, hp, hp_col, "hp"), (1.0, fill, col, label)]
                    {
                        let ry = y + row * (bh + 10.0);
                        frame.rect(x, ry, bw, bh, (12, 14, 12), 0.6);
                        frame.rect(x + 2.0, ry + 2.0, (bw - 4.0) * fill, bh - 4.0, col, 0.95);
                        frame.text(label, x + bw + 10.0, ry - 2.0, 18.0, (215, 220, 210));
                    }
                }
            }
            if pm.tick() % 30 == 0 {
                // Whichever predictor is live provides the speed; the
                // correction counters just add (the idle one is frozen).
                let speed = w
                    .pred
                    .get()
                    .state()
                    .map(|t| t.speed())
                    .or_else(|| w.pred_heli.get().state().map(|h| h.body.vel.len()))
                    .unwrap_or(0.0)
                    * 3.6
                    / 1.6;
                // BUILD_ID in the title: a staged/copied exe that's out
                // of date says so on sight (dirty builds carry a `+`).
                let title = format!(
                    "pm hogs [{}] — peer {}  {:.0} mph  rtt {:.0} ms  corrections {}  (h heli / t truck)",
                    pm::BUILD_ID,
                    net.peer(),
                    speed.abs(),
                    net.rtt_ms(),
                    w.pred.get().corrections + w.pred_heli.get().corrections,
                );
                let _ = window.set_title(&title);
            }
        }
    });

    // Display refresh paces the loop (WSLg ignores vsync; see solids).
    pm.loop_rate = refresh;
    pm.run().expect("connect");
}
