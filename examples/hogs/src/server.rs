//! Authoritative hogs server. Trucks are player-driven and predicted;
//! hogs — ground horde and winged flyers alike — are server-owned NPCs
//! stepped by AI tasks; clients only ever interpolate them. Bullets are real server-owned projectiles (a synced
//! pool clients render as tracers), and every tick of a bullet's flight
//! is lag-compensated the way drive's scoring is: the hit test runs
//! against the COLLIDER frame the SHOOTER was looking at (`acked_tick −
//! interp_ticks`, rewound through the collider pool's history ring —
//! hogs and teammates alike, favor-the-shooter), while damage lands in
//! the present. Collision is the collider-pool architecture (rustdoc
//! on common.rs `Collider`/`Contact`/`Parts`/`WorldIndex` is the
//! design record; this file's `bullets` task holds the lag-comp half):
//! owners register posed collider parts, ONE sweep writes contact
//! facts, response tasks own every consequence.
//!
//! Deliberate simplicities (this example is the replication stress lab,
//! not an AI showcase): hogs don't avoid each other (no separation
//! force — pm::SpatialGrid is the tool when that matters), and building
//! avoidance is push-out + slide, not pathfinding — a hog can nose a
//! wall for a moment before it swings round.

// TODO(roadmap): named phase constants instead of the float-literal
// task priorities scattered below (27 index / 28 bites / 31 sweep /
// 32 drain / 33 director / 95 telemetry…) — the same-tick contact
// contract already has a runtime guard; the numbers deserve names.

use std::collections::HashMap;

use pm::{Id, Pm, Rng, task};

use crate::common::*;
use crate::models::{Models, posed};

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

// TODO(refactor): FlyerBrain = HogBrain + target_alt, and flyer_ai /
// flyer_hits are near-verbatim clones of hog_ai / hog_hits (~200
// lines) — extract shared NPC verbs (cooldown tick, wander, steer-and-
// move, shove decay, wall clamp, building slide, bite-via-collider) so
// each AI task shrinks to ~40 lines of composition and the flyer's
// genuinely-3D lines stay visible.
/// Server-local per-flyer state — the airborne sibling of [`HogBrain`]:
/// same think/move split and wander machinery, plus the altitude the
/// per-tick move chases at `p.flyer_climb` authority (a swoop IS
/// `target_alt` dropping to a truck's deck).
#[derive(Clone, Copy, Default)]
struct FlyerBrain {
    bite_cd: f32,
    flee: f32,
    seed: f32,
    goal: (f32, f32),
    repick: f32,
    /// Knockback stays horizontal; the vertical drama on death is the
    /// client's falling ragdoll.
    shove: (f32, f32),
    desired: f32,
    target_speed: f32,
    target_alt: f32,
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
    /// `Collider::cat`'s contract): player rounds sweep vehicles AND hogs;
    /// gunner-hog rounds sweep vehicles only (no hog-on-hog carnage).
    mask: u8,
}

/// The janitor (the `Parts` lifecycle convention): one walk over the collider pool
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

/// Spawn one wave along the north/east/west walls (never the south
/// truck spawns): the classic mix — a winged slice upstairs, a gunner
/// slice armed, the rest ground horde. The director owns WHEN and how
/// big; this owns the entities.
#[allow(clippy::too_many_arguments)]
fn spawn_wave(
    pm: &mut Pm,
    hog: &pm::PoolHandle<Hog>,
    brain: &pm::PoolHandle<HogBrain>,
    gunner: &pm::PoolHandle<f32>,
    flyer: &pm::PoolHandle<Flyer>,
    fbrain: &pm::PoolHandle<FlyerBrain>,
    p: &Params,
    count: u32,
    wave: u32,
    quiet: bool,
) {
    let mut rng = Rng::new(pm.tick().wrapping_mul(0x9E37_79B9) | 1);
    let mut armed = 0u32;
    let mut winged = 0u32;
    for _ in 0..count {
        let along = rng.rfr(-ARENA + 3.0, ARENA - 3.0);
        let (x, z) = match rng.next_u32() % 3 {
            0 => (along, ARENA - 3.0),
            1 => (ARENA - 3.0, along),
            _ => (-ARENA + 3.0, along),
        };
        let id = pm.id_add();
        // The winged slice takes the fight upstairs; the rest is the
        // classic ground horde.
        if rng.rfr(0.0, 1.0) < p.flyer_frac {
            flyer.get_mut().add(
                id,
                Flyer {
                    x,
                    y: p.flyer_alt,
                    z,
                    heading: (-x).atan2(-z),
                    speed: p.flyer_speed * 0.55,
                    hp: p.flyer_hp,
                },
            );
            fbrain.get_mut().add(
                id,
                FlyerBrain {
                    seed: rng.rfr(0.0, std::f32::consts::TAU),
                    goal: roam_goal(&mut rng),
                    repick: rng.rfr(0.2, 1.0) * p.roam_repick,
                    desired: (-x).atan2(-z),
                    target_speed: p.flyer_speed * 0.55,
                    target_alt: p.flyer_alt,
                    ..FlyerBrain::default()
                },
            );
            winged += 1;
            continue;
        }
        hog.get_mut().add(
            id,
            Hog {
                x,
                z,
                heading: (-x).atan2(-z),
                speed: p.hog_roam,
                hp: p.hog_hp,
            },
        );
        brain.get_mut().add(
            id,
            HogBrain {
                seed: rng.rfr(0.0, std::f32::consts::TAU),
                goal: roam_goal(&mut rng),
                repick: rng.rfr(0.2, 1.0) * p.roam_repick,
                // Match the spawn pose until the first think.
                desired: (-x).atan2(-z),
                target_speed: p.hog_roam,
                ..HogBrain::default()
            },
        );
        // A fraction of the wave spawns armed (the biomod program
        // escalates): a gunner entry with a randomized first cooldown
        // so a fresh wave never opens with a synchronized volley.
        if rng.rfr(0.0, 1.0) < p.gunner_frac {
            gunner.get_mut().add(id, rng.rfr(0.0, p.hoggun_cd));
            armed += 1;
        }
    }
    if !quiet {
        eprintln!("[server] wave {wave}: {count} hogs ({armed} armed, {winged} winged)");
    }
}

/// The boss: one hog off the north wall wearing the [`Boss`] marker —
/// the hog AI drives it (fear bred out; see `hog_ai`), `hog_hits`
/// lands damage on the marker's big hp, and the director reads the
/// marker's absence as the win.
fn spawn_boss(
    pm: &mut Pm,
    hog: &pm::PoolHandle<Hog>,
    brain: &pm::PoolHandle<HogBrain>,
    boss: &pm::PoolHandle<Boss>,
    p: &Params,
) {
    let mut rng = Rng::new(pm.tick().wrapping_mul(0xB055_B055) | 1);
    let (x, z) = (0.0, ARENA - 8.0);
    let id = pm.id_add();
    hog.get_mut().add(
        id,
        Hog { x, z, heading: (-x).atan2(-z), speed: p.hog_roam, hp: p.hog_hp },
    );
    brain.get_mut().add(
        id,
        HogBrain {
            seed: rng.rfr(0.0, std::f32::consts::TAU),
            goal: roam_goal(&mut rng),
            repick: p.roam_repick,
            desired: (-x).atan2(-z),
            target_speed: p.hog_fast,
            ..HogBrain::default()
        },
    );
    boss.get_mut().add(id, Boss { hp: p.boss_hp });
}

/// Clear the field between missions: horde, flock, and the depot (its
/// self-keyed collider entry dies with the id). Bullets and impacts
/// expire on their own clocks; vehicles are the players', untouched.
fn purge_npcs(
    pm: &mut Pm,
    hog: &pm::PoolHandle<Hog>,
    flyer: &pm::PoolHandle<Flyer>,
    depot: &pm::PoolHandle<Depot>,
) {
    let ids: Vec<Id> = hog
        .get()
        .iter()
        .map(|(i, _)| i)
        .chain(flyer.get().iter().map(|(i, _)| i))
        .chain(depot.get().iter().map(|(i, _)| i))
        .collect();
    for id in ids {
        pm.id_remove(id);
    }
}

/// Point the `Hunt` single at a mission's briefing screen — the one
/// transition every arc edge (join, mission complete, retry, next
/// level) goes through.
fn enter_brief(sb: &mut Hunt, level: u32, mission: u32, p: &Params) {
    let m = mission_def(level, mission);
    sb.phase = PHASE_BRIEF;
    sb.level = level;
    sb.mission = mission;
    sb.kind = m.kind;
    sb.goal = m.goal;
    sb.done = 0;
    sb.wave = 0;
    sb.timer = p.brief_secs;
}

/// `addr` is the BIND address (loopback by default; `0.0.0.0:PORT` to
/// host for the outside world); `password`, when set, locks the session
/// — the transport bounces wrong or missing passwords before they ever
/// reach the roster (see `PmServer::password`).
pub fn run(
    quiet: bool,
    params_path: String,
    addr: &str,
    password: Option<String>,
    record: Option<String>,
    prof: bool,
) {
    let mut pm = Pm::server(addr);
    if let Some(pw) = &password {
        pm.password(pw);
    }
    // `record=FILE`: write this session as a keyframe + per-tick
    // deltas (the wire format is the demo format); watch it later with
    // `hogs replay=FILE`.
    if let Some(path) = &record {
        pm.record_to(path);
    }
    // THE params seam (engine-hosted, 2026-07-23): params file →
    // replicated "pm.params" single → clamp-of-record over the
    // "pm.param.set" event (+ save sentinel). `init` = the loaded set,
    // for the creation-frozen engine knobs below.
    let params = pm.params::<Params>(&params_path);
    let init = *params.get();
    // Reconnect: the engine parks a dropped session's PEER ID this long
    // (same token → same id back); the roster parks the VEHICLE for the
    // same window. One knob, both halves.
    pm.reconnect_grace(init.reconnect_grace);
    // Replicated pools: trucks (predicted client-side), hogs (interp'd
    // client-side), impact markers (TTL'd transient facts). Plus the
    // co-op scoreboard as a synced single.
    let truck = pm.sync_pool::<Truck>("truck");
    let heli = pm.sync_pool::<Heli>("heli");
    let health = pm.sync_pool::<Health>("truck.health");
    let hog = pm.wire_pool::<Hog>("hog");
    let flyer = pm.wire_pool::<Flyer>("flyer");
    let bullet = pm.wire_pool::<Bullet>("bullet");
    let impact = pm.wire_pool::<Impact>("impact");
    pm.ttl_pool(&impact, init.impact_ttl);
    // Mission furniture: the DEFEND objective (one entry while a defend
    // mission runs) and the boss marker (keyed by the boss's hog id —
    // real hp lives here, `Hog::hp` mirrors the fraction). Both owned
    // by the director task at the bottom of this fn.
    let depot = pm.sync_pool::<Depot>("depot");
    let boss = pm.sync_pool::<Boss>("boss");
    let hunt = pm.sync_single::<Hunt>("hunt");
    // The tuning set: seeded from the params file the
    // caller loaded, replicated to every client, written by the
    // "param.set" event task below. Server tasks read it where the old
    // consts used to be.
    // The models registry (models.rs): kind-level shape data. The
    // server's interest is the `collide.*` protos it poses into the
    // collider pool — hitboxes come from the same .glb the clients
    // draw (LOCAL single, never synced: each side reads its own file;
    // the server's copy is simply the authoritative one).
    let models = pm.single::<Models>("models");
    *models.get_mut() = Models::load();
    // Server-only state: per-hog brains, per-truck gun cooldowns,
    // per-bullet shooter info. Keyed by the same ids as their synced
    // siblings; entity removal cleans them up with everything else.
    let brain = pm.pool::<HogBrain>("hog.brain");
    let fbrain = pm.pool::<FlyerBrain>("flyer.brain");
    let gun = pm.pool::<f32>("truck.gun");
    let shot = pm.pool::<Shot>("bullet.shot");
    // The collider pool (rustdoc on `Collider` is the design record):
    // one entry per PART, keyed
    // by the part's own id, posed by its owner every tick, swept by the
    // bullets task. `parts` is the parent→child link (vehicle id → its
    // part ids: one for a truck, three for a heli) so the step task can
    // find its entries. Contacts are each tick's OUTPUT — facts on
    // fresh ids, written at prios 28 (bites) and 31 (the sweep),
    // drained by the response tasks at 32 the same tick.
    let collider = pm.pool::<Collider>("collider");
    let contact = pm.pool::<Contact>("contact");
    let parts = pm.pool::<Parts>("vehicle.part");
    // THE query seam (common.rs `WorldIndex`, whose rustdoc is the record):
    // the collider pool mirrored into a pm::DynamicTree, asked by every
    // targeting/bite question below. LOCAL single — derived state,
    // never on the wire.
    let index = pm.single::<WorldIndex>("collider.index");
    // Sparse gunner pool: membership IS the "armed" flag (the pm idiom
    // for "some hogs have guns"), value = refire cooldown. Keyed by the
    // hog id, so death disarms with everything else.
    let gunner = pm.pool::<f32>("hog.gunner");
    // A second of COLLIDER history — the rewind memory every shot is
    // judged in (the bullets task is the lag-comp record): hogs and vehicles in one
    // uniform ring, favor-the-shooter, decided 2026-07-17.
    let hist = pm.journal_pool(&collider, 1.0);

    if !quiet {
        eprintln!(
            "hogs server on {addr}{} [{}]",
            if password.is_some() { " (password locked)" } else { "" },
            pm::BUILD_ID
        );
    }
    let net = pm.net();

    // INTEREST (v2 item 4): the change-dense pools fill each peer's
    // snapshot budget nearest-their-vehicle-first. The horde re-dirties
    // every tick, so under budget pressure the far side of the arena
    // used to steal bytes from the hog about to bite you; now distance
    // decides the order and the engine's staleness multiplier keeps the
    // far side flowing at a lower cadence instead of never (the
    // starvation lesson, wearing its seatbelt). Scorers run inside the
    // net task's pack — they read the vehicle pools through handles,
    // never the pool being scored.
    {
        let avatar_xz = {
            let (net, truck, heli) = (net.clone(), truck.clone(), heli.clone());
            move |peer: u8| -> Option<(f32, f32)> {
                let id = net.own(peer)?;
                truck
                    .get()
                    .get(id)
                    .map(|t| (t.body.pos.x, t.body.pos.z))
                    .or_else(|| heli.get().get(id).map(|h| (h.body.pos.x, h.body.pos.z)))
            }
        };
        // VIEW-POSE refinement (item 4 stage 2): a GUI client reports
        // its camera every tick (`ClientNet::view_set`), so its score
        // measures distance from the EYE and multiplies in a forward
        // cone — on-screen hogs first, the swarm behind you at 1/3
        // cadence (never zero: the engine's staleness multiplier still
        // carries it). Bots report nothing and keep vehicle distance.
        let vnet = net.clone();
        let near = move |peer: u8, x: f32, z: f32| {
            if let Some((eye, fwd)) = vnet.view_pose(peer) {
                let (dx, dz) = (x - eye.x, z - eye.z);
                let d2 = dx * dx + dz * dz;
                let ahead = (dx * fwd.x + dz * fwd.z) / d2.sqrt().max(1e-3);
                return (0.33 + 0.67 * ahead.max(0.0)) / (1.0 + d2);
            }
            let Some((px, pz)) = avatar_xz(peer) else {
                return 1.0; // no avatar (spectating a wipe): flat order
            };
            let (dx, dz) = (x - px, z - pz);
            1.0 / (1.0 + dx * dx + dz * dz)
        };
        let n = near.clone();
        pm.interest_pool(&hog, move |p, _, h: &Hog| n(p, h.x, h.z));
        let n = near.clone();
        pm.interest_pool(&flyer, move |p, _, f: &Flyer| n(p, f.x, f.z));
        pm.interest_pool(&bullet, move |p, _, b: &Bullet| near(p, b.x, b.z));
    }

    let inputs = pm.input::<Drive>("drive");
    let respawns = pm.event::<Respawn>("respawn");
    let sessions = pm.event::<Session>("session");

    // TODO(refactor): vehicle spawn assembly (pool entry + health + gun
    // + parts + pose + own_set) is hand-rolled 3× (roster, respawn, the
    // drive task's two death branches), and death consequences (fresh
    // spawn + death_cost + boom impact + log) are twins in drive —
    // extract spawn_vehicle() / vehicle_died() helpers.
    // Joins and leaves: a truck (with health, gun, and body part) per
    // peer. A LEAVE parks the vehicle instead of deleting it (the v2
    // reconnect seam): the entity stays in the world — hittable,
    // biteable, exactly where the drop left it — and a rejoin inside
    // the grace window re-adopts it (the engine hands the same peer id
    // back to the same session token), hp and heat intact. A vehicle
    // that died while parked re-adopts dead and the drive task's death
    // branch respawns it fresh, the normal way. Grace expiry finally
    // removes the wreck. Lefts run BEFORE joins: the engine orders a
    // same-tick reclaim leave-first.
    let mut parked: HashMap<u8, (Id, u32)> = HashMap::new();
    task!(pm, "roster", 10.0, [truck, health, gun, collider, parts, net, models, params], move |pm| {
        let grace = params.get().reconnect_grace;
        for p in net.left() {
            if let Some(id) = net.own(p) {
                parked.insert(p, (id, pm.tick()));
                if !quiet {
                    eprintln!(
                        "[server] peer {p} left — vehicle parked {grace:.0}s for reconnect"
                    );
                }
            } else if !quiet {
                eprintln!("[server] peer {p} left");
            }
        }
        for p in net.joined() {
            if net.own(p).is_some() {
                // Quiet supersede: the same session redialed before the
                // old connection was declared dead — ownership never
                // lapsed, the vehicle never stopped being theirs.
                if !quiet {
                    eprintln!("[server] peer {p} reconnected in place");
                }
                continue;
            }
            if let Some((id, _)) = parked.remove(&p)
                && pm.id_alive(id)
            {
                net.own_set(p, id);
                if !quiet {
                    eprintln!("[server] peer {p} reconnected — vehicle restored");
                }
                continue;
            }
            let id = pm.id_add();
            let t = spawn_truck(p);
            truck.get_mut().add(id, t);
            health.get_mut().add(id, Health { hp: params.get().truck_hp });
            gun.get_mut().add(id, 0.0);
            let hulls =
                posed(models.get().protos("truck"), t.body.pos.x, 0.0, t.body.pos.z, t.heading());
            parts_add(pm, &collider, &parts, id, CAT_VEHICLE, &hulls);
            net.own_set(p, id);
            if !quiet {
                eprintln!("[server] peer {p} joined");
            }
        }
        // Nobody came back for these: clear the wrecks.
        let now = pm.tick();
        let grace_ticks = (grace / FIXED_DT) as u32;
        parked.retain(|p, &mut (id, since)| {
            if now.saturating_sub(since) <= grace_ticks {
                return pm.id_alive(id); // a dead parked vehicle needs no keeper
            }
            if pm.id_alive(id) {
                pm.id_remove(id);
                if !quiet {
                    eprintln!("[server] peer {p}'s parked vehicle expired");
                }
            }
            false
        });
    });

    // Respawn events: back to your spawn slot as the CHOSEN vehicle.
    // A vehicle swap must be a FRESH ENTITY: replication has no "entry
    // left this pool" message (snapshots are upserts plus ENTITY
    // removals), so pulling a live id out of the truck pool would ghost
    // that truck on every client forever. An entity IS its pool
    // membership — swap the entity, and the removal log plus the
    // ownership table tell every client the whole story. (`respawns`
    // isn't in the capture list: not a shared handle, just moved in.)
    task!(pm, "respawn", 11.0, [truck, heli, health, gun, collider, parts, net, models, params], move |pm| {
        let p = *params.get();
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
                let h = spawn_heli(peer, &p);
                heli.get_mut().add(id, h);
                let b = h.body;
                let hulls =
                    posed(models.get().protos("heli"), b.pos.x, b.pos.y, b.pos.z, b.yaw());
                parts_add(pm, &collider, &parts, id, CAT_VEHICLE, &hulls);
            } else {
                let t = spawn_truck(peer);
                truck.get_mut().add(id, t);
                let hulls = posed(
                    models.get().protos("truck"),
                    t.body.pos.x,
                    0.0,
                    t.body.pos.z,
                    t.heading(),
                );
                parts_add(pm, &collider, &parts, id, CAT_VEHICLE, &hulls);
            }
            health.get_mut().add(id, Health { hp: p.truck_hp });
            gun.get_mut().add(id, 0.0);
            net.own_set(peer, id);
        }
    });

    // Param writes: any client's telemetry knobs ride
    // the reliable "param.set" event; the server is the clamp of record.
    // The PARAM_SAVE sentinel persists the CURRENT set to this server's
    // params file instead. (`param_evs`, the path, and the send-tune
    // handle are moved in, like `respawns` above.)
    let sendtune = pm.send_tune();
    // Bridge net_kbps into the engine's flight budget every tick
    // (write-gated) — covers both the file-seeded value and live knob
    // writes with one compare. (The rest of the old params task — the
    // clamp of record, the save sentinel — is ENGINE now:
    // `PmServer::params`.)
    task!(pm, "sendtune", 12.0, [params], move |_pm| {
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
    // The index sync (prio 27, FIRST in the frame's sim block): mirror
    // the collider pool into the query tree before anyone asks it.
    // Owners posed their hulls at 28-30 LAST tick and deferred removals
    // flushed at tick end, so this snapshot is exactly the world the
    // old direct pool reads saw — one tick behind the poses this tick
    // will write, which is the staleness targeting always had.
    task!(pm, "index", 27.0, [collider, index], move |_pm| {
        index.get_mut().sync(&collider.get());
    });

    // hogs only THINK (target scan, steering trig, bite check) every
    // `ai_stride`th tick, in slot-staggered cohorts — the expensive
    // decision work drops to 1/stride per tick while the motion stays
    // 60 Hz smooth (this task is the sim's biggest).
    // TODO(roadmap): the single-core sim ceiling watch item — stride
    // bought the headroom; re-profile before reaching for the parked
    // opt-in threading design. Threading is still the eventual answer
    // if hordes grow 10x.
    task!(
        pm,
        "hog_ai",
        28.0,
        [hog, brain, boss, index, collider, contact, params, models],
        move |pm| {
            let now = pm.tick() as f32 * FIXED_DT;
            let p = *params.get(); // copy out: the each_with closure below borrows pools
            let stride = (p.ai_stride.round() as u32).clamp(1, 8);
            let phase = pm.tick() % stride;
            // Decisions cover the whole gap between thinks.
            let think_dt = stride as f32 * FIXED_DT;
            let mut rng = Rng::new(pm.tick().wrapping_mul(0x51D7_ACE5) | 1);
            // Everything a hog can chase IS everything it could bite:
            // the query band `[0, hog_leap]` — helis that climb past
            // leaping range stop existing for the horde (that's the
            // heli's whole trade), and a new vehicle kind is chaseable
            // the moment it registers collider parts.
            let idx = index.get();
            // The kind's hitbox, poseable (Proto is Copy — the guard
            // drops before the pool borrows below).
            let hog_proto = models.get().protos("hog")[0];
            let mut hogs = hog.get_mut();
            let mut brains = brain.get_mut();
            let mut cols = collider.get_mut();
            // each_with is the hog<->brain join; bite consequences apply
            // INLINE because they only touch OTHER pools (fine while hogs
            // is borrowed) and id_add/id_remove never borrow pools at all
            // (removal is deferred to end of tick by the kernel).
            hogs.each_with(&mut brains, |id, mut h, mut b| {
                // The boss is a hog with the fear bred out: aggro at
                // any range, never breaks off, and wears a grown hull.
                let is_boss = boss.get_id(id).is_some();
                b.bite_cd = (b.bite_cd - FIXED_DT).max(0.0);
                b.flee = (b.flee - FIXED_DT).max(0.0);

                // THINK (this hog's cohort tick only): scan targets,
                // decide where to go and how fast, and check the bite.
                // The decision lands in the brain; the per-tick motion
                // below chases it.
                if id.index() % stride == phase {
                    let nearest =
                        idx.nearest(h.x, 0.0, h.z, 4.0 * ARENA, (0.0, p.hog_leap), CAT_VEHICLE);

                    // Pick a desired heading and speed.
                    let (desired, target_speed) = match nearest {
                        Some(n) if b.flee > 0.0 => ((h.x - n.x).atan2(h.z - n.z), p.hog_fast),
                        Some(n) if is_boss || n.dist < p.hog_aggro => {
                            ((n.x - h.x).atan2(n.z - h.z), p.hog_fast)
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
                                b.repick = rng.rfr(0.5, 1.0) * p.roam_repick;
                            }
                            (gx.atan2(gz) + (now * 0.7 + b.seed).sin() * 0.4, p.hog_roam)
                        }
                    };
                    b.desired = desired;
                    b.target_speed = target_speed;

                    // Bite: any vehicle part overlapping the reach
                    // circle+band while off cooldown — the SAME query
                    // family as the chase above, so aggro and bite
                    // finally share one reach criterion. The seam hands
                    // back the part (a hog flanking a heli bites the
                    // tail it's next to); the CONSEQUENCES are the
                    // response tasks' business, so nothing applies
                    // here: a bite is a written fact. Think-cadence
                    // checking delays a bite ≤ stride-1 ticks — noise
                    // next to BITE_CD.
                    // The boss's jaws reach as far as its grown hull.
                    let reach = if is_boss { HOG_R + p.boss_grow } else { HOG_R };
                    let bitten = (b.bite_cd <= 0.0)
                        .then(|| idx.touch(h.x, h.z, reach, (0.0, p.hog_leap), CAT_VEHICLE))
                        .flatten();
                    if let Some(n) = bitten {
                        b.bite_cd = p.bite_cd;
                        if !is_boss {
                            b.flee = p.hog_flee;
                        }
                        let cid = pm.id_add();
                        contact.get_mut().add(
                            cid,
                            Contact {
                                owner: n.owner,
                                part: n.part,
                                kind: KIND_BITE,
                                source_peer: 0,
                                x: h.x,
                                y: n.band.0.max(0.0),
                                z: h.z,
                                heading: h.heading,
                            },
                        );
                    }
                }

                // MOVE (every tick): steer toward the cached decision at
                // full per-tick turn authority, then integrate.
                let turn = wrap_angle(b.desired - h.heading)
                    .clamp(-p.hog_turn * FIXED_DT, p.hog_turn * FIXED_DT);
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

                // Pose the collider: a hog is
                // its own single part — the entry is keyed by the hog's
                // OWN id, so death cleans it with the entity and the
                // janitor never has work here. Upsert every tick, the
                // pool is local — no wire cost to the change-density.
                let mut hull = hog_proto.pose(h.x, 0.0, h.z, h.heading);
                if is_boss {
                    // The hitbox matches the spectacle (p.boss_scale on
                    // the client's model).
                    hull = hull.grow(p.boss_grow);
                }
                cols.add(
                    id,
                    Collider { owner: id, part: PART_BODY, cat: CAT_HOG, hull },
                );
            });
        }
    );

    // TODO(refactor): near-verbatim copy of hog_ai — see the FlyerBrain
    // TODO above.
    // The flock (prio 28, beside the ground horde): winged hogs on the
    // same think/move cohort split — but the chase is 3D. Nearest
    // target by real distance (a heli counts until it climbs past
    // `flyer_ceil` — that's the refuge band), match its altitude at
    // p.flyer_climb authority, bite through the same contact seam the
    // horde uses; the response tasks never learn who bit them.
    task!(
        pm,
        "flyer_ai",
        28.0,
        [flyer, fbrain, index, collider, contact, params, models],
        move |pm| {
            let now = pm.tick() as f32 * FIXED_DT;
            let p = *params.get();
            let stride = (p.ai_stride.round() as u32).clamp(1, 8);
            let phase = pm.tick() % stride;
            let think_dt = stride as f32 * FIXED_DT;
            let mut rng = Rng::new(pm.tick().wrapping_mul(0xB1E5_51ED) | 1);
            // Anything with a heartbeat below the shed ceiling — the
            // flyer's query band is `[0, flyer_ceil]`; climb above it
            // and you're in the refuge.
            let idx = index.get();
            let flyer_proto = models.get().protos("flyer")[0];
            let mut flyers = flyer.get_mut();
            let mut brains = fbrain.get_mut();
            let mut cols = collider.get_mut();
            flyers.each_with(&mut brains, |id, mut f, mut b| {
                b.bite_cd = (b.bite_cd - FIXED_DT).max(0.0);
                b.flee = (b.flee - FIXED_DT).max(0.0);

                // THINK (this flyer's cohort tick): scan in 3D, decide
                // heading, speed, AND altitude; check the bite.
                if id.index() % stride == phase {
                    let nearest = idx.nearest(
                        f.x,
                        f.y,
                        f.z,
                        4.0 * ARENA,
                        (0.0, p.flyer_ceil),
                        CAT_VEHICLE,
                    );

                    let (desired, target_speed, target_alt) = match nearest {
                        // Broken off: away and back upstairs.
                        Some(n) if b.flee > 0.0 => {
                            ((f.x - n.x).atan2(f.z - n.z), p.flyer_speed, p.flyer_alt)
                        }
                        // The swoop: run the bearing, match the target's
                        // altitude — n.y is the flyer's own height
                        // clamped into the hull band, so the swoop
                        // levels off at a truck's deck or a heli's
                        // cabin edge instead of a hardcoded aim height.
                        Some(n) if n.dist < p.flyer_aggro => {
                            ((n.x - f.x).atan2(n.z - f.z), p.flyer_speed, n.y)
                        }
                        // Roam like the horde, one story up: the same
                        // goal walk plus a lazy altitude bob.
                        _ => {
                            b.repick -= think_dt;
                            let (gx, gz) = (b.goal.0 - f.x, b.goal.1 - f.z);
                            if b.repick <= 0.0 || gx * gx + gz * gz < 9.0 {
                                b.goal = roam_goal(&mut rng);
                                b.repick = rng.rfr(0.5, 1.0) * p.roam_repick;
                            }
                            (
                                gx.atan2(gz) + (now * 0.6 + b.seed).sin() * 0.35,
                                p.flyer_speed * 0.55,
                                p.flyer_alt * (1.0 + (now * 0.4 + b.seed).sin() * 0.2),
                            )
                        }
                    };
                    b.desired = desired;
                    b.target_speed = target_speed;
                    b.target_alt = target_alt.clamp(1.0, p.flyer_ceil);

                    // Bite: same verb as the horde — any part inside
                    // the reach circle+band, consequences behind the
                    // contact. Only the band differs: it rides the
                    // flyer's altitude, not the ground.
                    let bitten = (b.bite_cd <= 0.0)
                        .then(|| {
                            idx.touch(
                                f.x,
                                f.z,
                                FLYER_R,
                                (f.y - p.flyer_reach, f.y + p.flyer_reach),
                                CAT_VEHICLE,
                            )
                        })
                        .flatten();
                    if let Some(n) = bitten {
                        b.bite_cd = p.bite_cd;
                        b.flee = p.hog_flee;
                        let cid = pm.id_add();
                        contact.get_mut().add(
                            cid,
                            Contact {
                                owner: n.owner,
                                part: n.part,
                                kind: KIND_BITE,
                                source_peer: 0,
                                x: f.x,
                                y: f.y,
                                z: f.z,
                                heading: f.heading,
                            },
                        );
                    }
                }

                // MOVE (every tick): heading and speed like the horde;
                // altitude chases the decision on its own axis.
                let turn = wrap_angle(b.desired - f.heading)
                    .clamp(-p.flyer_turn * FIXED_DT, p.flyer_turn * FIXED_DT);
                f.heading = wrap_angle(f.heading + turn);
                f.speed += (b.target_speed - f.speed) * (3.0 * FIXED_DT).min(1.0);
                f.x += f.heading.sin() * f.speed * FIXED_DT;
                f.z += f.heading.cos() * f.speed * FIXED_DT;
                f.y += (b.target_alt - f.y).clamp(-p.flyer_climb * FIXED_DT, p.flyer_climb * FIXED_DT);
                if b.shove.0 != 0.0 || b.shove.1 != 0.0 {
                    f.x += b.shove.0 * FIXED_DT;
                    f.z += b.shove.1 * FIXED_DT;
                    let k = 1.0 - (KNOCK_DECAY * FIXED_DT).min(1.0);
                    b.shove.0 *= k;
                    b.shove.1 *= k;
                    if b.shove.0 * b.shove.0 + b.shove.1 * b.shove.1 < 0.05 {
                        b.shove = (0.0, 0.0);
                    }
                }
                if f.x.abs() > ARENA || f.z.abs() > ARENA {
                    f.x = f.x.clamp(-ARENA, ARENA);
                    f.z = f.z.clamp(-ARENA, ARENA);
                    f.heading = (-f.x).atan2(-f.z);
                }
                // Buildings shove only below the roofline — cruise
                // altitude clears everything but the downtown tower.
                let (px, pz, nx, nz) = building_push_below(f.x, f.z, FLYER_R, f.y);
                if nx != 0.0 || nz != 0.0 {
                    f.x = px;
                    f.z = pz;
                    let (fx, fz) = (f.heading.sin(), f.heading.cos());
                    let into = fx * nx + fz * nz;
                    if into < 0.0 {
                        let (tx, tz) = (fx - nx * into, fz - nz * into);
                        if tx * tx + tz * tz > 1e-6 {
                            f.heading = tx.atan2(tz);
                        }
                    }
                }

                // Pose the collider: self-keyed single part like the
                // ground horde, so death cleans it with the entity —
                // and the history ring makes rewound shots hit it.
                cols.add(
                    id,
                    Collider {
                        owner: id,
                        part: PART_BODY,
                        cat: CAT_HOG,
                        hull: flyer_proto.pose(f.x, f.y, f.z, f.heading),
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
    task!(pm, "hog_guns", 29.0, [hog, gunner, index, bullet, shot, params], move |pm| {
        let p = *params.get();
        let idx = index.get();
        let mut rng = Rng::new(pm.tick().wrapping_mul(0xC0FF_EE01) | 1);
        let hogs = hog.get();
        let mut guns = gunner.get_mut();
        for (id, mut cd) in guns.iter_mut() {
            *cd = (*cd - FIXED_DT).max(0.0);
            if *cd > 0.0 {
                continue;
            }
            let Some(h) = hogs.get(id) else { continue };
            let (mx, my, mz) = (h.x, p.hoggun_y, h.z);
            // Nearest vehicle part in range, ANY altitude (the range
            // sphere is the envelope) — the aim point is the hull's
            // closest axis point, muzzle height clamped into the band:
            // a level shot at a truck, a belly shot at a passing heli.
            let Some(n) = idx.nearest(
                mx,
                my,
                mz,
                p.hoggun_range,
                (f32::NEG_INFINITY, f32::INFINITY),
                CAT_VEHICLE,
            ) else {
                continue;
            };
            let (heading, pitch) = hog_aim(mx, my, mz, n.x, n.y, n.z);
            let bid = pm.id_add();
            bullet.get_mut().add(
                bid,
                Bullet {
                    x: mx,
                    y: my,
                    z: mz,
                    heading: wrap_angle(heading + rng.rfr(-p.hoggun_spread, p.hoggun_spread)),
                    pitch: wrap_angle(pitch + rng.rfr(-p.hoggun_spread, p.hoggun_spread)),
                    owner: 0.0, // peer 0 = the server's own trigger finger
                },
            );
            shot.get_mut().add(
                bid,
                Shot {
                    left: p.hoggun_travel,
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
        [truck, heli, health, bullet, gun, shot, impact, hunt, collider, parts, net, params, models],
        move |pm| {
            let p = *params.get();
            // Kind hitboxes, posed per vehicle below (the guard is a
            // read borrow on the single — held across the loop, fine).
            let mg = models.get();
            let (tp, hp) = (mg.protos("truck"), mg.protos("heli"));
            // Owners keep their parts current (the `Collider` contract):
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
                        pose(
                            id,
                            &posed(tp, fresh.body.pos.x, 0.0, fresh.body.pos.z, fresh.heading()),
                        );
                        if let Some(mut v) = health.get_id_mut(id) {
                            v.hp = p.truck_hp;
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
                    pose(
                        id,
                        &posed(tp, shooter.body.pos.x, 0.0, shooter.body.pos.z, shooter.heading()),
                    );
                    (truck_muzzle(&shooter), p.hit_pad_truck)
                } else if let Some(shooter) = heli.get_id_mut(id).map(|mut hl| {
                    heli_step(&mut hl, cmd, FIXED_DT, &p);
                    *hl
                }) {
                    let b = shooter.body;
                    if health.get_id(id).is_some_and(|v| v.hp <= 0.0) {
                        let fresh = spawn_heli(peer, &p);
                        if let Some(mut hl) = heli.get_id_mut(id) {
                            *hl = fresh;
                        }
                        let fb = fresh.body;
                        pose(id, &posed(hp, fb.pos.x, fb.pos.y, fb.pos.z, fb.yaw()));
                        if let Some(mut v) = health.get_id_mut(id) {
                            v.hp = p.truck_hp;
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
                    pose(id, &posed(hp, b.pos.x, b.pos.y, b.pos.z, b.yaw()));
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
                        view: inputs.view(peer).saturating_sub(interp_ticks(&p)),
                        pad,
                    },
                );
            }
        }
    );

    // Bullets (prio 31, right after the guns fire): THE collisions
    // sweep, and the LAG-COMP design record. Each bullet's per-tick
    // travel is judged in the collider frame its SHOOTER was looking
    // at (acked tick minus their interp delay, out of the history
    // ring) — hogs AND teammates in the same rewound frame,
    // favor-the-shooter, one timeline per shot. Decided 2026-07-17,
    // with eyes open: "I was behind cover on my own screen" loses to
    // sim consistency, the genre default (Source rewinds ALL players).
    // The Source guardrails came along: rewind bounded by ring depth
    // (1 s), restore exact for free (the sweep only READS old frames,
    // nothing is mutated), and the teleport guard falls out of id
    // generations — a vehicle swap is a fresh entity, so stale frames
    // simply miss. Historying the COLLIDER pool (pose data, never a
    // physics world) is precisely Source's hitbox-only rewind, and
    // Unity DOTS ships the uniform version (PhysicsWorldHistory-
    // Singleton) — shipped precedent on both ends. Ring cost = parts
    // × depth: fine while swarm entities stay single-part; don't give
    // every hog four parts because it got easy.
    //
    // The sweep iterates collider entries — a new vehicle registers
    // its part and is swept without this task changing — writes
    // Contact facts, and applies NOTHING; the response tasks below own
    // every consequence. Damage lands on the PRESENT entity (id_alive-
    // gated); only the hit test rewinds. Ghosts eat rounds
    // deliberately: a part outliving its owner by a tick is still hit
    // in old frames — the shooter saw it there — but no contact
    // reaches a dead owner. Buildings and arena walls stop bullets in
    // present time (they don't move).
    task!(
        pm,
        "bullets",
        31.0,
        [bullet, shot, collider, contact, impact, net, params],
        move |pm| {
            let p = *params.get();
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
                // THE sweep: the shot is judged
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
                } else if s.left <= 0.0 || b.x.abs() > ARENA || b.z.abs() > ARENA || b.y > p.heli_ceil
                {
                    pm.id_remove(id);
                }
            });
        }
    );

    // TODO(refactor): all four response tasks open with the same
    // "collect this tick's contacts whose owner is in MY pool" preamble,
    // and the bullets task carries the stale-purge half by hand — engine
    // candidate: formalize the fact-pool contract (write on fresh id,
    // changed-tick filter, consume-at-drain, loud purge of the
    // undrained) as one type, per the docs doctrine that landed designs
    // move onto types. hog_hits/flyer_hits are additionally twins — see
    // the FlyerBrain TODO. Rule of three for the vehicle side: the
    // differences between truck/heli responses ARE the per-vehicle feel
    // (bite scrub vs none, tail kick) — unify the health-chip half only
    // when a 4th vehicle proves the pattern.
    // Response tasks (prio 32, right after the sweep): each vehicle
    // kind drains the contacts addressed to ITS entities and owns every
    // consequence — detection and response never meet in a function
    // again (the `Contact` contract). The changed-tick filter keeps a
    // response to THIS tick's facts (anything older is the sweep's
    // loud-purge path), and the owner lookup doubles as the liveness
    // check at drain (validate at consumption, the `Parts` rule). Today
            // truck and
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
    // TODO(roadmap): part behavior as data — small, worth doing. The
    // damage multipliers and the tail kick below live as match arms;
    // moving `dmg_mul`/`kick` onto the part's collider entry means new
    // vehicles get part behavior without touching response tasks, and
    // the numbers become params-tunable.
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
    task!(pm, "hog_hits", 32.0, [contact, hog, brain, boss, hunt, impact, params], move |pm| {
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
            // The boss soaks it on the big pool — `Hog::hp` only
            // mirrors the fraction for tinting (its wire repr
            // saturates at 1.275, the reason `Boss::hp` exists). No
            // knockback: mass. Same corpse guard as the horde below.
            if boss.get_id(c.owner).is_some() {
                let hp_left = match boss.get_id_mut(c.owner) {
                    Some(mut bh) if bh.hp > 0.0 => {
                        bh.hp -= p.gun_dmg;
                        bh.hp
                    }
                    _ => continue,
                };
                if hp_left > 0.0 {
                    if let Some(mut h) = hog.get_id_mut(c.owner) {
                        h.hp = (hp_left / p.boss_hp).clamp(0.01, 1.0) * p.hog_hp;
                    }
                } else {
                    pm.id_remove(c.owner); // brain + boss entry + collider go with it
                    hunt.get_mut().points += p.kill_points * 10.0;
                    if !quiet {
                        eprintln!("[server] BOSS down at ({:.1},{:.1}) now={now}", c.x, c.z);
                    }
                }
                let mid = pm.id_add();
                impact.get_mut().add(
                    mid,
                    Impact {
                        x: c.x,
                        z: c.z,
                        kind: if hp_left <= 0.0 { IMPACT_BOOM } else { IMPACT_HIT },
                    },
                );
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

    // The flyers' response — hog_hits' airborne twin: same corpse
    // guard, same horizontal knockback (the vertical drama on a kill
    // is the client's falling ragdoll), same kill economy. Bites never
    // land here (flyers don't get bitten); only bullets do.
    task!(pm, "flyer_hits", 32.0, [contact, flyer, fbrain, hunt, impact, params], move |pm| {
        let now = pm.tick();
        let p = *params.get();
        let hits: Vec<(Id, Contact)> = {
            let pool = contact.get();
            pool.iter()
                .filter(|&(cid, c)| {
                    pool.changed_tick(cid) == Some(now) && flyer.get_id(c.owner).is_some()
                })
                .map(|(cid, c)| (cid, *c))
                .collect()
        };
        for (cid, c) in hits {
            pm.id_remove(cid); // fact consumed
            if c.kind != KIND_BULLET {
                continue;
            }
            let killed = match flyer.get_id_mut(c.owner) {
                Some(mut f) if f.hp > 0.0 => {
                    f.hp -= p.gun_dmg;
                    f.hp <= 0.0
                }
                _ => continue,
            };
            if !killed && let Some(mut br) = fbrain.get_id_mut(c.owner) {
                br.shove = (c.heading.sin() * p.knock, c.heading.cos() * p.knock);
            }
            if killed {
                pm.id_remove(c.owner); // brain + collider go with it
                hunt.get_mut().points += p.kill_points;
                if !quiet {
                    eprintln!("[server] flyer down at ({:.1},{:.1}) now={now}", c.x, c.z);
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

    // The depot's response — the DEFEND objective drains its own
    // contacts like any vehicle: bites chew it, stray rounds (gunner
    // or friendly) chip it. Nobody here decides the mission is lost —
    // the director reads hp; a response task applies damage, one
    // place owns the arc.
    task!(pm, "depot_hits", 32.0, [contact, depot, impact, params], move |pm| {
        let now = pm.tick();
        let p = *params.get();
        let hits: Vec<(Id, Contact)> = {
            let pool = contact.get();
            pool.iter()
                .filter(|&(cid, c)| {
                    pool.changed_tick(cid) == Some(now) && depot.get_id(c.owner).is_some()
                })
                .map(|(cid, c)| (cid, *c))
                .collect()
        };
        for (cid, c) in hits {
            pm.id_remove(cid); // fact consumed
            let dmg = match c.kind {
                KIND_BITE => p.bite_dmg,
                KIND_BULLET if c.source_peer == 0 => p.hoggun_dmg,
                KIND_BULLET => p.friendly_dmg,
                _ => 0.0,
            };
            if let Some(mut d) = depot.get_id_mut(c.owner) {
                d.hp = (d.hp - dmg).max(0.0);
            }
            let mid = pm.id_add();
            impact.get_mut().add(
                mid,
                Impact {
                    x: c.x,
                    z: c.z,
                    kind: if c.kind == KIND_BITE { IMPACT_BITE } else { IMPACT_HIT },
                },
            );
        }
    });

    // THE DIRECTOR (prio 33) — the game loop's arc, where the old
    // free-running wave task lived. A state machine over `Hunt.phase`
    // (this task is the single's only writer): LOBBY waits for the
    // first vehicle, BRIEF counts down a titled splash, PLAYING runs
    // the current mission's objective (see the MISSION_* contract in
    // common.rs), WON/LOST wait for any player's `Session` event —
    // retry the failed mission, or the next level after a win. Level
    // switching is deliberately cheap: purge the NPCs, index the next
    // `LevelDef`, brief mission 0 — a real map swap slots in behind
    // this same seam when maps stop being const. Difficulty is data:
    // the wave engine reads `wave_base`/`wave_grow` × the mission's
    // `size`, and the mission tables re-run kinds harder.
    task!(
        pm,
        "director",
        33.0,
        [hog, brain, gunner, flyer, fbrain, truck, heli, hunt, params, depot, boss, collider],
        move |pm| {
            let p = *params.get();
            let mut go = false;
            for (_peer, _ev) in sessions.drain() {
                go = true;
            }

            // Copy out, run the machine, write back only on change —
            // the single ships only when the arc moves (BRIEF and RACE
            // tick `timer`, so those phases stream; a quiet PLAYING
            // wave stays off the wire).
            let prev = *hunt.get();
            let mut sb = prev;
            sb.alive = (hog.get().len() + flyer.get().len()) as u32;

            match sb.phase {
                PHASE_LOBBY => {
                    // The roster spawns a truck on join, so "a vehicle
                    // exists" IS "someone's here". Nothing spawns
                    // until then — an empty server idles quiet.
                    if !truck.get().is_empty() || !heli.get().is_empty() {
                        let lv = sb.level;
                        enter_brief(&mut sb, lv, 0, &p);
                        if !quiet {
                            let ld = level_def(sb.level);
                            eprintln!("[server] level '{}' begins", ld.name);
                        }
                    }
                }
                PHASE_BRIEF => {
                    sb.timer -= FIXED_DT;
                    if sb.timer <= 0.0 {
                        let m = mission_def(sb.level, sb.mission);
                        sb.phase = PHASE_PLAYING;
                        sb.timer = m.time;
                        // Mission furniture goes live with the phase.
                        match m.kind {
                            MISSION_DEFEND => {
                                let id = pm.id_add();
                                let d = Depot { x: DEPOT_POS.0, z: DEPOT_POS.1, hp: p.depot_hp };
                                // Static self-keyed CAT_VEHICLE part —
                                // the whole defend mission: hog aggro,
                                // bites, and gunner fire route to it
                                // through seams that already exist.
                                collider.get_mut().add(
                                    id,
                                    Collider {
                                        owner: id,
                                        part: PART_BODY,
                                        cat: CAT_VEHICLE,
                                        hull: depot_hull(&d),
                                    },
                                );
                                depot.get_mut().add(id, d);
                            }
                            MISSION_BOSS => {
                                spawn_boss(pm, &hog, &brain, &boss, &p);
                                if !quiet {
                                    eprintln!("[server] boss spawned");
                                }
                            }
                            _ => {}
                        }
                        if !quiet {
                            eprintln!("[server] mission '{}' live", m.name);
                        }
                    }
                }
                PHASE_PLAYING => {
                    let m = mission_def(sb.level, sb.mission);
                    let cleared = hog.get().is_empty() && flyer.get().is_empty();
                    // 0 = still playing, 1 = complete, 2 = failed.
                    let mut outcome = 0u32;
                    match m.kind {
                        MISSION_WAVES | MISSION_DEFEND => {
                            if cleared {
                                if sb.wave > sb.done {
                                    sb.done = sb.wave; // that wave is cleared
                                }
                                if sb.done >= m.goal {
                                    outcome = 1;
                                } else {
                                    sb.wave = sb.done + 1;
                                    let count = (p.wave_base + (sb.wave - 1) as f32 * p.wave_grow)
                                        * m.size;
                                    spawn_wave(
                                        pm, &hog, &brain, &gunner, &flyer, &fbrain, &p,
                                        count.round() as u32, sb.wave, quiet,
                                    );
                                }
                            }
                            if m.kind == MISSION_DEFEND
                                && depot.get().iter().any(|(_, d)| d.hp <= 0.0)
                            {
                                outcome = 2;
                            }
                        }
                        MISSION_RACE => {
                            sb.timer -= FIXED_DT;
                            // Any vehicle through the beacon advances
                            // the loop — co-op, not a per-player lap.
                            let (cx, cz) = RACE_LOOP[sb.done as usize % RACE_LOOP.len()];
                            let near = |x: f32, z: f32| {
                                (x - cx) * (x - cx) + (z - cz) * (z - cz) < RACE_CP_R * RACE_CP_R
                            };
                            let reached = truck
                                .get()
                                .iter()
                                .any(|(_, t)| near(t.body.pos.x, t.body.pos.z))
                                || heli.get().iter().any(|(_, h)| {
                                    h.body.pos.y < RACE_CP_H
                                        && near(h.body.pos.x, h.body.pos.z)
                                });
                            if reached {
                                sb.done += 1;
                                if !quiet {
                                    eprintln!("[server] beacon {}/{}", sb.done, m.goal);
                                }
                            }
                            if sb.done >= m.goal {
                                outcome = 1;
                            } else if sb.timer <= 0.0 {
                                sb.timer = 0.0;
                                outcome = 2;
                            } else if sb.alive < (p.wave_base * m.size * 0.5) as u32 {
                                // The chase: no wave rhythm, a steady
                                // trickle off the walls keeps pressure
                                // on the runners.
                                spawn_wave(
                                    pm, &hog, &brain, &gunner, &flyer, &fbrain, &p, 6, sb.wave,
                                    true,
                                );
                            }
                        }
                        MISSION_BOSS => {
                            match boss.get().iter().next() {
                                Some((_, b)) => {
                                    sb.done = (b.hp / p.boss_hp * 100.0).ceil().max(1.0) as u32;
                                    // Escorts whenever the deck goes
                                    // quiet (alive == 1 is the boss
                                    // itself — it rides the hog pool,
                                    // so `cleared` never fires here).
                                    if sb.alive <= 1 {
                                        let count = (p.wave_base * m.size).round() as u32;
                                        spawn_wave(
                                            pm, &hog, &brain, &gunner, &flyer, &fbrain, &p,
                                            count, sb.wave, true,
                                        );
                                    }
                                }
                                // The boss entry going away IS the win
                                // (hog_hits removes it at 0 hp).
                                None => {
                                    sb.done = 0;
                                    outcome = 1;
                                }
                            }
                        }
                        _ => {}
                    }
                    match outcome {
                        1 => {
                            purge_npcs(pm, &hog, &flyer, &depot);
                            let next = sb.mission + 1;
                            if next as usize >= level_def(sb.level).missions.len() {
                                sb.phase = PHASE_WON;
                                sb.timer = 0.0;
                                if !quiet {
                                    eprintln!("[server] level complete, {} pts", sb.points as i32);
                                }
                            } else {
                                let lv = sb.level;
                                enter_brief(&mut sb, lv, next, &p);
                                if !quiet {
                                    eprintln!("[server] mission complete");
                                }
                            }
                        }
                        2 => {
                            purge_npcs(pm, &hog, &flyer, &depot);
                            sb.phase = PHASE_LOST;
                            if !quiet {
                                eprintln!("[server] mission FAILED");
                            }
                        }
                        _ => {}
                    }
                }
                PHASE_WON if go => {
                    // Next level (wrapping — the campaign loops until
                    // there are more maps than levels).
                    let next = (sb.level + 1) % LEVELS.len() as u32;
                    enter_brief(&mut sb, next, 0, &p);
                }
                PHASE_LOST if go => {
                    // Retry the mission that killed you, not the whole
                    // level.
                    let (lv, ms) = (sb.level, sb.mission);
                    enter_brief(&mut sb, lv, ms, &p);
                }
                _ => {}
            }

            if sb != prev {
                *hunt.get_mut() = sb;
            }
        }
    );

    if !quiet {
        task!(
            pm,
            "status",
            90.0,
            5.0,
            [hog, flyer, truck, heli, bullet, impact, hunt],
            move |pm| {
                let sb = hunt.get();
                // impacts churning 0..few proves the TTL; alive falling
                // proves shots land through the rewound frames; bullets
                // churning proves entity add/remove replicates at pace.
                eprintln!(
                    "[server] t={} phase={} m={} wave={} hogs={} flyers={} trucks={} helis={} pts={:.0} impacts={} bullets={}",
                    pm.tick() / 60,
                    sb.phase,
                    sb.mission,
                    sb.wave,
                    hog.get().len(),
                    flyer.get().len(),
                    truck.get().len(),
                    heli.get().len(),
                    sb.points,
                    impact.get().len(),
                    bullet.get().len(),
                );
            }
        );
    }

    // TODO(refactor): duplicated in bot_client.rs — share one prof task
    // fn (or grow an engine pm.prof_task(); every example will want it).
    // `prof` arg: per-task cycle times every 5 s — the stress lab should
    // answer "where does the tick go?" without a profiler attached.
    if prof {
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

/// The parent→child lifecycle convention, pinned (see `Parts` in
/// common.rs): a part has its OWN id, so removing the owner leaves a ghost
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
                hull: crate::models::collide_protos(&crate::models::truck())[0]
                    .pose(0.0, 0.0, 0.0, 0.0),
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
