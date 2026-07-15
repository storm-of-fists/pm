//! Hogs client gameplay, shared by the SDL player and headless bots:
//! full-schema registration (every client registers every channel — the
//! handshake demands it), local-truck prediction, and snapshot interp
//! for BOTH remote pools (trucks and the horde). The horde interp is the
//! interesting one: at 300 hogs the byte budget rotates and the interp
//! delay is what rides the multi-tick gaps between a hog's updates.

use pm::{EventTx, InputTx, Pm, PmClient, PoolHandle, Predictor, SingleHandle, SingleRx};

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
    /// Both vehicle predictors ride the same input channel; whichever
    /// pool holds the avatar is live, the other idles with `state() ==
    /// None` — which is also how game code asks "am I flying?".
    pub pred: SingleHandle<Predictor<Truck, Drive>>,
    pub pred_heli: SingleHandle<Predictor<Heli, Drive>>,
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
    let input = pm.input::<Drive>("drive");
    let respawn = pm.event::<Respawn>("respawn");

    // Local vehicle: reconcile against the input-seq echo, replay
    // unacked sends — the same steps the server runs make it byte-exact.
    // BOTH vehicles get predictors on the ONE input channel; the one
    // whose pool holds the avatar runs, the other idles empty.
    let pred = pm.predict_pool(&truck, &input, truck_step, err_metric, 1e-4, FIXED_DT);
    let pred_heli = pm.predict_pool(&heli, &input, heli_step, heli_err, 1e-4, FIXED_DT);

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
        pred,
        pred_heli,
    }
}

/// Headless bot: hunt the nearest hog — drive at it, shoot when lined
/// up, wander when the wave is dead.
pub fn run_bot(n: u32) {
    let mut pm = Pm::client(ADDR, 1.0 / FIXED_DT);
    let w = client_setup(&mut pm);

    // Building-jam recovery state: seconds spent commanding thrust while
    // not moving, and seconds of back-out left once we give up.
    let mut jam = 0.0f32;
    let mut back = 0.0f32;
    // Pilot bots: one-shot "give me the heli" request state.
    let mut asked_heli = false;

    pm.task_add("bot", 4.0, 0.0, move |pm| {
        let t = pm.tick() as f32 / 60.0 + n as f32 * 1.7;

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
                        let aligned = err.abs() < 0.25 && d < GUN_RANGE * 0.9;
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
                let tof = d / BULLET_SPEED;
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
                (
                    err.clamp(-1.0, 1.0),
                    // Bear down on far hogs, hold off point-blank ones.
                    if d > 14.0 { 0.85 } else { 0.35 },
                    (aligned && d < GUN_RANGE * 0.95) as i32 as f32,
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
            thrust = -0.7;
            turn = 1.0;
            fire = 0.0;
        } else {
            if thrust > 0.2 && me.speed().abs() < 1.0 {
                jam += FIXED_DT;
            } else {
                jam = 0.0;
            }
            if jam > 0.8 {
                jam = 0.0;
                back = 1.2;
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
    // pools riding the horde) — bot 0 only, so the dumps don't interleave.
    if n == 0 && std::env::var("PM_PROF").is_ok() {
        let mut prev: std::collections::HashMap<String, pm::TaskStat> = Default::default();
        pm::task!(pm, "prof", 91.0, 5.0, [], move |pm| {
            eprintln!("-- bot0 task stats (last 5s) --");
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
