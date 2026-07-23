//! Hogs' 3D player client: chase-cam truck OR helicopter, the horde and
//! the bullet tracers rendered off their interp draw pools, hit/kill/
//! bite markers straight off the TTL'd synced pool, and a local aim
//! line (client-side only feedback).
//!
//! TRUCK: WASD drives; hold RIGHT mouse to swing the turret AND the
//! camera (both ease back on release); LEFT mouse (or SPACE) fires;
//! SHIFT boosts — watch the heat bar, 1.0 is an explosion.
//! HELI: W/S pitches the nose (nose down = forward), A/D yaws, SPACE
//! climbs, CTRL descends. Hold RIGHT mouse to train the CHIN GUN
//! (mouse-x slews, mouse-y elevates — camera follows the gun like the
//! truck's turret); LEFT mouse fires. Hover flat and strafe the deck,
//! or track the winged ones overhead. Ground hogs leap at a low hover;
//! climb and they lose you — but the flyers don't shed until higher.
//! H respawns as the heli, T as the truck, R as whatever you are;
//! 1/2/3 switch cameras; P toggles panini; ` (tilde) toggles the DEBUG
//! VIEW — hitbox cages (every entity's derived collision hulls, what
//! the server's sweep actually tests) plus the engine overlay: live
//! per-task timings, pool populations, and net counters off this
//! client's Pm (the eventual live console opens here); Esc quits.
//!
//! The per-vehicle KEY CONTEXTS below (same keys, different Drive
//! fields) are hand-rolled — this is the seam the input-map subsystem
//! will eventually own.

// The MENU landed 2026-07-20 (`menu` at the bottom of this file):
// HOST / JOIN / address / password before any socket exists, drawn
// through the same compute-pass text the debug overlay uses. The
// SCORE SCREEN's first cut landed 2026-07-21 with the mission arc:
// the phase splashes in the render task (brief / won / lost + ENTER
// prompts) render whatever the `Hunt` single says — every client
// shows the same screen, zero client state.
// TODO(ship): dress the end screens into a real score screen —
// per-peer contribution (kills/bites eaten) wants a tiny per-peer
// synced stat pool. Also wanted: surface a "bad password" /
// disconnect reason IN the window instead of stderr (today a bounced
// join closes back to the terminal).

use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

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
use crate::models::hull_cage_tris;

const W: u32 = 1280;
const H: u32 = 800;

/// How much of the GUN's climb the chase camera follows (0 = level,
/// 1 = welded to the barrel). Fractional on purpose: the chase eye
/// rides the anchor frame, so a fully pitched frame would drive the
/// eye into the dirt at max elevation; at 0.35 the view visibly tips
/// with the gun while the eye stays above ground. Per-player feel —
/// a const, not a param (the client-cosmetic rule).
const CAM_PITCH_FOLLOW: f32 = 0.35;

fn hog_model(h: &Hog) -> Mat4 {
    Mat4::translate(vec3(h.x, 0.0, h.z)) * Mat4::rot_y(h.heading)
}

// TODO(refactor): Corpse is a hand-rolled entity type in a Vec captured
// by the render task — make it a client-local pool + step task (the
// Tracer pattern below already shows the idiom; pm::Removes now
// provides the spawn edge).
/// A dead hog (or flyer — they FALL), tumbling: CLIENT-SIDE
/// presentation physics (GPU/CPU particle-tier — nothing on the wire).
/// A kill is an entity REMOVAL on the wire; each client turns that edge
/// into its own ragdoll from the last replicated state. Clients may
/// disagree on exactly how a corpse tumbles — cosmetic, so nobody
/// cares; that's the line between server physics (gameplay) and client
/// physics (feel).
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

    /// A flyer dies where it flew: launched along its final run from
    /// altitude, spinning harder — the fall to the bounce is the show.
    fn from_flyer(f: &Flyer, rng: &mut pm::Rng) -> Corpse {
        Corpse {
            x: f.x,
            y: f.y.max(0.45),
            z: f.z,
            vx: f.heading.sin() * f.speed * 0.6 + rng.rfr(-1.5, 1.5),
            vy: rng.rfr(-1.0, 2.5),
            vz: f.heading.cos() * f.speed * 0.6 + rng.rfr(-1.5, 1.5),
            yaw: f.heading,
            ang: 0.0,
            spin: rng.rfr(7.0, 14.0) * if rng.rf() < 0.5 { -1.0 } else { 1.0 },
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
    // Window + renderer FIRST: the menu draws before any socket exists,
    // and the game (across every reconnect) reuses both — hence the
    // Rc<RefCell<…>>: each session's task closures borrow them per
    // tick instead of consuming them, so a redial can build a fresh Pm
    // against the same window.
    let (window, pump, refresh) = pm_sdl::window(
        &format!("pm hogs [{}] — wasd drives, rmb aims, lmb fires, 1-3 cams, r respawn, esc", pm::BUILD_ID),
        W,
        H,
    );
    let window = Rc::new(RefCell::new(window));
    let pump = Rc::new(RefCell::new(pump));
    let r3d = {
        let mut r = Renderer3d::new(&window.borrow()).expect("renderer");
        r.fog_distance = 320.0;
        Rc::new(RefCell::new(r))
    };

    'app: loop {
        // The front door (TODO(ship) launch flow, first half): bare
        // launch shows HOST / JOIN; CLI modes carry their answer in the
        // flags.
        let (join_addr, password) = if flags.menu {
            match menu(&mut pump.borrow_mut(), &mut r3d.borrow_mut(), &window.borrow(), &flags) {
                Some(choice) => choice,
                None => return, // quit from the menu
            }
        } else {
            (flags.addr.clone(), flags.password.clone())
        };

        // ONE reconnect identity per game session, resent on every
        // redial: the server hands the same peer id back inside its
        // grace window and the roster returns the parked vehicle
        // (common::p.reconnect_grace, both halves). PERSISTED to
        // disk (mtime refreshed while playing), so a crash or a
        // patch-and-relaunch inside the grace window is just another
        // redial — reconnect-after-patch needs no protocol: the pm/4
        // schema hash guards compatibility, the token carries identity.
        let token = session_token_persistent(&join_addr);
        let mut first_loss: Option<Instant> = None;
        let mut attempt = 0u32;
        loop {
            let (connected, lost) = session(
                window.clone(),
                pump.clone(),
                r3d.clone(),
                refresh,
                &flags,
                &join_addr,
                &password,
                token,
            );
            if flags.replay.is_some() {
                return; // a replay ran (or ended); nothing to redial
            }
            let Some(reason) = lost else { return }; // local quit (esc)
            if connected {
                // A real session ran and then dropped: fresh clock.
                first_loss = None;
                attempt = 0;
            }
            let since = *first_loss.get_or_insert_with(Instant::now);
            attempt += 1;
            eprintln!("[hogs] connection lost: {reason} — redial {attempt}");
            if since.elapsed().as_secs_f32() > Params::default().reconnect_grace + 5.0 {
                // Past the server's grace: the peer id and the parked
                // vehicle are gone; a further dial would be a fresh
                // join. Back to the menu (or out) instead of pretending.
                eprintln!(
                    "[hogs] gave up after {:.0}s — the session moved on",
                    since.elapsed().as_secs_f32()
                );
                if flags.menu {
                    continue 'app;
                }
                return;
            }
            if !reconnect_overlay(&pump, &r3d, &window, &reason, attempt) {
                return; // esc during the wait
            }
        }
    }
}

/// The between-redials screen: reason + attempt count for ~1.2 s,
/// Esc/close gives up (returns false). Ungrabs the mouse — the grab
/// belongs to a live session.
fn reconnect_overlay(
    pump: &Rc<RefCell<sdl3::EventPump>>,
    r3d: &Rc<RefCell<Renderer3d>>,
    window: &Rc<RefCell<sdl3::video::Window>>,
    reason: &str,
    attempt: u32,
) -> bool {
    pm_sdl::grab_mouse(&window.borrow(), false);
    let until = Instant::now() + Duration::from_millis(1200);
    while Instant::now() < until {
        for ev in pump.borrow_mut().poll_iter() {
            match ev {
                Event::Quit { .. }
                | Event::KeyDown { scancode: Some(Scancode::Escape), .. } => return false,
                _ => {}
            }
        }
        let mut r3d = r3d.borrow_mut();
        let window = window.borrow();
        if let Some(mut f) = r3d.frame(&window, Mat4::IDENTITY, vec3(0.35, 1.0, 0.3)) {
            let cx = W as f32 * 0.5 - 220.0;
            let y = H as f32 * 0.42;
            f.text("CONNECTION LOST", cx, y, 34.0, (245, 120, 90));
            f.text(
                &format!("reconnecting (attempt {attempt}) — {reason}"),
                cx,
                y + 44.0,
                18.0,
                (215, 220, 210),
            );
            f.text("esc gives up", cx, y + 72.0, 15.0, (150, 155, 145));
        }
        std::thread::sleep(Duration::from_millis(16));
    }
    true
}

/// One connected life of the game: builds a `Pm`, wires every task
/// against the shared window/renderer, runs until the loop quits.
/// Returns `(handshake_completed, ClientNet::lost())` — `lost` `None`
/// means a local quit (Esc), `Some` means the link died and the caller
/// decides whether to redial with the same token.
#[allow(clippy::too_many_arguments)] // the redial loop's plumbing, all of it load-bearing
/// Where this server's session token lives between launches.
fn session_token_path(addr: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "hogs-session-{}.token",
        addr.replace([':', '/', '\\'], "_")
    ))
}

/// Load (or mint and store) the reconnect token for `addr`. A relaunch
/// while the on-disk token is younger than the reconnect grace reuses
/// it — the server hands back the same peer id and parked vehicle — so
/// crash-and-relaunch and patch-and-relaunch both reconnect. Stale or
/// unreadable files mint fresh (worst case: a normal new join).
fn session_token_persistent(addr: &str) -> [u8; 16] {
    let path = session_token_path(addr);
    if let Ok(meta) = std::fs::metadata(&path)
        && meta
            .modified()
            .ok()
            .and_then(|m| m.elapsed().ok())
            .is_some_and(|age| age.as_secs_f32() < Params::default().reconnect_grace)
        && let Ok(bytes) = std::fs::read(&path)
        && bytes.len() == 16
    {
        eprintln!("[hogs] resuming session token (reconnect window)");
        return bytes.try_into().unwrap();
    }
    let token = pm::session_token_random();
    let _ = std::fs::write(&path, token);
    token
}

fn session(
    window_rc: Rc<RefCell<sdl3::video::Window>>,
    pump_rc: Rc<RefCell<sdl3::EventPump>>,
    r3d_rc: Rc<RefCell<Renderer3d>>,
    refresh: u32,
    flags: &Flags,
    join_addr: &str,
    password: &str,
    token: [u8; 16],
) -> (bool, Option<String>) {
    let mut pm = Pm::client(join_addr, 1.0 / FIXED_DT);
    if !password.is_empty() {
        pm.password(password);
    }
    pm.session_token(token);
    if let Some(path) = &flags.replay {
        // Spectate a recording instead of dialing: the engine feeds
        // recorded frames through the normal apply path — interp, draw
        // pools, HUD all work; there's just no avatar to predict.
        pm.replay_from(path);
        eprintln!("[hogs] replaying {path}");
    }
    // Keep the on-disk session token FRESH while playing — the
    // reconnect-after-crash clock is that file's mtime (see
    // session_token_persistent).
    {
        let path = session_token_path(join_addr);
        pm.task_add("session.token", 90.0, 5.0, move |_pm| {
            let _ = std::fs::write(&path, token);
        });
    }
    if flags.link != (0.0, 0.0) {
        pm.link_lag(flags.link.0, flags.link.1);
        eprintln!(
            "[hogs] simulated link: {} ms one-way, {:.1}% loss",
            flags.link.0,
            flags.link.1 * 100.0
        );
    }
    let w = client_setup(&mut pm);
    eprintln!("connecting to {join_addr} ...");
    let net = pm.net();
    let tune = pm.single::<Tune>("hogs.tune");
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

    // Report the camera pose to the server (v2 item 4 stage 2): the
    // server's interest scorers rank the horde by distance from this
    // EYE plus a forward-cone boost, so what's actually on screen
    // streams freshest under budget pressure. Bots never report — the
    // server falls back to vehicle distance for them.
    {
        let view = cam_view.clone();
        let net = net.clone();
        pm.task_add("view.report", 8.0, 0.0, move |_pm| {
            let v = view.get();
            if v.ready() {
                net.view_set(v.eye, (v.target - v.eye).norm());
            }
        });
    }

    // Steal the mouse (relative mode): no cursor wandering onto the
    // second monitor mid-firefight. Esc quits, which releases it.
    // (After the menu — nobody wants a grabbed cursor while typing.)
    pm_sdl::grab_mouse(&window_rc.borrow(), true);
    // Mesh uploads under one scoped borrow (released before the tasks
    // run — they take their own per-tick borrows). A redial re-uploads;
    // TODO(refactor): hoist static meshes out of the session once the
    // model registry can load without a Pm.
    let mut r3d_hold = r3d_rc.borrow_mut();
    let r3d = &mut *r3d_hold;
    let ground = r3d
        .upload_mesh(&checker_ground(
            14,
            8.0,
            (0.20, 0.24, 0.18), // scrubland, not asphalt
            (0.15, 0.18, 0.13),
        ))
        .expect("ground");
    // Entity models off the registry single (client_setup loaded it —
    // assets/*.glb when present, else the code definitions in
    // models.rs). Render parts upload here; the same models' collide.*
    // protos are what the server poses into the collider pool, so the
    // file IS the shape — visual and hitbox both. Relative shading is
    // BAKED into part colors; the per-draw tint carries identity/state
    // (peer color, hp).
    let truck_m = w.models.get().upload(&r3d, "truck");
    let heli_m = w.models.get().upload(&r3d, "heli");
    let hog_m = w.models.get().upload(&r3d, "hog");
    let flyer_m = w.models.get().upload(&r3d, "flyer");
    // Hitbox debug cages (part of the tilde debug view): each
    // kind's DERIVED hulls off the same registry protos the server
    // poses into the collider pool — so the cage and the hitbox cannot
    // disagree. Baked white; the magenta rides the instance tint.
    let cage = |name: &str| {
        r3d.upload_mesh(&bake(&hull_cage_tris(w.models.get().protos(name)), (1.0, 1.0, 1.0)))
            .expect("cage")
    };
    let truck_cage = cage("truck");
    let heli_cage = cage("heli");
    let hog_cage = cage("hog");
    let flyer_cage = cage("flyer");
    // Bullet tracer: a stretched sliver, centered — bullets carry their
    // own altitude now, so the height rides the translate.
    let tracer_mesh = r3d
        .upload_mesh(&bake(
            &box_tris(vec3(-0.07, -0.1, -0.65), vec3(0.07, 0.1, 0.65)),
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
    drop(r3d_hold);

    // TODO(refactor): gun state (aim / aim_pitch / gun_cd) lives in
    // closure captures — a local "hogs.gun" single would put it where
    // the doctrine says state lives and let other tasks (HUD) read it.
    // Turret angle is CLIENT state: holding RMB accumulates mouse-x into
    // it, releasing eases it back to dead ahead. The server only ever
    // sees the absolute angle in each command frame — the animation
    // replays exactly, so prediction never corrects over it. The heli's
    // chin gun adds the second axis: mouse-y under the same hold is the
    // gun's elevation (trucks ignore it).
    // Rad per mouse count while RMB is held. Per-player feel, not shared
    // truth — a const, not a param (the client-cosmetic rule on the
    // Params declaration in common.rs).
    const AIM_SENS: f32 = 0.008;
    let mut aim = 0.0f32;
    let mut aim_pitch = 0.0f32;

    // The cosmetic gun: your bang and tracer at the CLICK, from the
    // PREDICTED muzzle — the authoritative bullet still round-trips for
    // hits and for everyone else's eyes, and `Bullet::owner` hides our
    // late twins from the draw. Cooldown mirrors the server's; a tick
    // of drift is cosmetic-only.
    let mut gun_cd = 0.0f32;

    pm.task_add("input", 4.0, 0.0, {
        let cam_mgr = cam_mgr.clone();
        let tune = tune.clone();
        let respawn = w.respawn.clone();
        let session = w.session.clone();
        let input = w.input.clone();
        let pred = w.pred.clone();
        let pred_heli = w.pred_heli.clone();
        let tracer = tracer_local.clone();
        let params = w.params.clone();
        move |pm| {
            // Per-tick borrow of the shared pump (the redial loop owns
            // it between sessions).
            let mut pump = pump_rc.borrow_mut();
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
                        xrel, yrel, mousestate, ..
                    } if mousestate.is_mouse_button_pressed(MouseButton::Right) => {
                        aim = (aim + xrel * AIM_SENS).clamp(-params.get().aim_max, params.get().aim_max);
                        // Mouse down = gun down (yrel is +down). Clamp at
                        // the CURRENT vehicle's stops: letting the
                        // accumulator run to the heli gimbal's ±1.0 in a
                        // truck (stops −0.35..0.9) pinned the barrel with
                        // dead travel on reversal — "stuck, then moves
                        // past what I commanded" (Connor, 2026-07-23).
                        let (lo, hi) = {
                            let p = params.get();
                            if flying {
                                (-p.heli_aim_pitch, p.heli_aim_pitch)
                            } else {
                                (-p.truck_aim_down, p.truck_aim_up)
                            }
                        };
                        aim_pitch = (aim_pitch - yrel * AIM_SENS).clamp(lo, hi);
                    }
                    // ENTER advances the session from an end screen —
                    // the server's director decides what "go" means by
                    // phase, so sending it any other time is a no-op.
                    Event::KeyDown {
                        scancode: Some(Scancode::Return),
                        repeat: false,
                        ..
                    } => session.send(Session { op: SESSION_GO }),
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
                        scancode: Some(Scancode::Grave),
                        repeat: false,
                        ..
                    } => {
                        let mut t = tune.get_mut();
                        t.show_debug = !t.show_debug;
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
                let k = (-9.0 * pm.loop_dt()).exp();
                aim *= k;
                aim_pitch *= k;
                if aim.abs() < 0.005 {
                    aim = 0.0;
                }
                if aim_pitch.abs() < 0.005 {
                    aim_pitch = 0.0;
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
                    aim_pitch,
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
                    aim_pitch, // turret elevation (mouse-y under RMB)
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
            let (spd, ceil) = { let p = params.get(); (p.bullet_speed, p.heli_ceil) };
            let dead: Vec<_> = {
                let mut trs = tracer.get_mut();
                let ids: Vec<_> = trs.iter().map(|(id, _)| id).collect();
                ids.into_iter()
                    .filter(|&id| {
                        trs.get_mut(id).is_some_and(|mut tr| !tracer_step(&mut tr, dt, spd, ceil))
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
                        // aim no longer drags the view around. The
                        // camera reads the POD's aim — the barrel slews
                        // at turret_rate, so the view swings with the
                        // barrel, not the mouse. Elevation tips the
                        // frame by CAM_PITCH_FOLLOW of the gun climb.
                        let dir = t.heading() + t.aim;
                        let c = t.aim_pitch * CAM_PITCH_FOLLOW;
                        Some(CamAnchor {
                            pos: t.body.pos,
                            fwd: vec3(dir.sin() * c.cos(), c.sin(), dir.cos() * c.cos()),
                        })
                    } else {
                        // Heli: follow the CHIN GUN — azimuth exactly
                        // like the truck turret, and the same fraction
                        // of the gun line's climb (gimbal against the
                        // airframe, so a dive tips the view too).
                        heli_draw.get_id(cur).map(|h| *h).map(|h| {
                            let (yaw, pitch, _) = h.body.rot.to_yaw_pitch_roll();
                            let dir = yaw + h.aim;
                            let c = (h.aim_pitch - pitch) * CAM_PITCH_FOLLOW;
                            CamAnchor {
                                pos: h.body.pos,
                                fwd: vec3(dir.sin() * c.cos(), c.sin(), dir.cos() * c.cos()),
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
        // Death-edge tracking for ragdolls: pm::Removes hands back each
        // removal WITH the last replicated state, so a corpse launches
        // from the pose the entity died in.
        let mut hog_removes: pm::Removes<Hog> = Default::default();
        let mut flyer_removes: pm::Removes<Flyer> = Default::default();
        let mut corpses: Vec<Corpse> = Vec::new();
        // Muzzle flashes / explosions as short-lived point lights:
        // births on the local tracer pool (our click, 0 ms), remote
        // bullets, and boom impacts. (pos, seconds left, radius, color).
        let mut flash_tracer = pm::Adds::default();
        let mut flash_bullet = pm::Adds::default();
        let mut flash_boom = pm::Adds::default();
        // TODO(refactor): transient facts as a tuple Vec with a manual
        // TTL countdown — the contact-points rule says this is a local
        // Flash pod pool with an expiry task.
        let mut flashes: Vec<(Vec3, f32, f32, (f32, f32, f32))> = Vec::new();
        // Smoothed frame cycle time for the title bar (EMA so the
        // readout is legible, not a jittering slot machine).
        let mut frame_ms = 0.0f32;
        // Debug-view sample state: task timings and pool counts refresh
        // once a second (a readable table, not a blur); rates are the
        // window's deltas. `dbg_on` edge-detects the toggle so the
        // since-launch stat accumulation is dropped and the first
        // window shows live numbers.
        let mut dbg_on = false;
        let mut dbg_accum = 0.0f32;
        let mut dbg_tasks: Vec<(String, f32, f32, f32)> = Vec::new(); // name, hz, avg µs, max µs
        let mut dbg_pools: Vec<(String, usize)> = Vec::new();
        let mut dbg_snaps = 0.0f32;
        let mut dbg_snap_rate = 0.0f32;
        move |pm| {
            // Per-tick borrows of the shared renderer + window (the
            // redial loop owns them between sessions).
            let mut r3d = r3d_rc.borrow_mut();
            let mut window = window_rc.borrow_mut();
            frame_ms += (pm.loop_dt() * 1000.0 - frame_ms) * 0.06;
            let show_debug = tune.get().show_debug;
            if show_debug && !dbg_on {
                pm.task_stats_reset();
                dbg_snaps = net.snapshots() as f32;
                dbg_accum = 0.0;
                dbg_tasks.clear();
            }
            dbg_on = show_debug;
            if show_debug {
                dbg_accum += pm.loop_dt();
                if dbg_accum >= 1.0 {
                    dbg_tasks = pm
                        .task_stats()
                        .into_iter()
                        .map(|(n, s)| {
                            let calls = s.calls.max(1) as f32;
                            (
                                n,
                                s.calls as f32 / dbg_accum,
                                s.ns_total as f32 / calls / 1000.0,
                                s.ns_max as f32 / 1000.0,
                            )
                        })
                        .collect();
                    pm.task_stats_reset();
                    dbg_pools = pm.pool_stats();
                    let snaps = net.snapshots() as f32;
                    dbg_snap_rate = (snaps - dbg_snaps) / dbg_accum;
                    dbg_snaps = snaps;
                    dbg_accum = 0.0;
                }
            }
            // Spawn corpses off this frame's removals, then step them on
            // the render clock (client-local physics — cosmetic only).
            {
                let mut rng = pm::Rng::new(pm.tick().wrapping_mul(0x2545_F491) | 1);
                for (_, h) in hog_removes.drain(&w.hog.get()) {
                    corpses.push(Corpse::from_hog(&h, &mut rng));
                }
                // Flyers fall: same edge, launched from altitude.
                for (_, f) in flyer_removes.drain(&w.flyer.get()) {
                    corpses.push(Corpse::from_flyer(&f, &mut rng));
                }
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
            let day_secs = w.params.get().day_secs.max(10.0);
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
            for (id, h) in w.hog_draw.get().iter() {
                // Biomod green fades to livid red as hp drops (the
                // boss's `Hog::hp` mirrors its big pool's fraction, so
                // the same ramp reads its health too).
                let hp = (h.hp / w.params.get().hog_hp).clamp(0.0, 1.0);
                let tint = (0.45 + 0.4 * (1.0 - hp), 0.42 * hp + 0.10, 0.28 * hp + 0.06);
                // Boss membership is "draw that one huge" — same mesh,
                // same batch, scaled to match its grown hitbox.
                let s = if w.boss.get_id(id).is_some() { w.params.get().boss_scale } else { 1.0 };
                let model = hog_model(h) * Mat4::scale(s);
                hog_shadows.push(Instance3::emissive(
                    Mat4::translate(vec3(h.x, 0.02, h.z)) * Mat4::scale(0.95 * s),
                    (0.035, 0.04, 0.035),
                ));
                hog_bodies.push(Instance3::new(model, tint));
                hog_snouts.push(Instance3::new(model, tint)); // snout shade is baked
            }
            // Ragdoll corpses ride the same meshes — same batches,
            // their own fading tints.
            for c in &corpses {
                let fade = (1.0 - c.t / CORPSE_LIFE).clamp(0.0, 1.0);
                let tint = (0.30 + 0.25 * fade, 0.16 * fade + 0.08, 0.10 * fade + 0.05);
                let model = c.model();
                hog_bodies.push(Instance3::new(model, tint));
                hog_snouts.push(Instance3::new(model, tint)); // snout shade is baked
            }
            // The flock: same instanced treatment — body, two wings,
            // and a shrinking shadow per flyer. The flap is a per-
            // instance rotation about the forward axis computed here
            // (the corpse-tumble trick, airborne), phased by id so the
            // wings never beat in lockstep.
            let mut flyer_shadows: Vec<Instance3> = Vec::new();
            let mut flyer_bodies: Vec<Instance3> = Vec::new();
            let mut flyer_wings_l: Vec<Instance3> = Vec::new();
            let mut flyer_wings_r: Vec<Instance3> = Vec::new();
            for (id, f) in w.flyer_draw.get().iter() {
                let hp = (f.hp / w.params.get().flyer_hp).clamp(0.0, 1.0);
                // Biomod violet draining to livid red as hp drops.
                let tint = (
                    0.38 + 0.42 * (1.0 - hp),
                    0.16 + 0.18 * hp,
                    0.20 + 0.28 * hp,
                );
                let model = Mat4::translate(vec3(f.x, f.y, f.z)) * Mat4::rot_y(f.heading);
                let phase = (id.index() % 16) as f32 * 0.42;
                let flap = (pm.tick() as f32 * 0.55 + phase).sin() * 0.55;
                flyer_bodies.push(Instance3::new(model, tint));
                // Wing shade is baked in the model.
                flyer_wings_r
                    .push(Instance3::new(model * Mat4::rot_axis(vec3(0.0, 0.0, 1.0), flap), tint));
                flyer_wings_l
                    .push(Instance3::new(model * Mat4::rot_axis(vec3(0.0, 0.0, 1.0), -flap), tint));
                let s = 0.9 * (1.0 - f.y / (w.params.get().heli_ceil * 1.7)).max(0.2);
                flyer_shadows.push(Instance3::emissive(
                    Mat4::translate(vec3(f.x, 0.02, f.z)) * Mat4::scale(s),
                    (0.035, 0.04, 0.035),
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
            // Hitbox debug cages: posed with the SAME (x,y,z,yaw) the
            // server feeds Proto::pose — yaw-only even for the banking
            // heli, y=0 for ground vehicles — off the draw pools (what
            // you see is what favor-shooter lag comp honors). Emissive
            // so night can't hide a hitbox.
            let mut cage_inst: Vec<(&pm_sdl::gpu3d::Mesh3, Vec<Instance3>)> = Vec::new();
            if show_debug {
                let mag = (1.0, 0.25, 1.0);
                let at = |x: f32, y: f32, z: f32, yaw: f32| {
                    Instance3::emissive(Mat4::translate(vec3(x, y, z)) * Mat4::rot_y(yaw), mag)
                };
                let mut hogs = Vec::new();
                for (_, h) in w.hog_draw.get().iter() {
                    hogs.push(at(h.x, 0.0, h.z, h.heading));
                }
                let mut flyers = Vec::new();
                for (_, f) in w.flyer_draw.get().iter() {
                    flyers.push(at(f.x, f.y, f.z, f.heading));
                }
                let mut trucks = Vec::new();
                for (_, t) in w.truck_draw.get().iter() {
                    trucks.push(at(t.body.pos.x, 0.0, t.body.pos.z, t.heading()));
                }
                let mut helis = Vec::new();
                for (_, hl) in w.heli_draw.get().iter() {
                    helis.push(at(
                        hl.body.pos.x,
                        hl.body.pos.y,
                        hl.body.pos.z,
                        hl.body.yaw(),
                    ));
                }
                cage_inst.push((&hog_cage, hogs));
                cage_inst.push((&flyer_cage, flyers));
                cage_inst.push((&truck_cage, trucks));
                cage_inst.push((&heli_cage, helis));
            }
            let cage_b: Vec<_> = cage_inst
                .iter()
                .map(|(mesh, inst)| (*mesh, r3d.instances(inst)))
                .collect();
            let hog_shadow_b = r3d.instances(&hog_shadows);
            let hog_body_b = r3d.instances(&hog_bodies);
            let hog_snout_b = r3d.instances(&hog_snouts);
            let flyer_shadow_b = r3d.instances(&flyer_shadows);
            let flyer_body_b = r3d.instances(&flyer_bodies);
            let flyer_wing_l_b = r3d.instances(&flyer_wings_l);
            let flyer_wing_r_b = r3d.instances(&flyer_wings_r);
            let tracer_b = r3d.instances(&tracer_inst);

            let mine = net.mine();
            // RANGEFINDER: one dot where the own gun line actually
            // stops — the first collider on the line (hot color: a
            // live target under the crosshair), else the first wall /
            // roof / floor / range end the real bullet would die on
            // (`tracer_step`'s walls, sampled). Replaces the old
            // breadcrumb ground line: with an elevating turret the
            // shot leaves the ground plane, so the feedback has to
            // live on the LINE, not under it.
            let range_dot = |(mx, my, mz, dir, climb): (f32, f32, f32, f32, f32)| {
                let reach = gun_range * climb.cos();
                let dy = gun_range * climb.sin();
                if let Some(h) = w
                    .index
                    .get()
                    .sweep(mx, mz, my, dir, reach, dy, 0.0, CAT_VEHICLE | CAT_HOG, mine)
                {
                    return (vec3(h.x, h.y, h.z), true);
                }
                let (sx, sz) = (dir.sin() * climb.cos(), dir.cos() * climb.cos());
                let sy = climb.sin();
                let step = 1.5;
                let mut p = vec3(mx, my, mz);
                let mut left = gun_range;
                while left > 0.0 {
                    let n = p + vec3(sx, sy, sz) * step;
                    if n.y <= 0.0
                        || (n.y < building_top(n.x, n.z) && in_building(n.x, n.z, 0.0))
                        || n.x.abs() > ARENA
                        || n.z.abs() > ARENA
                        || n.y > w.params.get().heli_ceil
                    {
                        break;
                    }
                    p = n;
                    left -= step;
                }
                (p, false)
            };

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

                // Mission furniture, straight off the replicas. The
                // depot (DEFEND): a tank that bruises red as it's
                // chewed, with a warning strobe once it's half gone.
                let sb = w.hunt.get();
                for (_, d) in w.depot.get().iter() {
                    let f = (d.hp / w.params.get().depot_hp).clamp(0.0, 1.0);
                    frame.draw(
                        &cube,
                        Mat4::translate(vec3(d.x, 0.0, d.z))
                            * Mat4::scale_xyz(2.0 * DEPOT_R * 0.8, DEPOT_H, 2.0 * DEPOT_R * 0.8),
                        (0.30 + 0.45 * (1.0 - f), 0.42 * f + 0.10, 0.18 * f + 0.06),
                        true,
                    );
                    let blink = f < 0.5 && (pm.tick() / 20) % 2 == 0;
                    frame.draw_emissive(
                        &marker,
                        Mat4::translate(vec3(d.x, DEPOT_H, d.z)) * Mat4::scale(2.0),
                        if blink { (1.4, 0.25, 0.1) } else { (0.2, 0.9, 0.4) },
                        true,
                    );
                }
                // The race loop (RACE, while playing): the live beacon
                // is a pulsing pillar of light, the next one a dim
                // promise so you can set up the corner.
                if sb.phase == PHASE_PLAYING && sb.kind == MISSION_RACE {
                    let pulse = 1.0 + 0.25 * (pm.tick() as f32 * 0.15).sin();
                    let n = RACE_LOOP.len();
                    let (cx, cz) = RACE_LOOP[sb.done as usize % n];
                    frame.draw_emissive(
                        &cube,
                        Mat4::translate(vec3(cx, 0.0, cz))
                            * Mat4::scale_xyz(1.5 * pulse, 26.0, 1.5 * pulse),
                        (0.25, 1.1, 1.3),
                        true,
                    );
                    let (nx, nz) = RACE_LOOP[(sb.done as usize + 1) % n];
                    frame.draw_emissive(
                        &cube,
                        Mat4::translate(vec3(nx, 0.0, nz)) * Mat4::scale_xyz(0.9, 14.0, 0.9),
                        (0.08, 0.22, 0.26),
                        true,
                    );
                }

                // Trucks: smoothed view (predicted self, interp'd others).
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
                    // Shading (cabin/barrel darker) is baked in the
                    // model's vertex colors; the tint is pure peer.
                    frame.draw(truck_m.mesh("body"), model, tint, true);
                    frame.draw(truck_m.mesh("cabin"), model, tint, true);
                    // Turret: rotated by heading + aim, elevated about
                    // the barrel-root pivot (0, 1.45, 0 in truck space
                    // — where truck_muzzle says the barrel hinges) —
                    // replicated state, so every peer's swing AND
                    // elevation are visible.
                    let dir = t.heading() + t.aim;
                    frame.draw(
                        truck_m.mesh("barrel"),
                        Mat4::translate(t.body.pos)
                            * Mat4::rot_y(dir)
                            * Mat4::translate(vec3(0.0, 1.45, 0.0))
                            * Mat4::rot_x(-t.aim_pitch)
                            * Mat4::translate(vec3(0.0, -1.45, 0.0)),
                        tint,
                        true,
                    );
                    // Own truck: the rangefinder dot on the gun line —
                    // client-side feedback only, nothing on the wire.
                    if mine == Some(id) {
                        let (p, hot) = range_dot(truck_muzzle(&t));
                        frame.draw_emissive(
                            &marker,
                            Mat4::translate(p) * Mat4::scale(if hot { 1.3 } else { 0.9 }),
                            if hot { (1.4, 0.35, 0.2) } else { (0.8, 0.75, 0.45) },
                            true,
                        );
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
                    let s = 1.9 * (1.0 - hl.body.pos.y / (w.params.get().heli_ceil * 1.7)).max(0.2);
                    frame.draw_emissive(
                        &shadow,
                        Mat4::translate(vec3(hl.body.pos.x, 0.03, hl.body.pos.z))
                            * Mat4::scale(s),
                        (0.035, 0.04, 0.035),
                        false,
                    );
                    // Tail shade is baked; skid and rotor bake their
                    // ABSOLUTE colors and draw untinted.
                    frame.draw(heli_m.mesh("body"), model, tint, true);
                    frame.draw(heli_m.mesh("tail"), model, tint, true);
                    frame.draw(heli_m.mesh("skid"), model, (1.0, 1.0, 1.0), true);
                    frame.draw(heli_m.mesh("rotor"), model * spin, (1.0, 1.0, 1.0), true);
                    // Chin gun at the gimbal's pose — aim is replicated
                    // Heli state, so every peer sees it train. Pivoted
                    // at the chin point, not the body origin, so it
                    // slews in place like a turret.
                    let (yaw, pitch, _) = hl.body.rot.to_yaw_pitch_roll();
                    let dir = yaw + hl.aim;
                    let climb = hl.aim_pitch - pitch;
                    frame.draw(
                        heli_m.mesh("gun"),
                        Mat4::translate(hl.body.pos + vec3(0.0, -0.35, 0.0))
                            * Mat4::rot_y(dir)
                            * Mat4::rot_x(-climb),
                        tint,
                        true,
                    );
                    // Own heli: the same rangefinder dot the truck
                    // gets — where the GIMBAL's line actually stops,
                    // dive or no dive, climb or no climb.
                    if mine == Some(id) {
                        let (p, hot) = range_dot(heli_muzzle(&hl));
                        frame.draw_emissive(
                            &marker,
                            Mat4::translate(p) * Mat4::scale(if hot { 1.3 } else { 0.9 }),
                            if hot { (1.4, 0.35, 0.2) } else { (0.8, 0.75, 0.45) },
                            true,
                        );
                    }
                }

                // Tracers, corpses, the horde, and the flock: the
                // instance batches staged above frame() — a handful of
                // draw calls for the whole crowd (tint/emissive ride
                // per instance).
                frame.draw_instanced(&tracer_mesh, tracer_b, true);
                frame.draw_instanced(&shadow, hog_shadow_b, false);
                frame.draw_instanced(hog_m.mesh("body"), hog_body_b, true);
                frame.draw_instanced(hog_m.mesh("snout"), hog_snout_b, true);
                frame.draw_instanced(&shadow, flyer_shadow_b, false);
                frame.draw_instanced(flyer_m.mesh("body"), flyer_body_b, true);
                frame.draw_instanced(flyer_m.mesh("wing.l"), flyer_wing_l_b, true);
                frame.draw_instanced(flyer_m.mesh("wing.r"), flyer_wing_r_b, true);
                // The hitbox cages ride last so they overlay the art
                // (thin double-sided ribbons — no cull).
                for (mesh, batch) in cage_b {
                    frame.draw_instanced(mesh, batch, false);
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
                    // Emissive: a flash is light, not a lit thing.
                    frame.draw_emissive(
                        &marker,
                        Mat4::translate(vec3(c.x, 0.0, c.z)) * Mat4::scale(scale),
                        col,
                        true,
                    );
                }

                // HUD: team score big, the current OBJECTIVE under it —
                // all read RAW off the synced single (server-owned,
                // never predicted; `sb` copied above the furniture).
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
                let info = match (sb.phase, sb.kind) {
                    (PHASE_PLAYING, MISSION_DEFEND) => {
                        let dp = w
                            .depot
                            .get()
                            .iter()
                            .next()
                            .map_or(0, |(_, d)| (d.hp / w.params.get().depot_hp * 100.0).ceil() as i32);
                        format!(
                            "wave {}/{}   depot {}%   hogs {}",
                            sb.wave, sb.goal, dp, sb.alive
                        )
                    }
                    (PHASE_PLAYING, MISSION_RACE) => format!(
                        "beacon {}/{}   {:.0}s   hogs {}",
                        sb.done, sb.goal, sb.timer.max(0.0), sb.alive
                    ),
                    (PHASE_PLAYING, MISSION_BOSS) => {
                        format!("boss {}%   hogs {}", sb.done, sb.alive)
                    }
                    (PHASE_PLAYING, _) => {
                        format!("wave {}/{}   hogs {}", sb.wave, sb.goal, sb.alive)
                    }
                    _ => String::new(),
                };
                if !info.is_empty() {
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

                // Phase splashes: everyone renders the same screen off
                // the same single — the director's whole contract.
                let center = |frame: &mut Frame3, s: &str, y: f32, px: f32, col| {
                    let tw = frame.text_width(s, px);
                    hud_bold(frame, s, (W as f32 - tw) / 2.0, y, px, col);
                };
                let cy = H as f32 * 0.30;
                match sb.phase {
                    PHASE_LOBBY => {
                        center(&mut frame, "waiting for the team ...", cy, 26.0, (210, 215, 205));
                    }
                    PHASE_BRIEF => {
                        let ld = level_def(sb.level);
                        let md = mission_def(sb.level, sb.mission);
                        center(
                            &mut frame,
                            &format!("{} — mission {}", ld.name, sb.mission + 1),
                            cy - 34.0,
                            22.0,
                            (170, 180, 170),
                        );
                        center(&mut frame, md.name, cy, 44.0, (245, 235, 190));
                        center(&mut frame, md.brief, cy + 54.0, 20.0, (210, 215, 205));
                        center(
                            &mut frame,
                            &format!("{:.0}", sb.timer.max(0.0).ceil()),
                            cy + 96.0,
                            34.0,
                            (245, 245, 245),
                        );
                    }
                    PHASE_WON => {
                        center(&mut frame, "LEVEL COMPLETE", cy, 48.0, (150, 240, 150));
                        center(
                            &mut frame,
                            &format!("{} points", sb.points as i32),
                            cy + 58.0,
                            26.0,
                            (235, 235, 225),
                        );
                        center(&mut frame, "ENTER - next level", cy + 96.0, 20.0, (200, 210, 190));
                    }
                    PHASE_LOST => {
                        let md = mission_def(sb.level, sb.mission);
                        center(&mut frame, "MISSION FAILED", cy, 48.0, (245, 110, 90));
                        center(&mut frame, md.name, cy + 58.0, 24.0, (225, 205, 190));
                        center(&mut frame, "ENTER - retry", cy + 96.0, 20.0, (200, 210, 190));
                    }
                    _ => {}
                }

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
                        .map_or(0.0, |v| (v.hp / w.params.get().truck_hp).clamp(0.0, 1.0));
                    let hp_col = (
                        (60.0 + 195.0 * (1.0 - hp)) as u8,
                        (60.0 + 175.0 * hp) as u8,
                        60,
                    );
                    // Second bar is per-vehicle: boost heat in a truck,
                    // altitude in the heli (both predicted state — live,
                    // no round trip).
                    let (fill, col, label) = if let Some(hl) = w.pred_heli.get().state() {
                        let alt = (hl.body.pos.y / w.params.get().heli_ceil).clamp(0.0, 1.0);
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

                // --- the DEBUG OVERLAY (tilde): this client's engine,
                // live — per-task timings and pool populations off the
                // once-a-second sample above, net counters fresh. Two
                // panels; the columns are fixed x offsets (the gpu3d
                // font isn't monospaced, so padding can't align them).
                // Server tasks live in the other thread's Pm — they get
                // their window when the live console lands.
                // TODO(roadmap): the live console (queued 2026-07-18,
                // per Connor) — typed COMMANDS at this same key:
                // inspect and poke pools/singles/params from inside the
                // game, and a window into the SERVER's Pm (its tasks
                // live on the other thread; likely rides the
                // telemetry/params seam, which already crosses that gap
                // both ways).
                if show_debug {
                    let (lh, px) = (17.0, 15.0);
                    let hdr = (250, 205, 120);
                    let txt = (208, 218, 208);
                    let dim = (150, 160, 150);
                    let x0 = 20.0;
                    let faults = pm.task_faults().len();
                    let rows1 = 3.6 + dbg_tasks.len().max(1) as f32 + (faults > 0) as u8 as f32;
                    frame.rect(x0 - 8.0, 12.0, 380.0, rows1 * lh + 14.0, (6, 8, 6), 0.62);
                    let mut y = 18.0;
                    frame.text(
                        &format!(
                            "debug   tick {}   frame {:.1} ms   rtt {:.0} ms",
                            pm.tick(),
                            frame_ms,
                            net.rtt_ms()
                        ),
                        x0,
                        y,
                        px,
                        hdr,
                    );
                    y += lh;
                    frame.text(
                        &format!(
                            "peer {}   snaps {:.0}/s   corrections {}",
                            net.peer(),
                            dbg_snap_rate,
                            w.pred.get().corrections + w.pred_heli.get().corrections
                        ),
                        x0,
                        y,
                        px,
                        txt,
                    );
                    y += lh;
                    if faults > 0 {
                        frame.text(
                            &format!("task faults: {faults} (stderr has the story)"),
                            x0,
                            y,
                            px,
                            (255, 90, 70),
                        );
                        y += lh;
                    }
                    y += lh * 0.6;
                    for (c, s) in [(0.0, "task"), (185.0, "hz"), (245.0, "avg us"), (315.0, "max us")] {
                        frame.text(s, x0 + c, y, px, dim);
                    }
                    y += lh;
                    if dbg_tasks.is_empty() {
                        frame.text("sampling ...", x0, y, px, dim);
                    }
                    for (name, hz, avg, max) in &dbg_tasks {
                        frame.text(&name[..name.len().min(20)], x0, y, px, txt);
                        frame.text(&format!("{hz:.0}"), x0 + 185.0, y, px, txt);
                        frame.text(&format!("{avg:.0}"), x0 + 245.0, y, px, txt);
                        frame.text(&format!("{max:.0}"), x0 + 315.0, y, px, txt);
                        y += lh;
                    }

                    // Pools panel: every pool (singles included — a
                    // single is a one-entity pool), largest first,
                    // capped so a long store can't run off the screen.
                    const POOL_ROWS: usize = 24;
                    let x1 = 412.0;
                    let shown = dbg_pools.len().min(POOL_ROWS);
                    let rows2 = 1.0 + shown as f32 + (dbg_pools.len() > POOL_ROWS) as u8 as f32;
                    frame.rect(x1 - 8.0, 12.0, 310.0, rows2 * lh + 14.0, (6, 8, 6), 0.62);
                    let mut y = 18.0;
                    frame.text("pool", x1, y, px, dim);
                    frame.text("entities", x1 + 220.0, y, px, dim);
                    y += lh;
                    for (name, n) in dbg_pools.iter().take(POOL_ROWS) {
                        frame.text(&name[..name.len().min(26)], x1, y, px, txt);
                        frame.text(&format!("{n}"), x1 + 220.0, y, px, txt);
                        y += lh;
                    }
                    if dbg_pools.len() > POOL_ROWS {
                        frame.text(
                            &format!("+ {} more", dbg_pools.len() - POOL_ROWS),
                            x1,
                            y,
                            px,
                            dim,
                        );
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
                    "pm hogs [{}] — peer {}  {:.0} mph  rtt {:.0} ms  frame {:.1} ms  corrections {}  (h heli / t truck)",
                    pm::BUILD_ID,
                    net.peer(),
                    speed.abs(),
                    net.rtt_ms(),
                    frame_ms,
                    w.pred.get().corrections + w.pred_heli.get().corrections,
                );
                let _ = window.set_title(&title);
            }
        }
    });

    // Display refresh paces the loop (WSLg ignores vsync; see solids).
    pm.loop_rate = refresh;
    let ran = pm.run();
    // The Pm is gone; the cloned handles keep the status single alive —
    // this is how the redial loop learns WHY the loop ended.
    let connected = net.peer() != 0;
    match ran {
        Ok(()) => (connected, net.lost()),
        Err(e) => (connected, Some(format!("connect failed: {e}"))),
    }
}

/// The pre-connect menu: HOST / JOIN / address / password, keyboard
/// only (Up/Down rows, type into the text rows, Enter on a verb goes,
/// Esc quits). Returns `(connect_addr, password)`, or `None` to quit.
///
/// HOST spawns the server (+2 bots) in-process, exactly like the old
/// no-arg launch — bound to `0.0.0.0` so friends can dial your IP —
/// then joins it over loopback. The password locks the door for
/// everyone, bots included (they present it too).
fn menu(
    pump: &mut sdl3::EventPump,
    r3d: &mut Renderer3d,
    window: &sdl3::video::Window,
    flags: &Flags,
) -> Option<(String, String)> {
    const ROWS: usize = 4; // HOST, JOIN, address, password
    let mut sel = 0usize;
    let mut addr = flags.addr.clone();
    let mut password = flags.password.clone();
    // SDL text input gives us real typed characters (layout/IME aware)
    // as TextInput events — scancode chords could never spell ':'.
    let text_input = window.subsystem().text_input();
    text_input.start(window);

    let choice = 'menu: loop {
        for ev in pump.poll_iter() {
            match ev {
                Event::Quit { .. }
                | Event::KeyDown { scancode: Some(Scancode::Escape), .. } => {
                    break 'menu None;
                }
                Event::KeyDown { scancode: Some(Scancode::Up), .. } => {
                    sel = (sel + ROWS - 1) % ROWS;
                }
                Event::KeyDown { scancode: Some(Scancode::Down), .. }
                | Event::KeyDown { scancode: Some(Scancode::Tab), .. } => {
                    sel = (sel + 1) % ROWS;
                }
                Event::KeyDown { scancode: Some(Scancode::Backspace), .. } => {
                    match sel {
                        2 => {
                            addr.pop();
                        }
                        3 => {
                            password.pop();
                        }
                        _ => {}
                    }
                }
                Event::KeyDown { scancode: Some(Scancode::Return), .. } => match sel {
                    0 => break 'menu Some((true, addr.clone(), password.clone())),
                    1 => break 'menu Some((false, addr.clone(), password.clone())),
                    _ => sel = (sel + 1) % ROWS, // Enter on a field: next
                },
                Event::TextInput { text, .. } => match sel {
                    2 => addr.push_str(text.trim()),
                    3 => password.push_str(text.trim()),
                    _ => {}
                },
                _ => {}
            }
        }

        if let Some(mut f) = r3d.frame(window, Mat4::IDENTITY, vec3(0.35, 1.0, 0.3)) {
            let cx = W as f32 * 0.5 - 220.0;
            let mut y = H as f32 * 0.30;
            let dim = (150, 155, 145);
            let lit = (250, 220, 120);
            let txt = (215, 220, 210);
            f.text("PM HOGS", cx, y - 90.0, 46.0, (245, 200, 90));
            f.text("co-op hog hunting", cx, y - 44.0, 18.0, dim);
            let row = |f: &mut Frame3, i: usize, sel: usize, label: &str, y: f32| {
                let on = i == sel;
                f.text(if on { ">" } else { " " }, cx - 26.0, y, 22.0, lit);
                f.text(label, cx, y, 22.0, if on { lit } else { txt });
            };
            row(&mut f, 0, sel, "HOST GAME   (friends join your ip)", y);
            y += 34.0;
            row(&mut f, 1, sel, "JOIN GAME", y);
            y += 44.0;
            row(&mut f, 2, sel, &format!("address:  {addr}{}", if sel == 2 { "_" } else { "" }), y);
            y += 30.0;
            let mask = "*".repeat(password.chars().count());
            row(&mut f, 3, sel, &format!("password: {mask}{}", if sel == 3 { "_" } else { "" }), y);
            y += 44.0;
            f.text("up/down rows · type in fields · enter go · esc quit", cx, y, 15.0, dim);
            f.text(
                "host locks the game with the password; leave it empty for an open door",
                cx,
                y + 20.0,
                15.0,
                dim,
            );
        }
        std::thread::sleep(std::time::Duration::from_millis(16));
    };
    text_input.stop(window);

    let (host, addr, password) = choice?;
    if !host {
        return Some((addr, password));
    }
    // HOST: the old no-arg launch, now behind a menu verb — dedicated
    // thread server bound for the outside world, two bot teammates.
    // (The server loads the params file itself — engine seam.)
    let path = flags.params_path.clone();
    let pw = (!password.is_empty()).then(|| password.clone());
    std::thread::spawn(move || crate::server::run(true, path, HOST_BIND, pw, None, false));
    std::thread::sleep(std::time::Duration::from_millis(300));
    for n in 0..2 {
        let (link, pw) = (flags.link, password.clone());
        std::thread::spawn(move || crate::bot_client::run_bot(n, link, ADDR, &pw, false));
    }
    Some((ADDR.to_string(), password))
}
