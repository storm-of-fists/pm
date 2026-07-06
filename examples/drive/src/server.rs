//! Authoritative drive server. All transport plumbing lives in pm's net
//! layer (`pm.sync_pool` + `pm.run`) — this file is pure gameplay: spawn a
//! car per peer, step each car with its command-frame input.

use pm::{Id, Pm};

use crate::common::*;

pub fn run(quiet: bool) {
    let mut pm = Pm::server(ADDR);
    // Two replicated pools joined by id: motion (predicted client-side) and
    // score (authoritative-only). Registration order is irrelevant — pools
    // are keyed by name on the wire.
    let car = pm.sync_pool::<Car>("car");
    let score = pm.sync_pool::<Score>("score");
    // Billed collisions as transient replicated facts: fresh id per hit,
    // expired by the server's TTL — clients render entries and never clean
    // up. (Every client registers this pool too: one handshake schema.)
    let contact = pm.sync_pool::<Contact>("contact");
    pm.ttl_pool(&contact, CONTACT_TTL);
    // A second of car history — the rewind memory that lets scoring judge
    // each peer against the world THEY saw (acked tick − interp delay).
    let hist = pm.history_pool(&car, 1.0);
    if !quiet {
        eprintln!("drive server on {ADDR}");
    }
    // The server surface: joins/leaves this tick, and the peer→entity
    // table. `own_set` both ships a peer's car id down in every snapshot
    // header (so the client knows which car is its own — no bespoke
    // handshake) and serves as our garage lookup.
    let net = pm.net();
    // THE continuous input channel (one per connection): the same name and
    // pod the clients register — the handshake schema enforces it.
    let inputs = pm.input::<Drive>("drive");
    // Reliable client→server events: each peer can ask to be flipped back to
    // its spawn. The receiver only exists on the server (one-way channel).
    let respawns = pm.event::<Respawn>("respawn");

    // Apply respawns (prio 11 — after the net task receives events, before
    // the drive step). The reset is a plain state write; it replicates back.
    pm.task_add("respawn", 11.0, 0.0, {
        let car = car.clone();
        let net = net.clone();
        move |_pm| {
            for (peer, _) in respawns.drain() {
                if let Some(id) = net.own(peer)
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
        let net = net.clone();
        move |pm| {
            for p in net.joined() {
                let id = pm.id_add();
                car.get_mut().add(id, spawn_car(p));
                score.get_mut().add(id, Score::default());
                net.own_set(p, id);
                if !quiet {
                    eprintln!("[server] peer {p} joined");
                }
            }
            for p in net.left() {
                // Ownership auto-clears next tick; despawning is ours.
                if let Some(id) = net.own(p) {
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
        let net = net.clone();
        move |_pm| {
            let mut car = car.get_mut();
            for (peer, id) in net.owned() {
                // pop = command-frame consumption: one input per tick,
                // hold-when-dry, bounded skip-ahead. The consumed seq is
                // echoed automatically for prediction reconciliation.
                let cmd = inputs.pop(peer);
                if let Some(mut c) = car.get_mut(id) {
                    drive_step(&mut c, cmd, FIXED_DT);
                }
            }
        }
    });

    // Collision + scoring, right after the drive step (prio 31). The
    // positional push stays server-present physics — mutual, one truth.
    // The JUDGMENT (were you on someone; how close was the pass) is
    // per-actor and LAG-COMPENSATED: each peer is scored against the
    // rivals as they saw them — their acked tick minus their interp
    // delay, rewound through the car pool's history ring — so a pass
    // that looked clean on your screen doesn't bill you just because
    // the server's present had moved on ("favor the actor"; on mutual
    // contact each side is judged from its own view, independently).
    pm.task_add("score", 31.0, 0.0, {
        let car = car.clone();
        let score = score.clone();
        let contact = contact.clone();
        let net = net.clone();
        move |pm| {
            let mut cars = car.get_mut();
            let snap: Vec<(Id, Car)> = cars.iter().map(|(id, c)| (id, *c)).collect();
            let state: Vec<Car> = snap.iter().map(|(_, c)| *c).collect();

            // Positional push from the pre-move snapshot (present time).
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
            drop(cars); // motion done; judgment reads, never writes, cars

            // Bank each actor's score against their own view: a flat
            // HIT_COST on overlap (debounced by HIT_COOLDOWN so the push
            // flicking cars across the overlap line can't bill every
            // tick) plus the continuous proximity earning.
            let iticks = interp_ticks();
            let mut scores = score.get_mut();
            let mut hits: Vec<Contact> = Vec::new();
            for (peer, id) in net.owned() {
                let Some(me) = car.get().get(id).copied() else {
                    continue;
                };
                // The world this peer was looking at when they steered
                // into (or clear of) the contact. A just-joined peer has
                // acked nothing yet: view 0 clamps to the oldest frame
                // and settles within an RTT.
                let view = net.acked_tick(peer).saturating_sub(iticks);
                let others: Vec<Car> = hist.frame(view).map_or(Vec::new(), |f| {
                    f.iter()
                        .filter(|&&(fid, _)| fid != id)
                        .map(|&(_, c)| c)
                        .collect()
                });
                let hit_at = others.iter().find_map(|o| {
                    capsule_overlap(&me, o)
                        .map(|_| ((me.x + o.x) * 0.5, (me.z + o.z) * 0.5))
                });
                let reward = score_rate(&me, &others);
                if let Some(mut s) = scores.get_mut(id) {
                    s.hit_cd = (s.hit_cd - FIXED_DT).max(0.0);
                    if let Some((hx, hz)) = hit_at
                        && s.hit_cd <= 0.0
                    {
                        s.points = (s.points - HIT_COST).max(0.0);
                        s.hit_cd = HIT_COOLDOWN;
                        hits.push(Contact { x: hx, z: hz });
                        if !quiet {
                            // The lag-comp story in one line: billed at the
                            // world THIS peer saw (view), not the server's
                            // present tick.
                            eprintln!(
                                "[server] hit billed: peer {peer} at ({hx:.1},{hz:.1}) view={view} now={}",
                                pm.tick()
                            );
                        }
                    }
                    s.points = (s.points + reward * FIXED_DT).max(0.0);
                    // HUD rate: bleed red for the cooldown after a hit so the
                    // -100 reads, otherwise the live earning rate. Replicated;
                    // the HUD shows it raw.
                    s.rate = if s.hit_cd > 0.0 { -HIT_COST } else { reward };
                }
            }
            drop(scores);
            // Each billed hit becomes a transient replicated fact on a
            // fresh id; the pool's TTL is the whole cleanup story.
            for c in hits {
                let id = pm.id_add();
                contact.get_mut().add(id, c);
            }
        }
    });

    if !quiet {
        pm.task_add("status", 90.0, 5.0, {
            let car = car.clone();
            let score = score.clone();
            let contact = contact.clone();
            move |pm| {
                let cars = car.get();
                let speeds: Vec<String> = cars
                    .values()
                    .iter()
                    .map(|c| format!("{:.1}", c.speed))
                    .collect();
                let pts: Vec<String> = score
                    .get()
                    .values()
                    .iter()
                    .map(|s| format!("{:.0}", s.points))
                    .collect();
                // contacts churns 0..few if the TTL is doing its job —
                // monotonic growth here means expiry broke. pts moving
                // proves the lag-comped judgment reads real frames (a
                // broken rewind zeroes the proximity reward silently).
                eprintln!(
                    "[server] t={} cars={} contacts={} pts=[{}] speeds=[{}]",
                    pm.tick() / 60,
                    cars.len(),
                    contact.get().len(),
                    pts.join(", "),
                    speeds.join(", ")
                );
            }
        });
    }

    pm.loop_rate = 60;
    pm.run().unwrap_or_else(|e| {
        eprintln!("cannot serve {ADDR}: {e}");
        eprintln!("(a previous drive may still be running: pkill -x drive)");
        std::process::exit(1);
    });
}
