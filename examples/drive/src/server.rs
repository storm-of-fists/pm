//! Authoritative drive server. All transport plumbing lives in pm's net
//! layer (`pm.sync` + `pm.serve`) — this file is pure gameplay: spawn a
//! car per peer, step each car with its command-frame input.

use std::collections::HashMap;

use pm::{Commands, Id, PeerEvents, Pm, QuicServer, ServerOutbox};

use crate::common::*;

#[derive(Default)]
struct Garage(HashMap<u8, Id>);

pub fn run(quiet: bool) {
    let mut pm = Pm::new();
    let car = pm.pool::<Car>("car");
    let score = pm.pool::<Score>("score");
    let garage = pm.single::<Garage>("garage");

    // Two replicated pools joined by id: motion (predicted client-side) and
    // score (authoritative-only). Order must match the clients' sync calls.
    pm.sync(&car);
    pm.sync(&score);
    let quic = QuicServer::bind(ADDR, &pm.net_schema()).unwrap_or_else(|e| {
        eprintln!("cannot bind {ADDR}: {e}");
        eprintln!("(a previous drive may still be running: pkill -x drive)");
        std::process::exit(1);
    });
    if !quiet {
        eprintln!("drive server on {ADDR}");
    }
    pm.serve::<Drive>(quic);
    let peers = pm.single::<PeerEvents>("net.peers");
    let cmds = pm.single::<Commands<Drive>>("net.cmds");
    let out = pm.single::<ServerOutbox>("net.out");

    // Joins and leaves: a car per peer (prio 10 — after the net task).
    pm.task_add("roster", 10.0, 0.0, {
        let car = car.clone();
        let score = score.clone();
        let garage = garage.clone();
        move |pm| {
            for &p in &peers.get().joined {
                let id = pm.id_add();
                car.get_mut().add(id, spawn_car(p));
                score.get_mut().add(id, Score::default());
                garage.get_mut().0.insert(p, id);
                out.get_mut().send(p, EV_VEHICLE, &id.0.to_le_bytes());
                if !quiet {
                    eprintln!("[server] peer {p} joined");
                }
            }
            for &p in &peers.get().left {
                if let Some(id) = garage.get_mut().0.remove(&p) {
                    pm.id_remove(id);
                }
                if !quiet {
                    eprintln!("[server] peer {p} left");
                }
            }
        }
    });

    pm.task_add("drive", 30.0, 0.0, {
        let car = car.clone();
        let garage = garage.clone();
        move |_pm| {
            let mut cmds = cmds.get_mut();
            let mut car = car.get_mut();
            for (&peer, &id) in &garage.get().0 {
                // pop = command-frame consumption: one input per tick,
                // hold-when-dry, bounded skip-ahead. The consumed seq is
                // echoed automatically for prediction reconciliation.
                let cmd = cmds.pop(peer);
                if let Some(mut c) = car.get_mut(id) {
                    drive_step(&mut c, cmd, FIXED_DT);
                }
            }
        }
    });

    // Collision + scoring, right after the drive step (prio 31). Both are
    // server-authoritative: collisions push cars apart in the `car` (motion)
    // pool, and scoring banks into the separate `score` pool joined by id.
    // The split keeps motion predictable and score authoritative-only.
    pm.task_add("score", 31.0, 0.0, {
        let car = car.clone();
        let score = score.clone();
        move |_pm| {
            let mut cars = car.get_mut();
            let snap: Vec<(Id, Car)> = cars.iter().map(|(id, c)| (id, *c)).collect();
            let state: Vec<Car> = snap.iter().map(|(_, c)| *c).collect();

            // Positional push from the pre-move snapshot.
            let push = collide_push(&state);
            for (k, (id, _)) in snap.iter().enumerate() {
                if push[k] == (0.0, 0.0) {
                    continue;
                }
                if let Some(mut c) = cars.get_mut(*id) {
                    c.x = (c.x + push[k].0).clamp(-ARENA, ARENA);
                    c.z = (c.z + push[k].1).clamp(-ARENA, ARENA);
                    c.speed *= 0.85; // crunch scrubs a little speed
                }
            }
            drop(cars); // motion done; scoring touches the other pool only

            // Bank each car's score: a flat HIT_COST per collision (push[k]
            // non-zero == overlapping this tick), debounced by HIT_COOLDOWN
            // so the push flicking us across the overlap line can't bill us
            // every tick, plus the continuous positive proximity earning.
            let mut scores = score.get_mut();
            for (k, (id, _)) in snap.iter().enumerate() {
                let others: Vec<Car> = state
                    .iter()
                    .enumerate()
                    .filter(|(o, _)| *o != k)
                    .map(|(_, c)| *c)
                    .collect();
                let reward = score_rate(&state[k], &others);
                let hit = push[k] != (0.0, 0.0);
                if let Some(mut s) = scores.get_mut(*id) {
                    s.hit_cd = (s.hit_cd - FIXED_DT).max(0.0);
                    if hit && s.hit_cd <= 0.0 {
                        s.points = (s.points - HIT_COST).max(0.0);
                        s.hit_cd = HIT_COOLDOWN;
                    }
                    s.points = (s.points + reward * FIXED_DT).max(0.0);
                    // HUD rate: bleed red for the cooldown after a hit so the
                    // -100 reads, otherwise the live earning rate. Replicated;
                    // the HUD shows it raw.
                    s.rate = if s.hit_cd > 0.0 { -HIT_COST } else { reward };
                }
            }
        }
    });

    if !quiet {
        pm.task_add("status", 90.0, 5.0, {
            let car = car.clone();
            move |pm| {
                let cars = car.get();
                let speeds: Vec<String> = cars
                    .values()
                    .iter()
                    .map(|c| format!("{:.1}", c.speed))
                    .collect();
                eprintln!(
                    "[server] t={} cars={} speeds=[{}]",
                    pm.tick() / 60,
                    cars.len(),
                    speeds.join(", ")
                );
            }
        });
    }

    pm.loop_rate = 60;
    pm.loop_run();
}
