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
    /// Knockback velocity from bullet hits — SERVER-OWNED impulse
    /// physics (physics layer 2): hogs are interp'd, never predicted,
    /// so this needs no determinism/replay story, just tick budget. It
    /// reaches clients through the hog's replicated position like any
    /// other movement.
    shove: (f32, f32),
}

/// Bullet-hit knockback speed (u/s) and its decay rate (1/s).
const KNOCK: f32 = 9.0;
const KNOCK_DECAY: f32 = 6.0;

/// Server-local per-bullet state: how much travel is left, and the
/// shooter-timeline tick the NEXT hit test rewinds to. The view is
/// captured ONCE at fire (the shooter's `acked − interp_ticks` — what
/// they saw when they pulled the trigger) and advanced by exactly one
/// per flight tick. Re-reading `acked_tick` every tick let ack
/// burstiness (acks arrive 0..3 per tick under real lag) make the
/// tested frame JUMP, and a charging hog could skip the one-tick sweep
/// window entirely — zero kills under 40 ms lag until this. A steady
/// timeline is also the honest semantics: the bullet flies through the
/// world its shooter was watching.
#[derive(Clone, Copy, Default)]
struct Shot {
    left: f32,
    view: u32,
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
    let heli = pm.sync_pool::<Heli>("heli");
    let health = pm.sync_pool::<Health>("truck.health");
    let hog = pm.wire_pool::<Hog>("hog");
    let bullet = pm.wire_pool::<Bullet>("bullet");
    let impact = pm.wire_pool::<Impact>("impact");
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

    // Respawn events: back to your spawn slot as the CHOSEN vehicle.
    // A vehicle swap must be a FRESH ENTITY: replication has no "entry
    // left this pool" message (snapshots are upserts plus ENTITY
    // removals), so pulling a live id out of the truck pool would ghost
    // that truck on every client forever. An entity IS its pool
    // membership — swap the entity, and the removal log plus the
    // ownership table tell every client the whole story. (`respawns`
    // isn't in the capture list: not a shared handle, just moved in.)
    task!(pm, "respawn", 11.0, [truck, heli, health, gun, net], move |pm| {
        for (peer, ev) in respawns.drain() {
            let Some(old) = net.own(peer) else {
                continue;
            };
            pm.id_remove(old); // truck/heli + health + gun entries go with it
            let id = pm.id_add();
            if ev.vehicle == VEH_HELI {
                heli.get_mut().add(id, spawn_heli(peer));
            } else {
                truck.get_mut().add(id, spawn_truck(peer));
            }
            health.get_mut().add(id, Health { hp: TRUCK_HP });
            gun.get_mut().add(id, 0.0);
            net.own_set(peer, id);
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
        [hog, brain, truck, heli, health, impact, hunt],
        move |pm| {
            let now = pm.tick() as f32 * FIXED_DT;
            let mut rng = Rng::new(pm.tick().wrapping_mul(0x51D7_ACE5) | 1);
            // Everything a hog can chase and bite: trucks always, helis
            // only while they hover inside leaping range — climb and the
            // horde loses you (that's the heli's whole trade).
            let targets: Vec<(Id, f32, f32, bool)> = truck
                .get()
                .iter()
                .map(|(id, t)| (id, t.body.pos.x, t.body.pos.z, false))
                .chain(
                    heli.get()
                        .iter()
                        .filter(|(_, hl)| hl.body.pos.y < HOG_LEAP)
                        .map(|(id, hl)| (id, hl.body.pos.x, hl.body.pos.z, true)),
                )
                .collect();
            let mut hogs = hog.get_mut();
            let mut brains = brain.get_mut();
            // each_with is the hog<->brain join; bite consequences apply
            // INLINE because they only touch OTHER pools (fine while hogs
            // is borrowed) and id_add/id_remove never borrow pools at all
            // (removal is deferred to end of tick by the kernel).
            hogs.each_with(&mut brains, |_id, mut h, mut b| {
                b.bite_cd = (b.bite_cd - FIXED_DT).max(0.0);
                b.flee = (b.flee - FIXED_DT).max(0.0);

                let nearest = targets
                    .iter()
                    .map(|&(tid, tx, tz, fly)| {
                        let (dx, dz) = (tx - h.x, tz - h.z);
                        (tid, (tx, tz, fly), (dx * dx + dz * dz).sqrt())
                    })
                    .min_by(|a, b| a.2.total_cmp(&b.2));

                // Pick a desired heading and speed.
                let (desired, target_speed) = match nearest {
                    Some((_, (tx, tz, _), _)) if b.flee > 0.0 => {
                        ((h.x - tx).atan2(h.z - tz), HOG_FAST)
                    }
                    Some((_, (tx, tz, _), d)) if d < HOG_AGGRO => {
                        ((tx - h.x).atan2(tz - h.z), HOG_FAST)
                    }
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
                // Wrap at the write: the quantized wire repr saturates
                // past ±3.27 rad, and += would walk out of range circling.
                h.heading = wrap_angle(h.heading + turn);
                h.speed += (target_speed - h.speed) * (3.0 * FIXED_DT).min(1.0);
                h.x += h.heading.sin() * h.speed * FIXED_DT;
                h.z += h.heading.cos() * h.speed * FIXED_DT;
                // Knockback rides on top of locomotion and decays fast —
                // a hit visibly staggers the hog without stun-locking it.
                if b.shove.0 != 0.0 || b.shove.1 != 0.0 {
                    h.x += b.shove.0 * FIXED_DT;
                    h.z += b.shove.1 * FIXED_DT;
                    let k = 1.0 - (KNOCK_DECAY * FIXED_DT).min(1.0);
                    b.shove.0 *= k;
                    b.shove.1 *= k;
                    if b.shove.0 * b.shove.0 + b.shove.1 * b.shove.1 < 0.05 {
                        b.shove = (0.0, 0.0);
                    }
                }
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

                // Bite: contact with the nearest target while off
                // cooldown — trucks as capsules, low helis as circles.
                let bites = b.bite_cd <= 0.0
                    && nearest.is_some_and(|(tid, (tx, tz, fly), _)| {
                        if fly {
                            let (dx, dz) = (tx - h.x, tz - h.z);
                            dx * dx + dz * dz < (HOG_R + HELI_R) * (HOG_R + HELI_R)
                        } else {
                            truck.get_id(tid).is_some_and(|t| hog_bites_truck(&h, &t))
                        }
                    });
                if bites {
                    let (tid, (_, _, fly), _) = nearest.unwrap();
                    b.bite_cd = BITE_CD;
                    b.flee = HOG_FLEE;
                    if !fly && let Some(mut tr) = truck.get_id_mut(tid) {
                        // The hit you feel — but not a pin: turn
                        // authority scales with speed, so scrubbing
                        // too hard leaves a swarmed truck unable to
                        // steer out at all. (Speed lives in the body's
                        // velocity now; scrubbing the vector is the
                        // same scrub.)
                        tr.body.vel = tr.body.vel * 0.65;
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
        [truck, heli, health, bullet, gun, shot, impact, hunt, net],
        move |pm| {
            for (peer, id) in net.owned() {
                let cmd = inputs.pop(peer);

                // Step whichever vehicle pool holds the avatar; each
                // branch resolves to the muzzle pose (position, yaw,
                // climb) or `continue`s on death. Death is authoritative
                // state, never predicted: bitten to 0 hp (both), or
                // boosted to 1.0 heat (trucks). Fresh vehicle at the
                // spawn slot; prediction snaps the owner home.
                let (mx, my, mz, dir, climb) = if let Some(shooter) =
                    truck.get_id_mut(id).map(|mut t| {
                        truck_step(&mut t, cmd, FIXED_DT);
                        *t
                    }) {
                    let (x, z) = (shooter.body.pos.x, shooter.body.pos.z);
                    let dead =
                        shooter.heat >= 1.0 || health.get_id(id).is_some_and(|v| v.hp <= 0.0);
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
                        impact.get_mut().add(mid, Impact { x, z, kind: IMPACT_BOOM });
                        if !quiet {
                            eprintln!("[server] peer {peer} exploded at ({x:.1},{z:.1})");
                        }
                        continue;
                    }
                    // Turret muzzle at the barrel tip: flat shot.
                    let dir = shooter.heading() + shooter.aim;
                    (x + dir.sin() * 1.9, 1.45, z + dir.cos() * 1.9, dir, 0.0)
                } else if let Some(shooter) = heli.get_id_mut(id).map(|mut hl| {
                    heli_step(&mut hl, cmd, FIXED_DT);
                    *hl
                }) {
                    let b = shooter.body;
                    if health.get_id(id).is_some_and(|v| v.hp <= 0.0) {
                        if let Some(mut hl) = heli.get_id_mut(id) {
                            *hl = spawn_heli(peer);
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
                                x: b.pos.x,
                                z: b.pos.z,
                                kind: IMPACT_BOOM,
                            },
                        );
                        if !quiet {
                            eprintln!(
                                "[server] peer {peer} downed at ({:.1},{:.1})",
                                b.pos.x, b.pos.z
                            );
                        }
                        continue;
                    }
                    // Nose gun fires where the nose points — dive to
                    // strafe the horde. Body pitch>0 = nose down, so the
                    // bullet's climb is its negation.
                    let (yaw, pitch, _) = b.rot.to_yaw_pitch_roll();
                    (
                        b.pos.x + yaw.sin() * 2.3,
                        (b.pos.y - 0.35).max(0.2),
                        b.pos.z + yaw.cos() * 2.3,
                        yaw,
                        -pitch,
                    )
                } else {
                    continue;
                };

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

                let bid = pm.id_add();
                bullet.get_mut().add(
                    bid,
                    Bullet {
                        x: mx,
                        y: my,
                        z: mz,
                        // heading + aim can exceed ±pi; wrap for the
                        // quantized wire repr (saturates past ±3.27 rad).
                        heading: wrap_angle(dir),
                        pitch: wrap_angle(climb),
                    },
                );
                shot.get_mut().add(
                    bid,
                    Shot {
                        left: GUN_RANGE,
                        // The shooter's view when the trigger pulled —
                        // this bullet's whole flight is judged along the
                        // timeline that starts here (see Shot). The
                        // anchor is the FIRE INPUT's arrival-time ack
                        // (`InputRx::view`), not `net.acked_tick` at
                        // consumption — the queue makes the latter run
                        // a few ticks fresh, which is a clean miss on a
                        // charging hog.
                        view: inputs.view(peer).saturating_sub(interp_ticks()),
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
        [bullet, shot, hog, brain, impact, hunt, net],
        move |pm| {
            let step = BULLET_SPEED * FIXED_DT;
            let mut bullets = bullet.get_mut();
            let mut shots = shot.get_mut();
            // Everything applies inline mid-iteration: id_remove is
            // DEFERRED (kernel flushes at end of tick, so the join isn't
            // invalidated) and hogs/impacts/hunt are other pools.
            bullets.each_with(&mut shots, |id, mut b, mut s| {
                // One steady tick along the shooter's timeline per
                // flight tick — never re-read from acked (bursty).
                s.view = s.view.saturating_add(1);
                let view = s.view;
                // 3D flight: the climb angle splits the tick's travel
                // into a ground-plane sweep (the existing 2D ray) and a
                // vertical component; a hit only counts if the shot's
                // altitude at the hit point is inside the hog's band.
                let hstep = b.pitch.cos() * step;
                let dy = b.pitch.sin() * step;
                let hit = hist.frame(view).and_then(|f| {
                    ray_hit_hog(b.x, b.z, b.heading, hstep, &f).and_then(|(k, hx, hz)| {
                        let (ox, oz) = (hx - b.x, hz - b.z);
                        let frac = (ox * ox + oz * oz).sqrt() / hstep.max(1e-6);
                        let yh = b.y + dy * frac;
                        (0.0..=HOG_H).contains(&yh).then(|| (f[k].0, hx, hz))
                    })
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
                    // Survivors stagger away from the shot (the ragdoll
                    // for the dead is client-side; see player_client).
                    if !killed && let Some(mut br) = brain.get_id_mut(hid) {
                        br.shove = (b.heading.sin() * KNOCK, b.heading.cos() * KNOCK);
                    }
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
                b.x += b.heading.sin() * hstep;
                b.z += b.heading.cos() * hstep;
                b.y += dy;
                s.left -= step;
                if b.y <= 0.0 || (b.y < building_top(b.x, b.z) && in_building(b.x, b.z, 0.0)) {
                    // Dirt and wall hits flash too — with real tracers,
                    // seeing WHERE the shot died is most of the feedback.
                    // Above the roofline the shot overflies the block.
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
                } else if s.left <= 0.0 || b.x.abs() > ARENA || b.z.abs() > ARENA || b.y > HELI_CEIL
                {
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
            [hog, truck, heli, bullet, impact, hunt],
            move |pm| {
                let sb = hunt.get();
                // impacts churning 0..few proves the TTL; alive falling
                // proves shots land through the rewound frames; bullets
                // churning proves entity add/remove replicates at pace.
                eprintln!(
                    "[server] t={} wave={} hogs={} trucks={} helis={} pts={:.0} impacts={} bullets={}",
                    pm.tick() / 60,
                    sb.wave,
                    hog.get().len(),
                    truck.get().len(),
                    heli.get().len(),
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
