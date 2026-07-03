//! Drive client gameplay, shared by the SDL player and headless bots.
//! The transport, local-avatar prediction (`pm.predict_pool`), and
//! remote-car snapshot interpolation (`pm.interp`) are all pm's net module
//! now; this file supplies only what the module can't know — THE shared
//! step, the error metric, and how cars interpolate.

use pm::{InputTx, Pm, PmClient, Predictor, SingleHandle};

use crate::common::*;

fn err_metric(a: &Car, b: &Car) -> f32 {
    (a.x - b.x).abs()
        + (a.z - b.z).abs()
        + (a.heading - b.heading).abs()
        + (a.speed - b.speed).abs()
        + (a.steer - b.steer).abs()
}

/// Connect to the server and install everything but input generation and
/// rendering. pm owns the transport (`PM_LAG_MS`/`PM_LOSS` simulate the
/// link). Returns the predictor single (rendering reads `state()` /
/// `corrections`) and the draw pool (the smoothed view rendering should
/// iterate — predicted local car, interpolated remotes).
pub fn add_client_tasks(
    pm: &mut PmClient,
    car: &pm::PoolHandle<Car>,
    input: &InputTx<Drive>,
) -> (SingleHandle<Predictor<Car, Drive>>, pm::PoolHandle<Car>) {
    // No connect here — `run` does that once the schema is complete.
    // Local avatar: reconcile against the server's input-seq echo, replay
    // the input channel's unacked sends, and draw smooth-predicted. The
    // same `drive_step` the server runs is what makes reconciliation
    // byte-exact.
    let pred = pm.predict_pool(car, input, drive_step, err_metric, 1e-4, FIXED_DT);

    // Remote cars: snapshot interpolation ~50 ms behind newest, with a
    // capped 50 ms extrapolation to ride loss bursts — env-tunable so feel
    // can be A/B'd live. (The local car is overwritten by the predictor.)
    let env_ms = |k: &str, d: f64| {
        std::env::var(k)
            .ok()
            .and_then(|v| v.parse::<f64>().ok())
            .map_or(d, |ms| ms / 1000.0)
    };
    let draw = pm.interp_pool(
        car,
        car_lerp,
        env_ms("PM_INTERP_MS", 0.05),
        env_ms("PM_EXTRAP_MS", 0.05),
    );

    (pred, draw)
}

/// Headless bot: drives a lazy sine-wave racing line.
pub fn run_bot(n: u32) {
    let mut pm = Pm::client(ADDR, 1.0 / FIXED_DT);
    // Same synced pools as the server (order doesn't matter — keyed by name).
    let car = pm.sync_pool::<Car>("car");
    pm.sync_pool::<Score>("score");
    // Same channels as the server, too: the handshake schema covers every
    // named channel, so even a bot that never respawns registers the
    // respawn event — the schema is the connection's full contract.
    let input = pm.input::<Drive>("drive");
    let _respawn = pm.event::<Respawn>("respawn");
    // Headless: the draw pool is unused (no rendering), only the predictor.
    let (pred, _draw) = add_client_tasks(&mut pm, &car, &input);

    pm.task_add("bot", 4.0, 0.0, move |pm| {
        let t = pm.tick() as f32 / 60.0 + n as f32 * 1.7;
        // Wander on a sine, but steer home before grinding a wall —
        // the original pure-sine bot looped into the back wall and sat
        // there at 0.1 speed forever.
        let Some(c) = pred.get().state() else {
            return;
        };
        let r = (c.x * c.x + c.z * c.z).sqrt();
        let turn = if r > ARENA - 14.0 {
            let desired = (-c.x).atan2(-c.z); // heading toward the center
            let err = (desired - c.heading + std::f32::consts::PI)
                .rem_euclid(std::f32::consts::TAU)
                - std::f32::consts::PI;
            err.clamp(-1.0, 1.0)
        } else {
            (t * 0.43).sin() * 0.8
        };
        input.set(Drive {
            thrust: 0.75 + 0.25 * (t * 0.31).sin(),
            turn,
            drift: 0.0,
            bot: 1.0, // AI: steering lags, arrow leads
        });
    });

    pm.loop_rate = 60;
    pm.run().expect("connect");
}
