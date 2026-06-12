//! Drive client gameplay, shared by the SDL player and headless bots.
//! The transport (pump, snapshot apply + ack, fixed-cadence input send)
//! is pm's net module; this file adds what the module can't know: which
//! entity is ours, how to predict it (`pm::Predictor` over the shared
//! step fn), and how remote cars smooth (`pm::pool_mirror`).

use std::time::Duration;

use pm::{
    AppliedLog, ClientEvents, Id, NetClient, NetInput, Pm, Predictor, QuicClient, SentLog,
    coast_blend, pool_mirror, vec2,
};

use crate::common::*;

#[derive(Default)]
pub struct Stats {
    pub mine: Option<Id>,
    pub corrections: u32,
}

fn err_metric(a: &Car, b: &Car) -> f32 {
    (a.x - b.x).abs() + (a.z - b.z).abs() + (a.heading - b.heading).abs() + (a.speed - b.speed).abs()
}

pub fn connect(net: &NetClient) -> QuicClient {
    let mut quic = QuicClient::connect(ADDR, &net.schema()).expect("connect");
    let lag_ms: f32 = std::env::var("PM_LAG_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(0.0);
    let loss: f32 = std::env::var("PM_LOSS").ok().and_then(|v| v.parse().ok()).unwrap_or(0.0);
    if lag_ms > 0.0 || loss > 0.0 {
        quic.link_lag_set(Duration::from_secs_f32(lag_ms / 1000.0), loss);
    }
    quic
}

/// Everything but input generation and rendering. The input layer
/// writes the `"net.input"` single; rendering reads `car_draw` plus the
/// predictor's car for the local one.
pub fn add_client_tasks(
    pm: &mut Pm,
    quic: QuicClient,
    net: NetClient,
    car: &pm::Handle<Car>,
    draw: &pm::Handle<Car>,
) {
    net.connect::<Drive>(pm, quic, 1.0 / FIXED_DT);

    let pred = pm.single::<Predictor<Car, Drive>>("pred");
    let stats = pm.single::<Stats>("stats");
    let events = pm.single::<ClientEvents>("net.events");
    let applied = pm.single::<AppliedLog>("net.applied");
    let sent = pm.single::<SentLog<Drive>>("net.sent");
    let car = car.clone();
    let draw = draw.clone();

    // Prediction, right after the net module's tick (prio 6): reconcile
    // against each applied snapshot's input-seq echo, then feed this
    // tick's sent inputs into the rewind ring.
    pm.task_add("predict", 6.0, 0.0, {
        let pred = pred.clone();
        let car = car.clone();
        let stats = stats.clone();
        move |_pm| {
            for (ty, payload) in &events.borrow().0 {
                if *ty == EV_VEHICLE && payload.len() == 4 {
                    stats.borrow_mut().mine =
                        Some(Id(u32::from_le_bytes(payload.as_slice().try_into().unwrap())));
                }
            }
            let mine = stats.borrow().mine;
            for a in &applied.borrow().0 {
                let auth = mine.and_then(|id| car.borrow().get(id).copied());
                let Some(auth) = auth else { continue };
                let corrected = pred.borrow_mut().reconcile(
                    auth,
                    a.input_seq,
                    |s, c| drive_step(s, c, FIXED_DT),
                    err_metric,
                    1e-4,
                );
                if corrected {
                    stats.borrow_mut().corrections += 1;
                }
            }
            for &(seq, cmd) in &sent.borrow().0 {
                pred.borrow_mut().predict(seq, cmd, |s, c| drive_step(s, c, FIXED_DT));
            }
        }
    });

    // Remote cars dead-reckon between budget-rotated refreshes.
    pm.task_add("smooth", 30.0, 0.0, {
        let stats = stats.clone();
        move |pm| {
            let dt = pm.loop_dt();
            let mine = stats.borrow().mine;
            pool_mirror(&car, &draw, |id, d, a: &Car| {
                if Some(id) == mine {
                    return *a; // overwritten below from the predictor
                }
                let vel = vec2(a.heading.sin(), a.heading.cos()) * a.speed;
                let p = coast_blend(vec2(d.x, d.z), vel, vec2(a.x, a.z), dt, 0.15);
                Car { x: p.x, z: p.y, ..*a }
            });
            if let (Some(id), Some(p)) = (mine, pred.borrow().state()) {
                draw.borrow_mut().add(id, p);
            }
        }
    });
}

/// Headless bot: drives a lazy sine-wave racing line.
pub fn run_bot(n: u32) {
    let mut pm = Pm::new();
    let car = pm.pool::<Car>("car");
    let draw = pm.pool::<Car>("car_draw");
    let mut net = NetClient::new();
    net.pool_sync("car", &car);
    let quic = connect(&net);
    add_client_tasks(&mut pm, quic, net, &car, &draw);

    let cmd = pm.single::<NetInput<Drive>>("net.input");
    let pred = pm.single::<Predictor<Car, Drive>>("pred");
    pm.task_add("bot", 4.0, 0.0, move |pm| {
        let t = pm.tick() as f32 / 60.0 + n as f32 * 1.7;
        // Wander on a sine, but steer home before grinding a wall —
        // the original pure-sine bot looped into the back wall and sat
        // there at 0.1 speed forever.
        let Some(c) = pred.borrow().state() else { return };
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
        cmd.borrow_mut().0 = Drive { thrust: 0.75 + 0.25 * (t * 0.31).sin(), turn };
    });

    pm.loop_rate = 60;
    pm.loop_run();
}
