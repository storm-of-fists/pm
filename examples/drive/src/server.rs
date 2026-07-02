//! Authoritative drive server. All transport plumbing lives in pm's net
//! layer (`pm.sync_pool` + `pm.run`) — this file is pure gameplay: spawn a
//! car per peer, step each car with its command-frame input.

use pm::{Commands, Id, PeerEvents, Pm, ServerOwn};

use crate::common::*;

pub fn run(quiet: bool) {
    let mut pm = Pm::server(ADDR);
    // Two replicated pools joined by id: motion (predicted client-side) and
    // score (authoritative-only). Registration order is irrelevant — pools
    // are keyed by name on the wire.
    let car = pm.sync_pool::<Car>("car");
    let score = pm.sync_pool::<Score>("score");
    if !quiet {
        eprintln!("drive server on {ADDR}");
    }
    let peers = pm.single::<PeerEvents>("net.peers");
    let cmds = pm.single::<Commands<Drive>>("net.cmds");
    // `ServerOwn` is the built-in peer→entity channel: recording a peer's
    // car here both ships its id down (so the client knows which car is
    // its own — no bespoke handshake) and serves as our garage lookup.
    let own = pm.single::<ServerOwn>("net.own");
    // Reliable client→server events: each peer can ask to be flipped back to
    // its spawn. The receiver only exists on the server (one-way channel).
    let respawns = pm.event::<Respawn>("respawn");

    // Apply respawns (prio 11 — after the net task fills net.events, before
    // the drive step). The reset is a plain state write; it replicates back.
    pm.task_add("respawn", 11.0, 0.0, {
        let car = car.clone();
        let own = own.clone();
        move |_pm| {
            for (peer, _) in respawns.drain() {
                if let Some(id) = own.get().get(peer)
                    && let Some(mut c) = car.get_mut().get_mut(id)
                {
                    *c = spawn_car(peer);
                }
            }
        }
    });

    // Joins and leaves: a car per peer (prio 10 — after the net task).
    pm.task_add("roster", 10.0, 0.0, {
        let car = car.clone();
        let score = score.clone();
        let own = own.clone();
        move |pm| {
            for &p in &peers.get().joined {
                let id = pm.id_add();
                car.get_mut().add(id, spawn_car(p));
                score.get_mut().add(id, Score::default());
                own.get_mut().set(p, id);
                if !quiet {
                    eprintln!("[server] peer {p} joined");
                }
            }
            for &p in &peers.get().left {
                if let Some(id) = own.get().get(p) {
                    pm.id_remove(id);
                }
                own.get_mut().clear(p);
                if !quiet {
                    eprintln!("[server] peer {p} left");
                }
            }
        }
    });

    pm.task_add("drive", 30.0, 0.0, {
        let car = car.clone();
        let own = own.clone();
        move |_pm| {
            let mut cmds = cmds.get_mut();
            let mut car = car.get_mut();
            for (&peer, &id) in &own.get().0 {
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
    pm.run::<Drive>().unwrap_or_else(|e| {
        eprintln!("cannot serve {ADDR}: {e}");
        eprintln!("(a previous drive may still be running: pkill -x drive)");
        std::process::exit(1);
    });
}
