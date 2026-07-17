//! Authoritative hogs server. Trucks are player-driven and predicted;
//! hogs are server-owned NPCs stepped by an AI task — clients only ever
//! interpolate them. Bullets are real server-owned projectiles (a synced
//! pool clients render as tracers), and every tick of a bullet's flight
//! is lag-compensated the way drive's scoring is: the hit test runs
//! against the COLLIDER frame the SHOOTER was looking at (`acked_tick −
//! interp_ticks`, rewound through the collider pool's history ring —
//! hogs and teammates alike, favor-the-shooter), while damage lands in
//! the present. Collision is the docs/collisions.md architecture:
//! owners register posed collider parts, ONE sweep writes contact
//! facts, response tasks own every consequence.
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
    /// The last think's steering decision (desired heading + target
    /// speed): hogs THINK every `ai_stride`th tick (the target scan and
    /// goal trig), MOVE every tick toward this cached decision — a ≤3-
    /// tick-stale chase heading is invisible on a charging pig, and the
    /// horde stays change-dense on the wire (the workload this example
    /// exists to produce).
    desired: f32,
    target_speed: f32,
}

/// Bullet-hit knockback decay rate (1/s); the speed is `Params::knock`.
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
    /// Hit-circle padding for this shot (`HIT_PAD_*` by shooter platform).
    pad: f32,
    /// Category bits this shot tests (the query carries its filter —
    /// docs/collisions.md §2): player rounds sweep vehicles AND hogs;
    /// gunner-hog rounds sweep vehicles only (no hog-on-hog carnage).
    mask: u8,
}

/// A vehicle's registered part ids, in registration order — `ids[0]`
/// is the body part (bites test it by convention). Fixed capacity;
/// filler slots repeat the owner id and sit past `n`.
#[derive(Clone, Copy)]
struct Parts {
    ids: [Id; 4],
    n: u8,
}

/// The janitor (docs/collisions.md §3): one walk over the collider pool
/// per tick removes entries whose owner id died — the cull half of the
/// parent→child convention, `pm.id_alive` being the generation check
/// that answers "is this id current" without knowing pools. A part
/// spends at most one tick as a ghost (and one historical frame; a
/// rewound hit on it is discarded at contact-write by the same
/// `id_alive` check). Self-keyed entries (hogs) never appear here —
/// their entry dies with their entity.
fn cull_colliders(pm: &mut Pm, collider: &pm::PoolHandle<Collider>) {
    for (cid, c) in collider.get().iter() {
        if !pm.id_alive(c.owner) {
            pm.id_remove(cid); // deferred; the pool entry goes with the id
        }
    }
}

/// Register a vehicle's parts (docs/collisions.md stages 1+4): child
/// entities in the collider pool plus the parent→child link in
/// `parts` (vehicle id → part ids — keyed by the VEHICLE, so entity
/// removal cleans the link and the janitor above reaps the orphaned
/// parts). Hulls are re-posed by the drive task every tick; spawn
/// just needs them sane. A truck is one BODY entry; the heli is
/// three (stage 4 was data entry, as promised).
fn parts_add(
    pm: &mut Pm,
    collider: &pm::PoolHandle<Collider>,
    parts: &pm::PoolHandle<Parts>,
    owner: Id,
    cat: u8,
    hulls: &[(u8, Hull)],
) {
    let mut p = Parts { ids: [owner; 4], n: 0 };
    for &(part, hull) in hulls {
        let pid = pm.id_add();
        collider.get_mut().add(pid, Collider { owner, part, cat, hull });
        p.ids[p.n as usize] = pid;
        p.n += 1;
    }
    parts.get_mut().add(owner, p);
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

pub fn run(quiet: bool, init: Params, params_path: String) {
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
    // The tuning set (docs/params.md): seeded from the params file the
    // caller loaded, replicated to every client, written by the
    // "param.set" event task below. Server tasks read it where the old
    // consts used to be.
    let params = pm.sync_single::<Params>("params");
    *params.get_mut() = init;
    // Server-only state: per-hog brains, per-truck gun cooldowns,
    // per-bullet shooter info. Keyed by the same ids as their synced
    // siblings; entity removal cleans them up with everything else.
    let brain = pm.pool::<HogBrain>("hog.brain");
    let gun = pm.pool::<f32>("truck.gun");
    let shot = pm.pool::<Shot>("bullet.shot");
    // The collider pool (docs/collisions.md): one entry per PART, keyed
    // by the part's own id, posed by its owner every tick, swept by the
    // bullets task. `parts` is the parent→child link (vehicle id → its
    // part ids: one for a truck, three for a heli) so the step task can
    // find its entries. Contacts are each tick's OUTPUT — facts on
    // fresh ids, written at prios 28 (bites) and 31 (the sweep),
    // drained by the response tasks at 32 the same tick.
    let collider = pm.pool::<Collider>("collider");
    let contact = pm.pool::<Contact>("contact");
    let parts = pm.pool::<Parts>("vehicle.part");
    // Sparse gunner pool: membership IS the "armed" flag (the pm idiom
    // for "some hogs have guns"), value = refire cooldown. Keyed by the
    // hog id, so death disarms with everything else.
    let gunner = pm.pool::<f32>("hog.gunner");
    // A second of COLLIDER history — the rewind memory every shot is
    // judged in (docs/collisions.md §4): hogs and vehicles in one
    // uniform ring, favor-the-shooter, decided 2026-07-17.
    let hist = pm.history_pool(&collider, 1.0);

    if !quiet {
        eprintln!("hogs server on {ADDR} [{}]", pm::BUILD_ID);
    }
    let net = pm.net();
    let inputs = pm.input::<Drive>("drive");
    let respawns = pm.event::<Respawn>("respawn");
    let param_evs = pm.event::<ParamSet>("param.set");

    // Joins and leaves: a truck (with health, gun, and body part) per
    // peer.
    task!(pm, "roster", 10.0, [truck, health, gun, collider, parts, net], move |pm| {
        for p in net.joined() {
            let id = pm.id_add();
            let t = spawn_truck(p);
            truck.get_mut().add(id, t);
            health.get_mut().add(id, Health { hp: TRUCK_HP });
            gun.get_mut().add(id, 0.0);
            parts_add(pm, &collider, &parts, id, CAT_VEHICLE, &[(PART_BODY, truck_hull(&t))]);
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
    task!(pm, "respawn", 11.0, [truck, heli, health, gun, collider, parts, net], move |pm| {
        for (peer, ev) in respawns.drain() {
            let Some(old) = net.own(peer) else {
                continue;
            };
            // The old vehicle's part outlives this removal by one tick
            // (it has its OWN id) — the janitor in the bullets task
            // reaps it once `id_alive(owner)` fails.
            pm.id_remove(old); // truck/heli + health + gun + parts link go with it
            let id = pm.id_add();
            if ev.vehicle == VEH_HELI {
                let h = spawn_heli(peer);
                heli.get_mut().add(id, h);
                parts_add(pm, &collider, &parts, id, CAT_VEHICLE, &heli_hulls(&h));
            } else {
                let t = spawn_truck(peer);
                truck.get_mut().add(id, t);
                parts_add(pm, &collider, &parts, id, CAT_VEHICLE, &[(PART_BODY, truck_hull(&t))]);
            }
            health.get_mut().add(id, Health { hp: TRUCK_HP });
            gun.get_mut().add(id, 0.0);
            net.own_set(peer, id);
        }
    });

    // Param writes (docs/params.md): any client's telemetry knobs ride
    // the reliable "param.set" event; the server is the clamp of record.
    // The PARAM_SAVE sentinel persists the CURRENT set to this server's
    // params file instead. (`param_evs`, the path, and the send-tune
    // handle are moved in, like `respawns` above.)
    let sendtune = pm.send_tune();
    task!(pm, "params", 12.0, [params], move |_pm| {
        for (_peer, ev) in param_evs.drain() {
            if ev.idx == PARAM_SAVE {
                match params_save(&params_path, &params.get()) {
                    Ok(()) => {
                        if !quiet {
                            eprintln!("[server] params saved to {params_path}");
                        }
                    }
                    Err(e) => eprintln!("[server] params save FAILED ({params_path}): {e}"),
                }
                continue;
            }
            let Some(spec) = PARAM_SPECS.get(ev.idx as usize) else {
                continue; // stale schema on the sender: drop, don't die
            };
            let v = ev.value.clamp(spec.min, spec.max);
            let mut p = params.get_mut();
            // Write-gated single: only a real change stamps (and ships).
            if p.as_array()[ev.idx as usize] != v {
                p.as_array_mut()[ev.idx as usize] = v;
                if !quiet {
                    eprintln!("[server] param {} = {v}", spec.name);
                }
            }
        }
        // Bridge net_kbps into the engine's flight budget every tick
        // (write-gated) — this covers both the file-seeded init value
        // and live knob writes with one compare.
        let kbps = params.get().net_kbps;
        if sendtune.get().kbps != kbps {
            sendtune.get_mut().kbps = kbps;
        }
    });

    // The horde (prio 28, before the trucks move): wander until a truck
    // is in aggro range, charge it, bite on contact, break off after a
    // bite. Every hog MOVES every tick — deliberately: at horde scale
    // that makes the hog pool change-dense, which is exactly the
    // byte-budget-rotation workload this example exists to produce. But
    // hogs only THINK (target scan, steering trig, bite check) every
    // `ai_stride`th tick, in slot-staggered cohorts — the expensive
    // decision work drops to 1/stride per tick while the motion stays
    // 60 Hz smooth (this task is the sim's biggest; see the roadmap's
    // single-core watch item).
    task!(
        pm,
        "hog_ai",
        28.0,
        [hog, brain, truck, heli, collider, parts, contact, params],
        move |pm| {
            let now = pm.tick() as f32 * FIXED_DT;
            let p = *params.get(); // copy out: the each_with closure below borrows pools
            let stride = (p.ai_stride.round() as u32).clamp(1, 8);
            let phase = pm.tick() % stride;
            // Decisions cover the whole gap between thinks.
            let think_dt = stride as f32 * FIXED_DT;
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
                        .filter(|(_, hl)| hl.body.pos.y < p.hog_leap)
                        .map(|(id, hl)| (id, hl.body.pos.x, hl.body.pos.z, true)),
                )
                .collect();
            let mut hogs = hog.get_mut();
            let mut brains = brain.get_mut();
            let mut cols = collider.get_mut();
            // each_with is the hog<->brain join; bite consequences apply
            // INLINE because they only touch OTHER pools (fine while hogs
            // is borrowed) and id_add/id_remove never borrow pools at all
            // (removal is deferred to end of tick by the kernel).
            hogs.each_with(&mut brains, |id, mut h, mut b| {
                b.bite_cd = (b.bite_cd - FIXED_DT).max(0.0);
                b.flee = (b.flee - FIXED_DT).max(0.0);

                // THINK (this hog's cohort tick only): scan targets,
                // decide where to go and how fast, and check the bite.
                // The decision lands in the brain; the per-tick motion
                // below chases it.
                if id.index() % stride == phase {
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
                            ((h.x - tx).atan2(h.z - tz), p.hog_fast)
                        }
                        Some((_, (tx, tz, _), d)) if d < p.hog_aggro => {
                            ((tx - h.x).atan2(tz - h.z), p.hog_fast)
                        }
                        // Roaming: walk to a goal point, pick a fresh one
                        // on arrival or timeout — the horde spreads over
                        // the whole map instead of milling in place. The
                        // sine wobble stays so the walk reads organic.
                        _ => {
                            b.repick -= think_dt;
                            let (gx, gz) = (b.goal.0 - h.x, b.goal.1 - h.z);
                            if b.repick <= 0.0 || gx * gx + gz * gz < 9.0 {
                                b.goal = roam_goal(&mut rng);
                                b.repick = rng.rfr(0.5, 1.0) * ROAM_REPICK;
                            }
                            (gx.atan2(gz) + (now * 0.7 + b.seed).sin() * 0.4, p.hog_roam)
                        }
                    };
                    b.desired = desired;
                    b.target_speed = target_speed;

                    // Bite: contact with the nearest target while off
                    // cooldown. The GEOMETRY comes from the target's
                    // collider entry (via the parent→child link) — the
                    // brain knows no vehicle shapes anymore — and the
                    // CONSEQUENCES are the response tasks' business, so
                    // it applies none: a bite is a written fact. What
                    // stays behavioral stays here: the cooldown, the
                    // break-off, and the leap ceiling on the reach band.
                    // Think-cadence checking delays a bite ≤ stride-1
                    // ticks — noise next to BITE_CD.
                    let bitten = (b.bite_cd <= 0.0)
                        .then_some(nearest)
                        .flatten()
                        .and_then(|(tid, _, _)| {
                            let pid = parts.get_id(tid).map(|p| p.ids[0])?;
                            let c = cols.get(pid)?;
                            hull_hits_circle(&c.hull, h.x, h.z, HOG_R, 0.0, p.hog_leap)
                                .then(|| (tid, c.part, c.hull.y.0.max(0.0)))
                        });
                    if let Some((tid, part, bite_y)) = bitten {
                        b.bite_cd = p.bite_cd;
                        b.flee = p.hog_flee;
                        let cid = pm.id_add();
                        contact.get_mut().add(
                            cid,
                            Contact {
                                owner: tid,
                                part,
                                kind: KIND_BITE,
                                source_peer: 0,
                                x: h.x,
                                y: bite_y,
                                z: h.z,
                                heading: h.heading,
                            },
                        );
                    }
                }

                // MOVE (every tick): steer toward the cached decision at
                // full per-tick turn authority, then integrate.
                let turn = wrap_angle(b.desired - h.heading)
                    .clamp(-HOG_TURN * FIXED_DT, HOG_TURN * FIXED_DT);
                // Wrap at the write: the quantized wire repr saturates
                // past ±3.27 rad, and += would walk out of range circling.
                h.heading = wrap_angle(h.heading + turn);
                h.speed += (b.target_speed - h.speed) * (3.0 * FIXED_DT).min(1.0);
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

                // Pose the collider (docs/collisions.md §2): a hog is
                // its own single part — the entry is keyed by the hog's
                // OWN id, so death cleans it with the entity and the
                // janitor never has work here. Upsert every tick, the
                // pool is local — no wire cost to the change-density.
                cols.add(
                    id,
                    Collider {
                        owner: id,
                        part: PART_BODY,
                        cat: CAT_HOG,
                        hull: hog_hull(&h),
                    },
                );
            });
        }
    );

    // Gunner hogs (prio 29, after the brains have moved): each armed
    // hog fires a REAL bullet — same pool as the players', so tracers,
    // bangs, building hits, and the collider sweep all come free — at
    // the nearest vehicle inside its 3D envelope. Bad shots by design
    // (no lead, angular spread); the danger is volume: on the deck a
    // heli is inside a dozen envelopes at once, at altitude it's
    // inside none. Their rounds sweep vehicles only (`Shot::mask`) and
    // ride PRESENT-time frames — `view` starts at the current tick, so
    // the per-tick advance tracks the newest frame; a server-side
    // shooter has no lag to compensate.
    task!(pm, "hog_guns", 29.0, [hog, gunner, truck, heli, bullet, shot, params], move |pm| {
        let p = *params.get();
        let targets: Vec<(f32, f32, f32)> = truck
            .get()
            .iter()
            .map(|(_, t)| (t.body.pos.x, TRUCK_AIM_Y, t.body.pos.z))
            .chain(heli.get().iter().map(|(_, hl)| {
                let p = hl.body.pos;
                (p.x, p.y, p.z)
            }))
            .collect();
        let mut rng = Rng::new(pm.tick().wrapping_mul(0xC0FF_EE01) | 1);
        let hogs = hog.get();
        let mut guns = gunner.get_mut();
        for (id, mut cd) in guns.iter_mut() {
            *cd = (*cd - FIXED_DT).max(0.0);
            if *cd > 0.0 {
                continue;
            }
            let Some(h) = hogs.get(id) else { continue };
            let (mx, my, mz) = (h.x, HOGGUN_Y, h.z);
            let near = targets
                .iter()
                .map(|&(tx, ty, tz)| {
                    let (dx, dy, dz) = (tx - mx, ty - my, tz - mz);
                    ((tx, ty, tz), (dx * dx + dy * dy + dz * dz).sqrt())
                })
                .min_by(|a, b| a.1.total_cmp(&b.1));
            let Some(((tx, ty, tz), dist)) = near else {
                continue;
            };
            if dist > p.hoggun_range {
                continue;
            }
            let (heading, pitch) = hog_aim(mx, my, mz, tx, ty, tz);
            let bid = pm.id_add();
            bullet.get_mut().add(
                bid,
                Bullet {
                    x: mx,
                    y: my,
                    z: mz,
                    heading: wrap_angle(heading + rng.rfr(-HOGGUN_SPREAD, HOGGUN_SPREAD)),
                    pitch: wrap_angle(pitch + rng.rfr(-HOGGUN_SPREAD, HOGGUN_SPREAD)),
                    owner: 0.0, // peer 0 = the server's own trigger finger
                },
            );
            shot.get_mut().add(
                bid,
                Shot {
                    left: HOGGUN_TRAVEL,
                    view: pm.tick(),
                    pad: 0.0,
                    mask: CAT_VEHICLE,
                },
            );
            *cd = p.hoggun_cd * rng.rfr(0.75, 1.35);
        }
    });

    // Trucks + guns (prio 30): command-frame input, THE shared step, the
    // death check, and the turret. Firing just spawns a bullet at the
    // muzzle — the flight and the (lag-compensated) hit test live in the
    // bullets task below.
    task!(
        pm,
        "drive",
        30.0,
        [truck, heli, health, bullet, gun, shot, impact, hunt, collider, parts, net, params],
        move |pm| {
            let p = *params.get();
            // Owners keep their parts current (docs/collisions.md §2):
            // every exit from the step below re-poses the vehicle's
            // collider hulls, so the sweep at 31 never looks up a pose.
            // Each registered part finds its hull by tag — stage 4's
            // multi-part heli and the one-part truck share the loop.
            let pose = |id: Id, hulls: &[(u8, Hull)]| {
                let Some(p) = parts.get_id(id).map(|p| *p) else {
                    return;
                };
                for pid in &p.ids[..p.n as usize] {
                    if let Some(mut c) = collider.get_id_mut(*pid)
                        && let Some(&(_, hull)) = hulls.iter().find(|&&(tag, _)| tag == c.part)
                    {
                        c.hull = hull;
                    }
                }
            };
            for (peer, id) in net.owned() {
                let cmd = inputs.pop(peer);

                // Step whichever vehicle pool holds the avatar; each
                // branch resolves to the muzzle pose (position, yaw,
                // climb) or `continue`s on death. Death is authoritative
                // state, never predicted: bitten to 0 hp (both), or
                // boosted to 1.0 heat (trucks). Fresh vehicle at the
                // spawn slot; prediction snaps the owner home.
                let ((mx, my, mz, dir, climb), pad) = if let Some(shooter) =
                    truck.get_id_mut(id).map(|mut t| {
                        truck_step(&mut t, cmd, FIXED_DT, &p);
                        *t
                    }) {
                    let (x, z) = (shooter.body.pos.x, shooter.body.pos.z);
                    let dead =
                        shooter.heat >= 1.0 || health.get_id(id).is_some_and(|v| v.hp <= 0.0);
                    if dead {
                        let fresh = spawn_truck(peer);
                        if let Some(mut t) = truck.get_id_mut(id) {
                            *t = fresh;
                        }
                        pose(id, &[(PART_BODY, truck_hull(&fresh))]);
                        if let Some(mut v) = health.get_id_mut(id) {
                            v.hp = TRUCK_HP;
                        }
                        let mut sb = hunt.get_mut();
                        sb.points = (sb.points - p.death_cost).max(0.0);
                        drop(sb);
                        let mid = pm.id_add();
                        impact.get_mut().add(mid, Impact { x, z, kind: IMPACT_BOOM });
                        if !quiet {
                            eprintln!("[server] peer {peer} exploded at ({x:.1},{z:.1})");
                        }
                        continue;
                    }
                    pose(id, &[(PART_BODY, truck_hull(&shooter))]);
                    (truck_muzzle(&shooter), p.hit_pad_truck)
                } else if let Some(shooter) = heli.get_id_mut(id).map(|mut hl| {
                    heli_step(&mut hl, cmd, FIXED_DT, &p);
                    *hl
                }) {
                    let b = shooter.body;
                    if health.get_id(id).is_some_and(|v| v.hp <= 0.0) {
                        let fresh = spawn_heli(peer);
                        if let Some(mut hl) = heli.get_id_mut(id) {
                            *hl = fresh;
                        }
                        pose(id, &heli_hulls(&fresh));
                        if let Some(mut v) = health.get_id_mut(id) {
                            v.hp = TRUCK_HP;
                        }
                        let mut sb = hunt.get_mut();
                        sb.points = (sb.points - p.death_cost).max(0.0);
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
                    pose(id, &heli_hulls(&shooter));
                    (heli_muzzle(&shooter), p.hit_pad_heli)
                } else {
                    continue;
                };

                let ready = gun.get_id_mut(id).is_some_and(|mut g| {
                    *g = (*g - FIXED_DT).max(0.0);
                    let ready = cmd.fire > 0.5 && *g <= 0.0;
                    if ready {
                        *g = p.gun_cd;
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
                        owner: peer as f32,
                    },
                );
                shot.get_mut().add(
                    bid,
                    Shot {
                        left: p.gun_range,
                        mask: CAT_VEHICLE | CAT_HOG,
                        // The shooter's view when the trigger pulled —
                        // this bullet's whole flight is judged along the
                        // timeline that starts here (see Shot). The
                        // anchor is the FIRE INPUT's arrival-time ack
                        // (`InputRx::view`), not `net.acked_tick` at
                        // consumption — the queue makes the latter run
                        // a few ticks fresh, which is a clean miss on a
                        // charging hog.
                        view: inputs.view(peer).saturating_sub(interp_ticks()),
                        pad,
                    },
                );
            }
        }
    );

    // Bullets (prio 31, right after the guns fire): THE collisions
    // sweep (docs/collisions.md). Each bullet's per-tick travel is
    // judged in the collider frame its SHOOTER was looking at (acked
    // tick minus their interp delay, out of the history ring) — hogs
    // AND teammates in the same rewound frame, favor-the-shooter, one
    // timeline per shot. The sweep iterates collider entries — a new
    // vehicle registers its part and is swept without this task
    // changing — writes Contact facts, and applies NOTHING; the
    // response tasks below own every consequence. Buildings and arena
    // walls stop bullets in present time (they don't move).
    task!(
        pm,
        "bullets",
        31.0,
        [bullet, shot, collider, contact, impact, net, params],
        move |pm| {
            // Same-tick contract check: the response tasks at 32 drain
            // every contact each tick writes. A LAST-tick leftover
            // means a response task missed its owner — purge loudly
            // rather than let a stale fact double-apply. Same-tick
            // entries pass untouched: the hog brain writes its bites
            // at prio 28, before this task runs.
            let stale: Vec<Id> = {
                let pool = contact.get();
                let now = pm.tick();
                pool.iter()
                    .filter(|&(cid, _)| pool.changed_tick(cid) != Some(now))
                    .map(|(cid, _)| cid)
                    .collect()
            };
            if !stale.is_empty() {
                eprintln!(
                    "[server] {} undrained contacts culled (a response task missed them)",
                    stale.len()
                );
                for cid in stale {
                    pm.id_remove(cid);
                }
            }
            cull_colliders(pm, &collider);
            let step = params.get().bullet_speed * FIXED_DT;
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
                // THE sweep (docs/collisions.md §4): the shot is judged
                // in its shooter's rewound collider frame — hogs AND
                // teammates, one timeline, favor-the-shooter. Two
                // passes because the pad differs (hog forgiveness never
                // fattens a teammate); nearest hit along the travel
                // wins, so a hog can shield a truck and vice versa.
                let skip = net.own(b.owner as u8);
                let hit = hist.frame(view).and_then(|f| {
                    let ff = (s.mask & CAT_VEHICLE != 0)
                        .then(|| {
                            sweep_colliders(
                                b.x, b.z, b.y, b.heading, hstep, dy, 0.0, CAT_VEHICLE, skip, &f,
                            )
                        })
                        .flatten();
                    let hg = (s.mask & CAT_HOG != 0)
                        .then(|| {
                            sweep_colliders(
                                b.x, b.z, b.y, b.heading, hstep, dy, s.pad, CAT_HOG, None, &f,
                            )
                        })
                        .flatten();
                    [ff, hg]
                        .into_iter()
                        .flatten()
                        .min_by(|a, b| a.frac.total_cmp(&b.frac))
                });
                if let Some(hit) = hit {
                    pm.id_remove(id); // the shot ends either way
                    // A ghost in an old frame (it died since that view)
                    // eats the round — the shooter SAW it there — but
                    // hurts nothing: stale ids fail the gen check, and
                    // no contact means no response. Damage stays in the
                    // PRESENT; the response tasks re-check at drain.
                    if pm.id_alive(hit.owner) {
                        let cid = pm.id_add();
                        contact.get_mut().add(
                            cid,
                            Contact {
                                owner: hit.owner,
                                part: hit.part,
                                kind: KIND_BULLET,
                                source_peer: b.owner as u8,
                                x: hit.x,
                                y: hit.y,
                                z: hit.z,
                                heading: b.heading,
                            },
                        );
                    }
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

    // Response tasks (prio 32, right after the sweep): each vehicle
    // kind drains the contacts addressed to ITS entities and owns every
    // consequence — detection and response never meet in a function
    // again (docs/collisions.md §2). The changed-tick filter keeps a
    // response to THIS tick's facts (anything older is the sweep's
    // loud-purge path), and the owner lookup doubles as the liveness
    // check at drain (§3: validate at consumption). Today truck and
    // heli responses are twins; per-part branches (tail hit → yaw kick)
    // are exactly where they'd diverge — stage 4's business.
    task!(pm, "truck_hits", 32.0, [contact, truck, health, hunt, impact, net, params], move |pm| {
        let now = pm.tick();
        let p = *params.get();
        let hits: Vec<(Id, Contact)> = {
            let pool = contact.get();
            pool.iter()
                .filter(|&(cid, c)| {
                    pool.changed_tick(cid) == Some(now) && truck.get_id(c.owner).is_some()
                })
                .map(|(cid, c)| (cid, *c))
                .collect()
        };
        for (cid, c) in hits {
            pm.id_remove(cid); // fact consumed
            match c.kind {
                KIND_BULLET => {
                    // Gunner-hog rounds (source_peer 0) chip lighter
                    // than a teammate's cannon; only player fire earns
                    // the FF log.
                    let dmg = if c.source_peer == 0 { p.hoggun_dmg } else { p.friendly_dmg };
                    if let Some(mut v) = health.get_id_mut(c.owner) {
                        v.hp -= dmg;
                    }
                    if !quiet
                        && c.source_peer != 0
                        && let Some(victim) = net.owner_of(c.owner)
                    {
                        eprintln!(
                            "[server] friendly fire: peer {} hit peer {victim} (part {} at y {:.1})",
                            c.source_peer, c.part, c.y
                        );
                    }
                    let mid = pm.id_add();
                    impact.get_mut().add(mid, Impact { x: c.x, z: c.z, kind: IMPACT_HIT });
                }
                KIND_BITE => {
                    // The hit you feel — but not a pin: turn authority
                    // scales with speed, so scrubbing too hard leaves
                    // a swarmed truck unable to steer out at all.
                    if let Some(mut tr) = truck.get_id_mut(c.owner) {
                        tr.body.vel = tr.body.vel * 0.65;
                    }
                    // Chip the truck; the drive task turns hp 0 into
                    // the explosion (one place owns death).
                    if let Some(mut v) = health.get_id_mut(c.owner) {
                        v.hp -= p.bite_dmg;
                    }
                    let mut sb = hunt.get_mut();
                    sb.points = (sb.points - p.bite_cost).max(0.0);
                    drop(sb);
                    let mid = pm.id_add();
                    impact.get_mut().add(mid, Impact { x: c.x, z: c.z, kind: IMPACT_BITE });
                }
                _ => {}
            }
        }
    });
    task!(pm, "heli_hits", 32.0, [contact, heli, health, hunt, impact, net, params], move |pm| {
        let now = pm.tick();
        let p = *params.get();
        let hits: Vec<(Id, Contact)> = {
            let pool = contact.get();
            pool.iter()
                .filter(|&(cid, c)| {
                    pool.changed_tick(cid) == Some(now) && heli.get_id(c.owner).is_some()
                })
                .map(|(cid, c)| (cid, *c))
                .collect()
        };
        for (cid, c) in hits {
            pm.id_remove(cid);
            match c.kind {
                KIND_BULLET => {
                    // Stage 4 pays off: the part tag picks the story.
                    // Rotor strikes double the damage; boom hits
                    // glance (half) but kick the nose around.
                    let base = if c.source_peer == 0 { p.hoggun_dmg } else { p.friendly_dmg };
                    let dmg = match c.part {
                        PART_ROTOR => base * 2.0,
                        PART_TAIL => base * 0.5,
                        _ => base,
                    };
                    if let Some(mut v) = health.get_id_mut(c.owner) {
                        v.hp -= dmg;
                    }
                    // Tail kick: torque scales with obliquity (a shot
                    // down the boom's axis barely turns it). A
                    // server-side write to a predicted pod — the owner
                    // reconciles, the same seam as the bite scrub.
                    if c.part == PART_TAIL && let Some(mut hl) = heli.get_id_mut(c.owner) {
                        let b = &mut hl.body;
                        let (yaw, pitch, roll) = b.rot.to_yaw_pitch_roll();
                        let kick = p.heli_tail_kick * (yaw - c.heading).sin();
                        b.rot = pm::Quat::from_yaw_pitch_roll(wrap_angle(yaw + kick), pitch, roll)
                            .norm();
                    }
                    if !quiet
                        && c.source_peer != 0
                        && let Some(victim) = net.owner_of(c.owner)
                    {
                        eprintln!(
                            "[server] friendly fire: peer {} hit peer {victim} (part {} at y {:.1})",
                            c.source_peer, c.part, c.y
                        );
                    }
                    let mid = pm.id_add();
                    impact.get_mut().add(mid, Impact { x: c.x, z: c.z, kind: IMPACT_HIT });
                }
                KIND_BITE => {
                    // No velocity scrub — a nipped heli keeps flying;
                    // the chip is the cost of hovering in reach.
                    if let Some(mut v) = health.get_id_mut(c.owner) {
                        v.hp -= p.bite_dmg;
                    }
                    let mut sb = hunt.get_mut();
                    sb.points = (sb.points - p.bite_cost).max(0.0);
                    drop(sb);
                    let mid = pm.id_add();
                    impact.get_mut().add(mid, Impact { x: c.x, z: c.z, kind: IMPACT_BITE });
                }
                _ => {}
            }
        }
    });
    // The hogs' response: damage in the PRESENT (the frame only judged
    // the hit), the corpse guard against same-tick double kills, the
    // knockback shove, kill points, and the flash — everything the
    // bullets task used to apply inline, now behind the contact seam.
    task!(pm, "hog_hits", 32.0, [contact, hog, brain, hunt, impact, params], move |pm| {
        let now = pm.tick();
        let p = *params.get();
        let hits: Vec<(Id, Contact)> = {
            let pool = contact.get();
            pool.iter()
                .filter(|&(cid, c)| {
                    pool.changed_tick(cid) == Some(now) && hog.get_id(c.owner).is_some()
                })
                .map(|(cid, c)| (cid, *c))
                .collect()
        };
        for (cid, c) in hits {
            pm.id_remove(cid); // fact consumed
            if c.kind != KIND_BULLET {
                continue;
            }
            // The hog may have died THIS tick to an earlier bullet (hp
            // already 0, removal pending): a corpse absorbs no damage
            // and pays no double kill points — and gets no flash.
            let killed = match hog.get_id_mut(c.owner) {
                Some(mut h) if h.hp > 0.0 => {
                    h.hp -= p.gun_dmg;
                    h.hp <= 0.0
                }
                _ => continue,
            };
            // Survivors stagger away from the shot (the ragdoll for
            // the dead is client-side; see player_client).
            if !killed && let Some(mut br) = brain.get_id_mut(c.owner) {
                br.shove = (c.heading.sin() * p.knock, c.heading.cos() * p.knock);
            }
            if killed {
                pm.id_remove(c.owner); // brain + collider entries go with it
                hunt.get_mut().points += p.kill_points;
                if !quiet {
                    eprintln!("[server] hog down at ({:.1},{:.1}) now={now}", c.x, c.z);
                }
            }
            let mid = pm.id_add();
            impact.get_mut().add(
                mid,
                Impact {
                    x: c.x,
                    z: c.z,
                    kind: if killed { IMPACT_KILL } else { IMPACT_HIT },
                },
            );
        }
    });

    // Waves (prio 33): when the horde is dead, spawn a bigger one along
    // the far walls. Also keeps the scoreboard's live counters fresh.
    task!(pm, "wave", 33.0, [hog, brain, gunner, hunt, params], move |pm| {
        if hog.get().is_empty() {
            let p = *params.get();
            let wave = hunt.get().wave + 1;
            let count = (p.wave_base + (wave - 1) as f32 * p.wave_grow).round() as u32;
            let mut rng = Rng::new(pm.tick().wrapping_mul(0x9E37_79B9) | 1);
            let mut armed = 0u32;
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
                        speed: p.hog_roam,
                        hp: HOG_HP,
                    },
                );
                brain.get_mut().add(
                    id,
                    HogBrain {
                        seed: rng.rfr(0.0, std::f32::consts::TAU),
                        goal: roam_goal(&mut rng),
                        repick: rng.rfr(0.2, 1.0) * ROAM_REPICK,
                        // Match the spawn pose until the first think.
                        desired: (-x).atan2(-z),
                        target_speed: p.hog_roam,
                        ..HogBrain::default()
                    },
                );
                // A fraction of the wave spawns armed (the biomod
                // program escalates): a gunner entry with a randomized
                // first cooldown so a fresh wave never opens with a
                // synchronized volley.
                if rng.rfr(0.0, 1.0) < p.gunner_frac {
                    gunner.get_mut().add(id, rng.rfr(0.0, p.hoggun_cd));
                    armed += 1;
                }
            }
            hunt.get_mut().wave = wave;
            if !quiet {
                eprintln!("[server] wave {wave}: {count} hogs ({armed} armed)");
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

/// The parent→child lifecycle convention, pinned (docs/collisions.md
/// §3): a part has its OWN id, so removing the owner leaves a ghost
/// for exactly one flush — the janitor must filter it from the live
/// list immediately and free it next tick.
#[cfg(test)]
mod collider_tests {
    use super::*;

    const DT: f32 = 1.0 / 60.0;

    #[test]
    fn janitor_culls_ghost_parts() {
        let mut pm = Pm::new();
        let collider = pm.pool::<Collider>("collider");
        let owner = pm.id_add();
        let part = pm.id_add();
        collider.get_mut().add(
            part,
            Collider {
                owner,
                part: PART_BODY,
                cat: CAT_VEHICLE,
                hull: truck_hull(&Truck::default()),
            },
        );
        cull_colliders(&mut pm, &collider);
        pm.loop_once(DT);
        assert!(collider.get().contains(part), "a live owner's part stays");

        pm.id_remove(owner);
        pm.loop_once(DT); // flush: the owner dies, the part survives it
        assert!(
            collider.get().contains(part),
            "the part has its own id — the owner's removal can't reach it"
        );
        assert!(!pm.id_alive(owner), "the sweep's contact-write guard fails here");
        cull_colliders(&mut pm, &collider);
        pm.loop_once(DT); // flush the janitor's id_remove
        assert!(!collider.get().contains(part), "the janitor freed the entry");
        assert!(!pm.id_alive(part), "and the id itself");
    }
}
