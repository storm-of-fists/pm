//! Authoritative hogs server. Trucks are player-driven and predicted;
//! hogs are server-owned NPCs stepped by an AI task — clients only ever
//! interpolate them. Bullets are real server-owned projectiles (a synced
//! pool clients render as tracers), and every tick of a bullet's flight
//! is lag-compensated the way drive's scoring is: the hit test runs
//! against the hog frame the SHOOTER was looking at (`acked_tick −
//! interp_ticks`, rewound through the hog pool's history ring), while
//! damage lands in the present.
//!
//! Deliberate simplicities (this example is the replication stress lab,
//! not an AI showcase): hogs don't avoid each other (no separation
//! force — pm::SpatialGrid is the tool when that matters), and building
//! avoidance is push-out + slide, not pathfinding — a hog can nose a
//! wall for a moment before it swings round.

use pm::{Id, Pm, Rng, task};

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
    /// Where this hog is roaming to, and seconds until it picks anew.
    goal: (f32, f32),
    repick: f32,
}

/// Server-local per-bullet state: who fired it (that peer's latency is
/// what each hit test rewinds by — `id.peer()` would be the recycling
/// owner, not the shooter) and how much travel is left.
#[derive(Clone, Copy, Default)]
struct Shot {
    peer: u8,
    left: f32,
}

/// A roam destination: anywhere in the arena that isn't inside a
/// building (bounded retries; the fallback mid-map is never inside one).
fn roam_goal(rng: &mut Rng) -> (f32, f32) {
    for _ in 0..16 {
        let x = rng.rfr(-ARENA + 5.0, ARENA - 5.0);
        let z = rng.rfr(-ARENA + 5.0, ARENA - 5.0);
        if !in_building(x, z, HOG_R + 1.0) {
            return (x, z);
        }
    }
    (0.0, 0.0)
}

pub fn run(quiet: bool) {
    let mut pm = Pm::server(ADDR);
    // Replicated pools: trucks (predicted client-side), hogs (interp'd
    // client-side), impact markers (TTL'd transient facts). Plus the
    // co-op scoreboard as a synced single.
    let truck = pm.sync_pool::<Truck>("truck");
    let health = pm.sync_pool::<Health>("truck.health");
    let hog = pm.sync_pool::<Hog>("hog");
    let bullet = pm.sync_pool::<Bullet>("bullet");
    let impact = pm.sync_pool::<Impact>("impact");
    pm.ttl_pool(&impact, IMPACT_TTL);
    let hunt = pm.sync_single::<Hunt>("hunt");
    // A second of hog history — the rewind memory shots are judged in.
    let hist = pm.history_pool(&hog, 1.0);
    // Server-only state: per-hog brains, per-truck gun cooldowns,
    // per-bullet shooter info. Keyed by the same ids as their synced
    // siblings; entity removal cleans them up with everything else.
    let brain = pm.pool::<HogBrain>("hog.brain");
    let gun = pm.pool::<f32>("truck.gun");
    let shot = pm.pool::<Shot>("bullet.shot");

    if !quiet {
        eprintln!("hogs server on {ADDR}");
    }
    let net = pm.net();
    let inputs = pm.input::<Drive>("drive");
    let respawns = pm.event::<Respawn>("respawn");

    // Joins and leaves: a truck (with health and gun) per peer.
    task!(pm, "roster", 10.0, [truck, health, gun, net], move |pm| {
        for p in net.joined() {
            let id = pm.id_add();
            truck.get_mut().add(id, spawn_truck(p));
            health.get_mut().add(id, Health { hp: TRUCK_HP });
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
    });

    // Respawn events: flip the sender's truck back to its spawn slot,
    // vitals included. (`respawns` isn't in the capture list: not a
    // shared handle, just moved in like any other closure state.)
    task!(pm, "respawn", 11.0, [truck, health, net], move |_pm| {
        for (peer, _) in respawns.drain() {
            let Some(id) = net.own(peer) else {
                continue;
            };
            if let Some(mut t) = truck.get_id_mut(id) {
                *t = spawn_truck(peer);
            }
            if let Some(mut v) = health.get_id_mut(id) {
                v.hp = TRUCK_HP;
            }
        }
    });

    // The horde (prio 28, before the trucks move): wander until a truck
    // is in aggro range, charge it, bite on contact, break off after a
    // bite. Every hog is written every tick — deliberately: at horde
    // scale that makes the hog pool change-dense, which is exactly the
    // byte-budget-rotation workload this example exists to produce.
    task!(
        pm,
        "hog_ai",
        28.0,
        [hog, brain, truck, health, impact, hunt],
        move |pm| {
            let now = pm.tick() as f32 * FIXED_DT;
            let mut rng = Rng::new(pm.tick().wrapping_mul(0x51D7_ACE5) | 1);
            let trucks: Vec<(Id, Truck)> = truck.get().iter().map(|(id, t)| (id, *t)).collect();
            let mut hogs = hog.get_mut();
            let mut brains = brain.get_mut();
            // each_with is the hog<->brain join; bite consequences apply
            // INLINE because they only touch OTHER pools (fine while hogs
            // is borrowed) and id_add/id_remove never borrow pools at all
            // (removal is deferred to end of tick by the kernel).
            hogs.each_with(&mut brains, |_id, mut h, mut b| {
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
                    Some((_, t, _)) if b.flee > 0.0 => ((h.x - t.x).atan2(h.z - t.z), HOG_FAST),
                    Some((_, t, d)) if d < HOG_AGGRO => ((t.x - h.x).atan2(t.z - h.z), HOG_FAST),
                    // Roaming: walk to a goal point, pick a fresh one
                    // on arrival or timeout — the horde spreads over
                    // the whole map instead of milling in place. The
                    // sine wobble stays so the walk reads organic.
                    _ => {
                        b.repick -= FIXED_DT;
                        let (gx, gz) = (b.goal.0 - h.x, b.goal.1 - h.z);
                        if b.repick <= 0.0 || gx * gx + gz * gz < 9.0 {
                            b.goal = roam_goal(&mut rng);
                            b.repick = rng.rfr(0.5, 1.0) * ROAM_REPICK;
                        }
                        (gx.atan2(gz) + (now * 0.7 + b.seed).sin() * 0.4, HOG_ROAM)
                    }
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
                // Buildings: push out, then slide the heading along
                // the wall tangent (keeping the component of travel
                // that isn't INTO the wall) — cheap avoidance that
                // reads as the hog shouldering round the corner.
                let (px, pz, nx, nz) = building_push(h.x, h.z, HOG_R);
                if nx != 0.0 || nz != 0.0 {
                    h.x = px;
                    h.z = pz;
                    let (fx, fz) = (h.heading.sin(), h.heading.cos());
                    let into = fx * nx + fz * nz;
                    if into < 0.0 {
                        let (tx, tz) = (fx - nx * into, fz - nz * into);
                        if tx * tx + tz * tz > 1e-6 {
                            h.heading = tx.atan2(tz);
                        }
                    }
                }

                // Bite: contact with any truck while off cooldown.
                if b.bite_cd <= 0.0
                    && let Some((tid, t, _)) = nearest
                    && hog_bites_truck(&h, &t)
                {
                    b.bite_cd = BITE_CD;
                    b.flee = HOG_FLEE;
                    if let Some(mut tr) = truck.get_id_mut(tid) {
                        // The hit you feel — but not a pin: turn
                        // authority scales with speed, so scrubbing
                        // too hard leaves a swarmed truck unable to
                        // steer out at all.
                        tr.speed *= 0.65;
                    }
                    if let Some(mut v) = health.get_id_mut(tid) {
                        // Chip the truck; the drive task turns hp 0
                        // into the explosion (one place owns death).
                        v.hp -= BITE_DMG;
                    }
                    let mut sb = hunt.get_mut();
                    sb.points = (sb.points - BITE_COST).max(0.0);
                    drop(sb);
                    let mid = pm.id_add();
                    impact.get_mut().add(
                        mid,
                        Impact {
                            x: h.x,
                            z: h.z,
                            kind: IMPACT_BITE,
                        },
                    );
                }
            });
        }
    );

    // Trucks + guns (prio 30): command-frame input, THE shared step, the
    // death check, and the turret. Firing just spawns a bullet at the
    // muzzle — the flight and the (lag-compensated) hit test live in the
    // bullets task below.
    task!(
        pm,
        "drive",
        30.0,
        [truck, health, bullet, gun, shot, impact, hunt, net],
        move |pm| {
            for (peer, id) in net.owned() {
                let cmd = inputs.pop(peer);
                let Some(shooter) = truck.get_id_mut(id).map(|mut t| {
                    truck_step(&mut t, cmd, FIXED_DT);
                    *t
                }) else {
                    continue;
                };

                // Death: bitten to 0 hp, or boosted to 1.0 heat. Clients
                // predict the heat climbing but never the boom — that's
                // authoritative state, like all damage. Fresh truck at
                // the spawn slot; prediction snaps the owner home.
                let dead = shooter.heat >= 1.0 || health.get_id(id).is_some_and(|v| v.hp <= 0.0);
                if dead {
                    if let Some(mut t) = truck.get_id_mut(id) {
                        *t = spawn_truck(peer);
                    }
                    if let Some(mut v) = health.get_id_mut(id) {
                        v.hp = TRUCK_HP;
                    }
                    let mut sb = hunt.get_mut();
                    sb.points = (sb.points - DEATH_COST).max(0.0);
                    drop(sb);
                    let mid = pm.id_add();
                    impact.get_mut().add(
                        mid,
                        Impact {
                            x: shooter.x,
                            z: shooter.z,
                            kind: IMPACT_BOOM,
                        },
                    );
                    if !quiet {
                        eprintln!(
                            "[server] peer {peer} exploded at ({:.1},{:.1})",
                            shooter.x, shooter.z
                        );
                    }
                    continue;
                }

                let ready = gun.get_id_mut(id).is_some_and(|mut g| {
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

                // Muzzle at the barrel tip, along the turret direction.
                let dir = shooter.heading + shooter.aim;
                let bid = pm.id_add();
                bullet.get_mut().add(
                    bid,
                    Bullet {
                        x: shooter.x + dir.sin() * 1.9,
                        z: shooter.z + dir.cos() * 1.9,
                        heading: dir,
                    },
                );
                shot.get_mut().add(
                    bid,
                    Shot {
                        peer,
                        left: GUN_RANGE,
                    },
                );
            }
        }
    );

    // Bullets (prio 31, right after the guns fire): sweep each bullet's
    // per-tick travel against the hog frame its SHOOTER was looking at
    // (acked tick minus their interp delay, out of the history ring) —
    // per-tick lag comp, so leading a hog is never required at sane
    // latencies. Damage still lands on the PRESENT hog; buildings and
    // arena walls stop bullets in present time (they don't move).
    task!(
        pm,
        "bullets",
        31.0,
        [bullet, shot, hog, impact, hunt, net],
        move |pm| {
            let iticks = interp_ticks();
            let step = BULLET_SPEED * FIXED_DT;
            let mut bullets = bullet.get_mut();
            let mut shots = shot.get_mut();
            // Everything applies inline mid-iteration: id_remove is
            // DEFERRED (kernel flushes at end of tick, so the join isn't
            // invalidated) and hogs/impacts/hunt are other pools.
            bullets.each_with(&mut shots, |id, mut b, mut s| {
                let view = net.acked_tick(s.peer).saturating_sub(iticks);
                let hit = hist.frame(view).and_then(|f| {
                    ray_hit_hog(b.x, b.z, b.heading, step, &f).map(|(k, hx, hz)| (f[k].0, hx, hz))
                });
                if let Some((hid, hx, hz)) = hit {
                    pm.id_remove(id); // shot entry goes with it
                    // The rewound hog may have died since that frame —
                    // or THIS tick, to an earlier bullet (hp already 0,
                    // removal pending): a corpse absorbs no damage and
                    // pays no double kill points.
                    let killed = match hog.get_id_mut(hid) {
                        Some(mut h) if h.hp > 0.0 => {
                            h.hp -= GUN_DMG;
                            h.hp <= 0.0
                        }
                        _ => return,
                    };
                    if killed {
                        pm.id_remove(hid); // brain entry goes with it
                        hunt.get_mut().points += KILL_POINTS;
                        if !quiet {
                            eprintln!("[server] hog down at ({hx:.1},{hz:.1}) now={}", pm.tick());
                        }
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
                    return;
                }
                b.x += b.heading.sin() * step;
                b.z += b.heading.cos() * step;
                s.left -= step;
                if in_building(b.x, b.z, 0.0) {
                    // Wall hits flash too — with real tracers, seeing
                    // WHERE the shot died is most of the feedback.
                    pm.id_remove(id);
                    let mid = pm.id_add();
                    impact.get_mut().add(
                        mid,
                        Impact {
                            x: b.x,
                            z: b.z,
                            kind: IMPACT_HIT,
                        },
                    );
                } else if s.left <= 0.0 || b.x.abs() > ARENA || b.z.abs() > ARENA {
                    pm.id_remove(id);
                }
            });
        }
    );

    // Waves (prio 33): when the horde is dead, spawn a bigger one along
    // the far walls. Also keeps the scoreboard's live counters fresh.
    task!(pm, "wave", 33.0, [hog, brain, hunt], move |pm| {
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
                        speed: HOG_ROAM,
                        hp: HOG_HP,
                    },
                );
                brain.get_mut().add(
                    id,
                    HogBrain {
                        seed: rng.rfr(0.0, std::f32::consts::TAU),
                        goal: roam_goal(&mut rng),
                        repick: rng.rfr(0.2, 1.0) * ROAM_REPICK,
                        ..HogBrain::default()
                    },
                );
            }
            hunt.get_mut().wave = wave;
            if !quiet {
                eprintln!("[server] wave {wave}: {count} hogs");
            }
        }
        // Write-gated single: reading `alive` through the guard is
        // free; only an actual change stamps it — a quiet scoreboard
        // stays off the wire.
        let mut sb = hunt.get_mut();
        let alive = hog.get().len() as u32;
        if sb.alive != alive {
            sb.alive = alive;
        }
    });

    if !quiet {
        task!(
            pm,
            "status",
            90.0,
            5.0,
            [hog, truck, bullet, impact, hunt],
            move |pm| {
                let sb = hunt.get();
                // impacts churning 0..few proves the TTL; alive falling
                // proves shots land through the rewound frames; bullets
                // churning proves entity add/remove replicates at pace.
                eprintln!(
                    "[server] t={} wave={} hogs={} trucks={} pts={:.0} impacts={} bullets={}",
                    pm.tick() / 60,
                    sb.wave,
                    hog.get().len(),
                    truck.get().len(),
                    sb.points,
                    impact.get().len(),
                    bullet.get().len(),
                );
            }
        );
    }

    // PM_PROF=1: per-task cycle times every 5 s — the stress lab should
    // answer "where does the tick go?" without a profiler attached.
    if std::env::var("PM_PROF").is_ok() {
        let mut prev: std::collections::HashMap<String, pm::TaskStat> = Default::default();
        task!(pm, "prof", 91.0, 5.0, [], move |pm| {
            eprintln!("-- server task stats (last 5s) --");
            let mut tick_total = 0.0f32;
            for (name, s) in pm.task_stats() {
                let p = prev.get(&name).cloned().unwrap_or_default();
                let calls = s.calls - p.calls;
                if calls > 0 {
                    let avg_us = (s.ns_total - p.ns_total) as f32 / calls as f32 / 1000.0;
                    // Interval tasks amortize over the window's ~300 ticks.
                    tick_total += (s.ns_total - p.ns_total) as f32 / 300.0 / 1000.0;
                    eprintln!(
                        "  {name:<12} {avg_us:>8.1} us/call  {calls:>5} calls  max {:>8.1} us",
                        s.ns_max as f32 / 1000.0
                    );
                }
                prev.insert(name, s);
            }
            eprintln!("  ~{tick_total:.0} us/tick of the 16667 us budget");
        });
    }

    pm.loop_rate = 60;
    pm.run().unwrap_or_else(|e| {
        eprintln!("cannot serve {ADDR}: {e}");
        eprintln!("(a previous hogs may still be running: pkill -x hogs)");
        std::process::exit(1);
    });
}
