//! Authoritative hogs server. Trucks are player-driven and predicted;
//! hogs are server-owned NPCs stepped by an AI task — clients only ever
//! interpolate them. Shooting is lag-compensated the same way drive's
//! scoring is: each shot is judged against the hog frame the SHOOTER was
//! looking at (`acked_tick − interp_ticks`, rewound through the hog
//! pool's history ring), while damage lands in the present.
//!
//! Deliberate simplicities (this example is the replication stress lab,
//! not an AI showcase): hogs don't avoid each other (no separation
//! force — pm::SpatialGrid is the tool when that matters), and a miss
//! spawns no marker (the client draws its own aim line; hit/kill/bite
//! markers are the replicated facts).

use pm::{Id, Pm, Rng};

use crate::common::*;

/// Server-local per-hog state (never replicated — local pools are free,
/// they don't enter the handshake schema).
#[derive(Clone, Copy, Default)]
struct HogBrain {
    /// Re-bite lockout, seconds left.
    bite_cd: f32,
    /// Break-off timer after a bite, seconds left.
    flee: f32,
    /// Wander phase so the horde doesn't move in lockstep.
    seed: f32,
}

pub fn run(quiet: bool) {
    let mut pm = Pm::server(ADDR);
    // Replicated pools: trucks (predicted client-side), hogs (interp'd
    // client-side), impact markers (TTL'd transient facts). Plus the
    // co-op scoreboard as a synced single.
    let truck = pm.sync_pool::<Truck>("truck");
    let hog = pm.sync_pool::<Hog>("hog");
    let impact = pm.sync_pool::<Impact>("impact");
    pm.ttl_pool(&impact, IMPACT_TTL);
    let hunt = pm.sync_single::<Hunt>("hunt");
    // A second of hog history — the rewind memory shots are judged in.
    let hist = pm.history_pool(&hog, 1.0);
    // Server-only state: per-hog brains, per-truck gun cooldowns. Keyed
    // by the same ids as their synced siblings; entity removal cleans
    // them up with everything else.
    let brain = pm.pool::<HogBrain>("hog.brain");
    let gun = pm.pool::<f32>("truck.gun");

    if !quiet {
        eprintln!("hogs server on {ADDR}");
    }
    let net = pm.net();
    let inputs = pm.input::<Drive>("drive");
    let respawns = pm.event::<Respawn>("respawn");

    // Joins and leaves: a truck (and its gun) per peer.
    pm.task_add("roster", 10.0, 0.0, {
        let truck = truck.clone();
        let gun = gun.clone();
        let net = net.clone();
        move |pm| {
            for p in net.joined() {
                let id = pm.id_add();
                truck.get_mut().add(id, spawn_truck(p));
                gun.get_mut().add(id, 0.0);
                net.own_set(p, id);
                if !quiet {
                    eprintln!("[server] peer {p} joined");
                }
            }
            for p in net.left() {
                if let Some(id) = net.own(p) {
                    pm.id_remove(id);
                }
                if !quiet {
                    eprintln!("[server] peer {p} left");
                }
            }
        }
    });

    // Respawn events: flip the sender's truck back to its spawn slot.
    pm.task_add("respawn", 11.0, 0.0, {
        let truck = truck.clone();
        let net = net.clone();
        move |_pm| {
            for (peer, _) in respawns.drain() {
                if let Some(id) = net.own(peer)
                    && let Some(mut t) = truck.get_mut().get_mut(id)
                {
                    *t = spawn_truck(peer);
                }
            }
        }
    });

    // The horde (prio 28, before the trucks move): wander until a truck
    // is in aggro range, charge it, bite on contact, break off after a
    // bite. Every hog is written every tick — deliberately: at horde
    // scale that makes the hog pool change-dense, which is exactly the
    // byte-budget-rotation workload this example exists to produce.
    pm.task_add("hog_ai", 28.0, 0.0, {
        let hog = hog.clone();
        let brain = brain.clone();
        let truck = truck.clone();
        let impact = impact.clone();
        let hunt = hunt.clone();
        move |pm| {
            let now = pm.tick() as f32 * FIXED_DT;
            let trucks: Vec<(Id, Truck)> = truck.get().iter().map(|(id, t)| (id, *t)).collect();
            // (bitten truck, where) — applied after the hog borrows drop.
            let mut bites: Vec<(Id, f32, f32)> = Vec::new();
            {
                let mut hogs = hog.get_mut();
                let mut brains = brain.get_mut();
                for (id, mut h) in hogs.iter_mut() {
                    let Some(mut b) = brains.get_mut(id) else {
                        continue;
                    };
                    b.bite_cd = (b.bite_cd - FIXED_DT).max(0.0);
                    b.flee = (b.flee - FIXED_DT).max(0.0);

                    let nearest = trucks
                        .iter()
                        .map(|&(tid, t)| {
                            let (dx, dz) = (t.x - h.x, t.z - h.z);
                            (tid, t, (dx * dx + dz * dz).sqrt())
                        })
                        .min_by(|a, b| a.2.total_cmp(&b.2));

                    // Pick a desired heading and speed.
                    let (desired, target_speed) = match nearest {
                        Some((_, t, _)) if b.flee > 0.0 => {
                            ((h.x - t.x).atan2(h.z - t.z), HOG_FAST)
                        }
                        Some((_, t, d)) if d < HOG_AGGRO => {
                            ((t.x - h.x).atan2(t.z - h.z), HOG_FAST)
                        }
                        _ => (
                            h.heading + (now * 0.7 + b.seed).sin() * 0.9,
                            HOG_SLOW,
                        ),
                    };
                    let turn = wrap_angle(desired - h.heading)
                        .clamp(-HOG_TURN * FIXED_DT, HOG_TURN * FIXED_DT);
                    h.heading += turn;
                    h.speed += (target_speed - h.speed) * (3.0 * FIXED_DT).min(1.0);
                    h.x += h.heading.sin() * h.speed * FIXED_DT;
                    h.z += h.heading.cos() * h.speed * FIXED_DT;
                    // Walls: clamp and head back toward the middle.
                    if h.x.abs() > ARENA || h.z.abs() > ARENA {
                        h.x = h.x.clamp(-ARENA, ARENA);
                        h.z = h.z.clamp(-ARENA, ARENA);
                        h.heading = (-h.x).atan2(-h.z);
                    }

                    // Bite: contact with any truck while off cooldown.
                    if b.bite_cd <= 0.0
                        && let Some((tid, t, _)) = nearest
                        && hog_bites_truck(&h, &t)
                    {
                        bites.push((tid, h.x, h.z));
                        b.bite_cd = BITE_CD;
                        b.flee = HOG_FLEE;
                    }
                }
            }
            for (tid, x, z) in bites {
                if let Some(mut t) = truck.get_mut().get_mut(tid) {
                    t.speed *= 0.4; // the hit you feel
                }
                {
                    let mut sb = hunt.get_mut();
                    sb.points = (sb.points - BITE_COST).max(0.0);
                }
                let id = pm.id_add();
                impact.get_mut().add(id, Impact { x, z, kind: IMPACT_BITE });
            }
        }
    });

    // Trucks + guns (prio 30): command-frame input, THE shared step, and
    // the fixed forward gun. The shot is judged in the shooter's rewound
    // frame; the damage lands on the present hog (if it still lives).
    pm.task_add("drive", 30.0, 0.0, {
        let truck = truck.clone();
        let hog = hog.clone();
        let gun = gun.clone();
        let impact = impact.clone();
        let hunt = hunt.clone();
        let net = net.clone();
        move |pm| {
            let iticks = interp_ticks();
            for (peer, id) in net.owned() {
                let cmd = inputs.pop(peer);
                let Some(shooter) = truck.get_mut().get_mut(id).map(|mut t| {
                    truck_step(&mut t, cmd, FIXED_DT);
                    *t
                }) else {
                    continue;
                };

                let ready = gun.get_mut().get_mut(id).is_some_and(|mut g| {
                    *g = (*g - FIXED_DT).max(0.0);
                    let ready = cmd.fire > 0.5 && *g <= 0.0;
                    if ready {
                        *g = GUN_CD;
                    }
                    ready
                });
                if !ready {
                    continue;
                }

                // Muzzle at the truck's nose; judge against the world
                // this peer was AIMING at (their acked tick minus their
                // interp delay), not the server's present.
                let view = net.acked_tick(peer).saturating_sub(iticks);
                let (mx, mz) = {
                    let (_, front) = truck_seg(&shooter);
                    front
                };
                let hit = hist.frame(view).and_then(|f| {
                    ray_hit_hog(mx, mz, shooter.heading, &f)
                        .map(|(k, hx, hz)| (f[k].0, hx, hz))
                });
                let Some((hid, hx, hz)) = hit else {
                    continue;
                };
                // The rewound hog may have died since that frame.
                let killed = {
                    let mut hogs = hog.get_mut();
                    match hogs.get_mut(hid) {
                        Some(mut h) => {
                            h.hp -= GUN_DMG;
                            h.hp <= 0.0
                        }
                        None => continue,
                    }
                };
                if killed {
                    pm.id_remove(hid); // brain entry goes with it
                    hunt.get_mut().points += KILL_POINTS;
                }
                let mid = pm.id_add();
                impact.get_mut().add(
                    mid,
                    Impact {
                        x: hx,
                        z: hz,
                        kind: if killed { IMPACT_KILL } else { IMPACT_HIT },
                    },
                );
                if !quiet && killed {
                    eprintln!(
                        "[server] hog down: peer {peer} at ({hx:.1},{hz:.1}) view={view} now={}",
                        pm.tick()
                    );
                }
            }
        }
    });

    // Waves (prio 33): when the horde is dead, spawn a bigger one along
    // the far walls. Also keeps the scoreboard's live counters fresh.
    pm.task_add("wave", 33.0, 0.0, {
        let hog = hog.clone();
        let brain = brain.clone();
        let hunt = hunt.clone();
        move |pm| {
            if hog.get().is_empty() {
                let wave = hunt.get().wave + 1;
                let count = wave_base() + (wave - 1) * WAVE_GROW;
                let mut rng = Rng::new(pm.tick().wrapping_mul(0x9E37_79B9) | 1);
                for _ in 0..count {
                    // North, east, or west wall — never the truck spawns.
                    let along = rng.rfr(-ARENA + 3.0, ARENA - 3.0);
                    let (x, z) = match rng.next_u32() % 3 {
                        0 => (along, ARENA - 3.0),
                        1 => (ARENA - 3.0, along),
                        _ => (-ARENA + 3.0, along),
                    };
                    let id = pm.id_add();
                    hog.get_mut().add(
                        id,
                        Hog {
                            x,
                            z,
                            heading: (-x).atan2(-z),
                            speed: HOG_SLOW,
                            hp: HOG_HP,
                        },
                    );
                    brain.get_mut().add(
                        id,
                        HogBrain {
                            seed: rng.rfr(0.0, std::f32::consts::TAU),
                            ..HogBrain::default()
                        },
                    );
                }
                hunt.get_mut().wave = wave;
                if !quiet {
                    eprintln!("[server] wave {wave}: {count} hogs");
                }
            }
            let mut sb = hunt.get_mut();
            sb.alive = hog.get().len() as u32;
        }
    });

    if !quiet {
        pm.task_add("status", 90.0, 5.0, {
            let hog = hog.clone();
            let truck = truck.clone();
            let impact = impact.clone();
            let hunt = hunt.clone();
            move |pm| {
                let sb = hunt.get();
                // impacts churning 0..few proves the TTL; alive falling
                // proves shots land through the rewound frames.
                eprintln!(
                    "[server] t={} wave={} hogs={} trucks={} pts={:.0} impacts={}",
                    pm.tick() / 60,
                    sb.wave,
                    hog.get().len(),
                    truck.get().len(),
                    sb.points,
                    impact.get().len(),
                );
            }
        });
    }

    pm.loop_rate = 60;
    pm.run().unwrap_or_else(|e| {
        eprintln!("cannot serve {ADDR}: {e}");
        eprintln!("(a previous hogs may still be running: pkill -x hogs)");
        std::process::exit(1);
    });
}
