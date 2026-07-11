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
    /// Authoritative replicas.
    pub hog: PoolHandle<Hog>,
    pub impact: PoolHandle<Impact>,
    /// Smoothed views rendering should read (predicted own truck wins on
    /// the truck draw pool; hogs are pure interp).
    pub truck_draw: PoolHandle<Truck>,
    pub hog_draw: PoolHandle<Hog>,
    /// The co-op scoreboard (server-owned synced single).
    pub hunt: SingleRx<Hunt>,
    pub input: InputTx<Drive>,
    pub respawn: EventTx<Respawn>,
    pub pred: SingleHandle<Predictor<Truck, Drive>>,
}

/// Register the full channel set and install prediction + interpolation.
/// No connect here — `run` does that once the schema is complete.
pub fn client_setup(pm: &mut PmClient) -> ClientWorld {
    let truck = pm.sync_pool::<Truck>("truck");
    let hog = pm.sync_pool::<Hog>("hog");
    let impact = pm.sync_pool::<Impact>("impact");
    let hunt = pm.sync_single::<Hunt>("hunt");
    let input = pm.input::<Drive>("drive");
    let respawn = pm.event::<Respawn>("respawn");

    // Local truck: reconcile against the input-seq echo, replay unacked
    // sends — the same truck_step the server runs makes it byte-exact.
    let pred = pm.predict_pool(&truck, &input, truck_step, err_metric, 1e-4, FIXED_DT);

    // Remote smoothing, shared delay contract with the server's shot
    // rewind (see common.rs INTERP_DELAY). Capped extrapolation rides
    // loss bursts — and, for the horde, budget-rotation gaps.
    let extrap = std::env::var("PM_EXTRAP_MS")
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
        .map_or(0.05, |ms| ms / 1000.0);
    let truck_draw = pm.interp_pool(&truck, truck_lerp, interp_delay() as f64, extrap);
    let hog_draw = pm.interp_pool(&hog, hog_lerp, interp_delay() as f64, extrap);

    ClientWorld {
        hog,
        impact,
        truck_draw,
        hog_draw,
        hunt,
        input,
        respawn,
        pred,
    }
}

/// Headless bot: hunt the nearest hog — drive at it, shoot when lined
/// up, wander when the wave is dead.
pub fn run_bot(n: u32) {
    let mut pm = Pm::client(ADDR, 1.0 / FIXED_DT);
    let w = client_setup(&mut pm);

    pm.task_add("bot", 4.0, 0.0, move |pm| {
        let t = pm.tick() as f32 / 60.0 + n as f32 * 1.7;
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
                let (dx, dz) = (h.x - me.x, h.z - me.z);
                (*h, (dx * dx + dz * dz).sqrt())
            })
            .min_by(|a, b| a.1.total_cmp(&b.1));

        let (mut turn, mut thrust, mut fire) = match target {
            Some((h, d)) => {
                let bearing = (h.x - me.x).atan2(h.z - me.z);
                let err = wrap_angle(bearing - me.heading);
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
        let r = (me.x * me.x + me.z * me.z).sqrt();
        if r > ARENA - 10.0 {
            let home = (-me.x).atan2(-me.z);
            turn = wrap_angle(home - me.heading).clamp(-1.0, 1.0);
            thrust = 0.7;
            fire = 0.0;
        }
        w.input.set(Drive {
            thrust,
            turn,
            fire,
            bot: 1.0, // AI: steering lags
        });
    });

    pm.loop_rate = 60;
    pm.run().expect("connect");
}
