//! Hogs client gameplay, shared by the SDL player and headless bots:
//! full-schema registration (every client registers every channel — the
//! handshake demands it), local-truck prediction, and snapshot interp
//! for BOTH remote pools (trucks and the horde). The horde interp is the
//! interesting one: at 300 hogs the byte budget rotates and the interp
//! delay is what rides the multi-tick gaps between a hog's updates.

use pm::{EventTx, Id, InputTx, Pm, PmClient, PoolHandle, Predictor, SingleHandle, SingleRx};

use crate::common::*;

/// Everything a hogs client holds after setup. Fields the headless bot
/// doesn't read (draw pools, scoreboard) still exist — registration is
/// the schema contract; reading is optional.
pub struct ClientWorld {
    /// Authoritative replicas. Health is server-owned truth read RAW
    /// (never predicted, never interp'd — a HUD wants the latest word).
    /// Bullet/impact raw replicas also feed the sfx `Births` trackers —
    /// edges come off the earliest copy, not the delayed draw pools.
    pub hog: PoolHandle<Hog>,
    pub health: PoolHandle<Health>,
    pub bullet: PoolHandle<Bullet>,
    pub impact: PoolHandle<Impact>,
    /// Smoothed views rendering should read (predicted own vehicle wins
    /// on its draw pool; hogs and bullets are pure interp).
    pub truck_draw: PoolHandle<Truck>,
    pub heli_draw: PoolHandle<Heli>,
    pub hog_draw: PoolHandle<Hog>,
    pub bullet_draw: PoolHandle<Bullet>,
    /// The co-op scoreboard (server-owned synced single).
    pub hunt: SingleRx<Hunt>,
    pub input: InputTx<Drive>,
    pub respawn: EventTx<Respawn>,
    /// Reliable param writes (telemetry knobs → server clamp).
    pub param_set: EventTx<ParamSet>,
    /// The server's tuning set, replicated (docs/params.md stage 2):
    /// the predictors' steps read it (shared-step constants), and any
    /// client read of a tunable — bot gates, cosmetic gun cadence, aim
    /// line reach — comes off this replica, never a const.
    pub params: SingleRx<Params>,
    /// Both vehicle predictors ride the same input channel; whichever
    /// pool holds the avatar is live, the other idles with `state() ==
    /// None` — which is also how game code asks "am I flying?".
    pub pred: SingleHandle<Predictor<Truck, Drive>>,
    pub pred_heli: SingleHandle<Predictor<Heli, Drive>>,
}

impl ClientWorld {
    /// Every vehicle's collision [`Hull`] with its id, off the smoothed
    /// draw pools — the client-side mirror of the server's friendly-fire
    /// registry (see server.rs `hulls`). The bots' hold-fire gate walks
    /// this; a NEW VEHICLE is one `extend` line here.
    pub fn hulls(&self) -> Vec<(Id, Hull)> {
        let mut v: Vec<(Id, Hull)> = Vec::new();
        v.extend(self.truck_draw.get().iter().map(|(id, t)| (id, truck_hull(t))));
        v.extend(self.heli_draw.get().iter().map(|(id, h)| (id, heli_hull(h))));
        v
    }
}

/// Register the full channel set and install prediction + interpolation.
/// No connect here — `run` does that once the schema is complete.
pub fn client_setup(pm: &mut PmClient) -> ClientWorld {
    let truck = pm.sync_pool::<Truck>("truck");
    let heli = pm.sync_pool::<Heli>("heli");
    let health = pm.sync_pool::<Health>("truck.health");
    let hog = pm.wire_pool::<Hog>("hog");
    let bullet = pm.wire_pool::<Bullet>("bullet");
    let impact = pm.wire_pool::<Impact>("impact");
    let hunt = pm.sync_single::<Hunt>("hunt");
    // The server's tuning set (docs/params.md), stage 2 live: the
    // predictor step closures below capture this replica, so the same
    // numbers drive both ends — a live change mispredicts for one
    // snapshot interval and reconciles (the documented blip). The write
    // path is `param_set` below.
    let params = pm.sync_single::<Params>("params");
    let input = pm.input::<Drive>("drive");
    let respawn = pm.event::<Respawn>("respawn");
    let param_set = pm.event::<ParamSet>("param.set");

    // Local vehicle: reconcile against the input-seq echo, replay
    // unacked sends — the same steps the server runs make it byte-exact.
    // BOTH vehicles get predictors on the ONE input channel; the one
    // whose pool holds the avatar runs, the other idles empty.
    let pred = pm.predict_pool(
        &truck,
        &input,
        {
            let params = params.clone();
            move |s, c, dt| truck_step(s, c, dt, &params.get())
        },
        err_metric,
        1e-4,
        FIXED_DT,
    );
    let pred_heli = pm.predict_pool(
        &heli,
        &input,
        {
            let params = params.clone();
            move |s, c, dt| heli_step(s, c, dt, &params.get())
        },
        heli_err,
        1e-4,
        FIXED_DT,
    );

    // Remote smoothing, shared delay contract with the server's shot
    // rewind (see common.rs INTERP_DELAY). Capped extrapolation rides
    // loss bursts — and, for the horde, budget-rotation gaps.
    let extrap = std::env::var("PM_EXTRAP_MS")
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
        .map_or(0.05, |ms| ms / 1000.0);
    let truck_draw = pm.interp_pool(&truck, truck_lerp, interp_delay() as f64, extrap);
    let heli_draw = pm.interp_pool(&heli, heli_lerp, interp_delay() as f64, extrap);
    let hog_draw = pm.interp_pool(&hog, hog_lerp, interp_delay() as f64, extrap);
    // Bullets too: they live ~0.6 s and cross the map in a straight
    // line — interp (plus the extrapolation cap) is plenty.
    let bullet_draw = pm.interp_pool(&bullet, bullet_lerp, interp_delay() as f64, extrap);

    ClientWorld {
        params,
        hog,
        health,
        bullet,
        impact,
        truck_draw,
        heli_draw,
        hog_draw,
        bullet_draw,
        hunt,
        input,
        respawn,
        param_set,
        pred,
        pred_heli,
    }
}

/// Headless bot: hunt the nearest hog — drive at it, shoot when lined
/// up, wander when the wave is dead.
pub fn run_bot(n: u32, link: (f32, f32)) {
    let mut pm = Pm::client(ADDR, 1.0 / FIXED_DT);
    if link != (0.0, 0.0) {
        pm.link_lag(link.0, link.1);
    }
    let w = client_setup(&mut pm);
    let net = pm.net();
    // Predictor handles for the prof task below (`w` moves into the bot
    // task first).
    let w2_pred = w.pred.clone();
    let w2_pred_heli = w.pred_heli.clone();

    // Building-jam recovery state: seconds spent commanding thrust while
    // not moving, and seconds of back-out left once we give up.
    let mut jam = 0.0f32;
    let mut back = 0.0f32;
    // Which way the back-out drives: opposite whatever was jammed.
    let mut back_dir = -1.0f32;
    // Pilot bots: one-shot "give me the heli" request state.
    let mut asked_heli = false;

    pm.task_add("bot", 4.0, 0.0, move |pm| {
        let t = pm.tick() as f32 / 60.0 + n as f32 * 1.7;
        let p = w.params.get();

        // Trigger discipline — friendly fire is live: hold when a
        // teammate's (grown) hull crosses the line of fire. Same hulls
        // and sweep the server judges with, generous margin: a held
        // shot costs a quarter second, a connected one costs a quarter
        // of a buddy's hp.
        let line_clear = |x: f32, y: f32, z: f32, bearing: f32, pitch: f32, reach: f32| {
            let mine = net.mine();
            let dy = pitch.tan() * reach;
            !w.hulls().iter().any(|(id, h)| {
                mine != Some(*id)
                    && ray_hits_hull(x, z, y, bearing, reach, dy, &h.grow(0.5)).is_some()
            })
        };

        // Bots n >= 2 are PILOTS: swap to the heli once spawned, then
        // fly strafing runs — deliberately exercising the whole 3D path
        // headlessly (vehicle swap, heli prediction, diving bullets,
        // low hover inside hog-leap range on sloppy pull-ups).
        if n >= 2 {
            if let Some(hl) = w.pred_heli.get().state() {
                let b = hl.body;
                let (yaw, _, _) = b.rot.to_yaw_pitch_roll();
                let target = w
                    .hog
                    .get()
                    .values()
                    .iter()
                    .map(|h| {
                        let (dx, dz) = (h.x - b.pos.x, h.z - b.pos.z);
                        (*h, (dx * dx + dz * dz).sqrt())
                    })
                    .min_by(|a, b| a.1.total_cmp(&b.1));
                let (turn, pitch, fire) = match target {
                    Some((h, d)) => {
                        let bearing = (h.x - b.pos.x).atan2(h.z - b.pos.z);
                        let err = wrap_angle(bearing - yaw);
                        // Nose over far enough that the gun line meets
                        // the ground at the hog.
                        let dive = (b.pos.y / d.max(3.0)).atan();
                        let aligned = err.abs() < 0.25
                            && d < p.gun_range * 0.9
                            && line_clear(b.pos.x, b.pos.y, b.pos.z, bearing, -dive, d);
                        (
                            err.clamp(-1.0, 1.0),
                            (dive / HELI_PITCH_MAX).clamp(-1.0, 1.0),
                            aligned as i32 as f32,
                        )
                    }
                    None => ((t * 0.3).sin() * 0.5, 0.15, 0.0),
                };
                // Hold a working altitude; drift low on purpose now and
                // then so leaping hogs get their shot at us.
                let lift = if b.pos.y < 6.0 + (t * 0.11).sin() * 4.0 {
                    1.0
                } else if b.pos.y > 14.0 {
                    -0.5
                } else {
                    0.05
                };
                w.input.set(Drive {
                    thrust: 0.0,
                    turn,
                    fire,
                    aim: 0.0,
                    boost: 0.0,
                    bot: 1.0,
                    pitch,
                    lift,
                });
                return;
            }
            if !asked_heli && w.pred.get().state().is_some() {
                w.respawn.send(Respawn { vehicle: VEH_HELI });
                asked_heli = true;
            }
            // Fall through and drive the truck until the swap lands.
        }

        let Some(me) = w.pred.get().state() else {
            return;
        };
        // Nearest hog, from the authoritative replica (bots don't render,
        // so no reason to read the draw pool).
        let target = w
            .hog
            .get()
            .values()
            .iter()
            .map(|h| {
                let (dx, dz) = (h.x - me.body.pos.x, h.z - me.body.pos.z);
                (*h, (dx * dx + dz * dz).sqrt())
            })
            .min_by(|a, b| a.1.total_cmp(&b.1));

        let (mut turn, mut thrust, mut fire) = match target {
            Some((h, d)) => {
                // Lead the shot: aim where the hog will be when the
                // bullet arrives, not where it is — the pod carries its
                // velocity. (Humans lead by watching tracers; a bot that
                // insta-aims at the current bearing only ever hits hogs
                // charging straight down the ray.)
                let tof = d / p.bullet_speed;
                let (lx, lz) = (
                    h.x + h.heading.sin() * h.speed * tof,
                    h.z + h.heading.cos() * h.speed * tof,
                );
                let bearing = (lx - me.body.pos.x).atan2(lz - me.body.pos.z);
                let err = wrap_angle(bearing - me.heading());
                // Only pull the trigger when the hog's body actually
                // subtends the aim error (with slack) — a fixed gate
                // either never hits at range or wastes every shot.
                let aligned = err.abs() < (HOG_R / d.max(2.0)).atan() * 2.0;
                // GRIP-PHYSICS throttle: momentum carries now, so the
                // old two-speed throttle glided into every close hog
                // and ate the bite. Chase a TARGET SPEED instead —
                // proportional standoff close in (backing up point
                // blank), full chase far out — and slow to turn: a big
                // bearing error caps the target so the chassis rotates
                // ahead of the momentum instead of orbiting the hog.
                // Standoff at 12u: outside a bite lunge, inside easy
                // gun range — and full reverse when a charge closes
                // (soak data: parking at spit distance = death by bite).
                let mut want = ((d - 12.0) * 0.9).clamp(-7.0, p.vmax);
                if err.abs() > 0.9 {
                    want = want.min(7.0);
                }
                (
                    err.clamp(-1.0, 1.0),
                    ((want - me.speed()) * 0.4).clamp(-1.0, 1.0),
                    (aligned
                        && d < p.gun_range * 0.95
                        && line_clear(me.body.pos.x, 1.45, me.body.pos.z, bearing, 0.0, d))
                        as i32 as f32,
                )
            }
            // Wave's dead: lazy sine wander until the next one lands.
            None => ((t * 0.43).sin() * 0.8, 0.6, 0.0),
        };
        // Wall recovery beats everything (drive's lesson: a pure chaser
        // grinds a wall forever).
        let r = (me.body.pos.x * me.body.pos.x + me.body.pos.z * me.body.pos.z).sqrt();
        if r > ARENA - 10.0 {
            let home = (-me.body.pos.x).atan2(-me.body.pos.z);
            turn = wrap_angle(home - me.heading()).clamp(-1.0, 1.0);
            thrust = 0.7;
            fire = 0.0;
        }
        // Building jam beats even that: commanding thrust while going
        // nowhere means a wall is eating it — back out turning, then
        // resume the chase from the new angle.
        if back > 0.0 {
            back -= FIXED_DT;
            thrust = 0.7 * back_dir;
            turn = 1.0;
            fire = 0.0;
        } else {
            // Standoff drives REVERSE now too — a wall can eat either
            // direction, so jam on commanded thrust of any sign and
            // back out the opposite way.
            if thrust.abs() > 0.2 && me.speed().abs() < 1.0 {
                jam += FIXED_DT;
            } else {
                jam = 0.0;
            }
            if jam > 0.8 {
                jam = 0.0;
                back = 1.2;
                back_dir = -thrust.signum();
            }
        }
        w.input.set(Drive {
            thrust,
            turn,
            fire,
            aim: 0.0,   // bots shoot over the hood
            boost: 0.0, // and drive responsibly
            bot: 1.0,   // AI: steering lags
            pitch: 0.0, // bots keep their wheels on the ground
            lift: 0.0,
        });
    });

    // PM_PROF=1: where a CLIENT's tick goes (prediction, the two interp
    // pools riding the horde) — bot 0 only, so the dumps don't
    // interleave. The corrections line is the PREDICTION health gauge:
    // it should stay flat while driving; a live shared-step param write
    // (docs/params.md stage 2) may step it once, never stream.
    if n == 0 && std::env::var("PM_PROF").is_ok() {
        let mut prev: std::collections::HashMap<String, pm::TaskStat> = Default::default();
        let pred = w2_pred.clone();
        let pred_heli = w2_pred_heli.clone();
        pm::task!(pm, "prof", 91.0, 5.0, [], move |pm| {
            eprintln!(
                "-- bot0 task stats (last 5s) --  corrections={}",
                pred.get().corrections + pred_heli.get().corrections
            );
            let mut tick_total = 0.0f32;
            for (name, s) in pm.task_stats() {
                let p = prev.get(&name).cloned().unwrap_or_default();
                let calls = s.calls - p.calls;
                if calls > 0 {
                    let avg_us = (s.ns_total - p.ns_total) as f32 / calls as f32 / 1000.0;
                    tick_total += (s.ns_total - p.ns_total) as f32 / 300.0 / 1000.0;
                    eprintln!(
                        "  {name:<16} {avg_us:>8.1} us/call  {calls:>5} calls  max {:>8.1} us",
                        s.ns_max as f32 / 1000.0
                    );
                }
                prev.insert(name, s);
            }
            eprintln!("  ~{tick_total:.0} us/tick of the 16667 us budget");
        });
    }

    pm.loop_rate = 60;
    pm.run().expect("connect");
}
