//! Drive client netcode, shared by the SDL player and headless bots:
//! snapshot apply + ack, prediction/reconciliation via pm::Predictor,
//! input at a FIXED 60 Hz cadence (the render loop may run at any
//! display rate — prediction must step exactly like the server), and
//! remote-car smoothing via pm::pool_mirror.

use std::time::Duration;

use pm::{Id, NetClient, Pm, Predictor, QuicClient, coast_blend, pool_mirror, vec2};

use crate::common::*;

#[derive(Default)]
pub struct CurCmd(pub Drive);

#[derive(Default)]
pub struct Stats {
    pub mine: Option<Id>,
    pub rtt_ms: f32,
    pub snapshots: u32,
    pub corrections: u32,
    pub peer: u8,
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
/// writes `CurCmd`; rendering reads `car_draw` + the predictor's car
/// for the local one.
pub fn add_client_tasks(
    pm: &mut Pm,
    mut quic: QuicClient,
    net: NetClient,
    car: &pm::Handle<Car>,
    draw: &pm::Handle<Car>,
) {
    let cmd = pm.single::<CurCmd>("cmd");
    let pred = pm.single::<Predictor<Car, Drive>>("pred");
    let stats = pm.single::<Stats>("stats");
    let car = car.clone();
    let draw = draw.clone();

    // Pump + apply every loop tick (snappy at any display rate).
    pm.task_add("net", 5.0, {
        let pred = pred.clone();
        let car = car.clone();
        let stats = stats.clone();
        let mut input_accum = 0.0f32;
        move |pm| {
            quic.pump();
            if let Some(err) = quic.error() {
                eprintln!("disconnected: {err}");
                pm.loop_quit();
                return;
            }
            if quic.is_gone() {
                eprintln!("server closed the connection");
                pm.loop_quit();
                return;
            }
            for (ty, payload) in quic.events_drain() {
                if ty == EV_VEHICLE && payload.len() == 4 {
                    stats.borrow_mut().mine =
                        Some(Id(u32::from_le_bytes(payload.as_slice().try_into().unwrap())));
                }
            }
            for snap in quic.snapshots_drain() {
                let Ok(applied) = net.apply(pm, &snap) else { continue };
                quic.ack_send(applied.tick);
                stats.borrow_mut().snapshots += 1;
                let mine = stats.borrow().mine;
                let auth = mine.and_then(|id| car.borrow().get(id).copied());
                let Some(auth) = auth else { continue };
                let corrected = pred.borrow_mut().reconcile(
                    auth,
                    applied.input_seq,
                    |s, c| drive_step(s, c, FIXED_DT),
                    err_metric,
                    1e-4,
                );
                if corrected {
                    stats.borrow_mut().corrections += 1;
                }
            }
            if let Some(peer) = quic.handshake_done() {
                pm.local_peer = peer;
                stats.borrow_mut().peer = peer;
            }
            stats.borrow_mut().rtt_ms = quic.rtt().as_secs_f32() * 1e3;

            // Input at a fixed 60 Hz cadence regardless of loop rate:
            // the server consumes one per tick and prediction must step
            // FIXED_DT per send, or replay diverges from authority.
            input_accum += pm.loop_dt();
            while input_accum >= FIXED_DT {
                input_accum -= FIXED_DT;
                if quic.handshake_done().is_some() {
                    let cmd = cmd.borrow().0;
                    let seq = quic.input_send(bytemuck::bytes_of(&cmd));
                    pred.borrow_mut().predict(seq, cmd, |s, c| drive_step(s, c, FIXED_DT));
                }
            }
        }
    });

    // Remote cars dead-reckon between budget-rotated refreshes.
    pm.task_add("smooth", 30.0, {
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

    let cmd = pm.single::<CurCmd>("cmd");
    let pred = pm.single::<Predictor<Car, Drive>>("pred");
    pm.task_add("bot", 4.0, move |pm| {
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
