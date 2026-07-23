//! Hogs client gameplay, shared by the SDL player and headless bots:
//! full-schema registration (every client registers every channel — the
//! handshake demands it), local-truck prediction, and snapshot interp
//! for BOTH remote pools (trucks and the horde). The horde interp is the
//! interesting one: at 300 hogs the byte budget rotates and the interp
//! delay is what rides the multi-tick gaps between a hog's updates.

use pm::{EventTx, Id, InputTx, Pm, PmClient, PoolHandle, Predictor, SingleHandle, SingleRx};

use crate::common::*;
use crate::models::{Models, posed};

/// Everything a hogs client holds after setup. Fields the headless bot
/// doesn't read (draw pools, scoreboard) still exist — registration is
/// the schema contract; reading is optional.
pub struct ClientWorld {
    /// Authoritative replicas. Health is server-owned truth read RAW
    /// (never predicted, never interp'd — a HUD wants the latest word).
    /// Bullet/impact raw replicas also feed the sfx `Adds` trackers —
    /// edges come off the earliest copy, not the delayed draw pools.
    pub hog: PoolHandle<Hog>,
    pub flyer: PoolHandle<Flyer>,
    pub health: PoolHandle<Health>,
    pub bullet: PoolHandle<Bullet>,
    pub impact: PoolHandle<Impact>,
    /// Mission furniture, read raw: the DEFEND objective (one entry
    /// while a defend mission runs) and the boss marker pool (keyed by
    /// the boss's hog id — membership is "draw that one huge").
    pub depot: PoolHandle<Depot>,
    pub boss: PoolHandle<Boss>,
    /// Smoothed views rendering should read (predicted own vehicle wins
    /// on its draw pool; hogs, flyers, and bullets are pure interp).
    pub truck_draw: PoolHandle<Truck>,
    pub heli_draw: PoolHandle<Heli>,
    pub hog_draw: PoolHandle<Hog>,
    pub flyer_draw: PoolHandle<Flyer>,
    pub bullet_draw: PoolHandle<Bullet>,
    /// The co-op scoreboard (server-owned synced single).
    pub hunt: SingleRx<Hunt>,
    pub input: InputTx<Drive>,
    pub respawn: EventTx<Respawn>,
    /// End-screen advance (ENTER on won/lost) — the director's door.
    pub session: EventTx<Session>,
    /// Reliable param writes (telemetry knobs → server clamp).
    pub param_set: EventTx<ParamSet>,
    /// The server's tuning set, replicated (the Params declaration in
    /// common.rs is the design record):
    /// the predictors' steps read it (shared-step constants), and any
    /// client read of a tunable — bot gates, cosmetic gun cadence, aim
    /// line reach — comes off this replica, never a const.
    pub params: SingleRx<Params>,
    // TODO(refactor): the "whichever predictor is live" or-chain repeats
    // in player_client (input, lamps, title bar), telemetry, and sfx —
    // add ClientWorld helpers (my_pose / my_speed / corrections).
    /// Both vehicle predictors ride the same input channel; whichever
    /// pool holds the avatar is live, the other idles with `state() ==
    /// None` — which is also how game code asks "am I flying?".
    pub pred: SingleHandle<Predictor<Truck, Drive>>,
    pub pred_heli: SingleHandle<Predictor<Heli, Drive>>,
    /// The models registry (models.rs): the client reads the same
    /// `collide.*` protos the server judges with (hold-fire courtesy)
    /// and, on the player, uploads the render parts from it.
    pub models: SingleHandle<Models>,
    /// THE client-side world-query index (`WorldIndex`'s rustdoc is the
    /// design record): the
    /// local collider pool below, mirrored into a tree — same struct,
    /// same verbs, same shapes as the server's, posed from the SMOOTHED
    /// draw pools (what this client believes the world looks like,
    /// which is exactly the view the server's lag comp honors for its
    /// shots). Ask it `sweep`/`nearest`/`touch`; never scan pools for
    /// geometry.
    pub index: SingleHandle<WorldIndex>,
}

// (The old `hulls()` — a Vec of posed hulls rebuilt on every call —
// died here 2026-07-20: the client now keeps a REAL local collider
// pool + WorldIndex, posed by the `colliders` task below, and asks it
// through the same verbs the server uses. The pods-vs-colliders
// boundary that survived the migration: colliders answer WHERE
// (line_clear, any geometry question); the pods answer WHAT — bot
// targeting stays on the replicas because the lead math reads
// velocity (`heading`/`speed`) and kind-specific gates, which shape
// data deliberately does not carry. The debug cages stay instanced
// off per-kind protos for the same reason: a cage MESH is per kind,
// and the collider pool is kind-erased by design. The WorldIndex
// rustdoc tells the whole story.)

/// Register the full channel set and install prediction + interpolation.
/// No connect here — `run` does that once the schema is complete.
pub fn client_setup(pm: &mut PmClient) -> ClientWorld {
    let truck = pm.sync_pool::<Truck>("truck");
    let heli = pm.sync_pool::<Heli>("heli");
    let health = pm.sync_pool::<Health>("truck.health");
    let hog = pm.wire_pool::<Hog>("hog");
    let flyer = pm.wire_pool::<Flyer>("flyer");
    let bullet = pm.wire_pool::<Bullet>("bullet");
    let impact = pm.wire_pool::<Impact>("impact");
    let depot = pm.sync_pool::<Depot>("depot");
    let boss = pm.sync_pool::<Boss>("boss");
    let hunt = pm.sync_single::<Hunt>("hunt");
    // The server's tuning set, live: the
    // predictor step closures below capture this replica, so the same
    // numbers drive both ends — a live change mispredicts for one
    // snapshot interval and reconciles (the documented blip). The write
    // path is `param_set` below.
    let params = pm.sync_single::<Params>("params");
    let input = pm.input::<Drive>("drive");
    let respawn = pm.event::<Respawn>("respawn");
    let session = pm.event::<Session>("session");
    let param_set = pm.event::<ParamSet>("param.set");
    // The models registry (LOCAL single — never in the handshake):
    // shape data by kind name, same load the server runs.
    let models = pm.single::<Models>("models");
    *models.get_mut() = Models::load();

    // Local vehicle: reconcile against the input-seq echo, replay
    // unacked sends — the same steps the server runs make it byte-exact.
    // BOTH vehicles get predictors on the ONE input channel; the one
    // whose pool holds the avatar runs, the other idles empty.
    let pred = pm.predict_pool(
        &truck,
        &input,
        {
            let params = params.clone();
            move |s, c, dt| truck_step(s, c, dt, &params.get())
        },
        Truck::pod_err,
        1e-4,
        FIXED_DT,
    );
    let pred_heli = pm.predict_pool(
        &heli,
        &input,
        {
            let params = params.clone();
            move |s, c, dt| heli_step(s, c, dt, &params.get())
        },
        Heli::pod_err,
        1e-4,
        FIXED_DT,
    );

    // Remote smoothing, shared delay contract with the server's shot
    // rewind (see common.rs INTERP_DELAY). Capped extrapolation rides
    // loss bursts — and, for the horde, budget-rotation gaps.
    let extrap = std::env::var("PM_EXTRAP_MS")
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
        .map_or(0.05, |ms| ms / 1000.0);
    let truck_draw = pm.interp_pool(&truck, Truck::pod_lerp, interp_delay() as f64, extrap);
    let heli_draw = pm.interp_pool(&heli, Heli::pod_lerp, interp_delay() as f64, extrap);
    let hog_draw = pm.interp_pool(&hog, Hog::pod_lerp, interp_delay() as f64, extrap);
    let flyer_draw = pm.interp_pool(&flyer, Flyer::pod_lerp, interp_delay() as f64, extrap);
    // Bullets too: they live ~0.6 s and cross the map in a straight
    // line — interp (plus the extrapolation cap) is plenty.
    let bullet_draw = pm.interp_pool(&bullet, Bullet::pod_lerp, interp_delay() as f64, extrap);

    // The client-local collider pool + query index — the SAME
    // structures the server keeps, posed from the draw pools instead
    // of the sim. Local pools, never in the
    // handshake; the shared names make the symmetry legible in
    // pool_stats. The `colliders` task at 6.5 runs right after the
    // interp tasks (NET_PRIO+1) refresh the draw pools, so the index
    // is exactly as fresh as what's on screen.
    let colliders = pm.pool::<Collider>("collider");
    let cparts = pm.pool::<Parts>("vehicle.part");
    let index = pm.single::<WorldIndex>("collider.index");
    {
        let (truck_draw, heli_draw) = (truck_draw.clone(), heli_draw.clone());
        let (hog_draw, flyer_draw) = (hog_draw.clone(), flyer_draw.clone());
        let (depot, boss) = (depot.clone(), boss.clone());
        let (colliders, cparts, index, models) =
            (colliders.clone(), cparts.clone(), index.clone(), models.clone());
        // Lifecycle edges off the draw pools: vehicles get part child
        // entities exactly like the server's roster/respawn path; the
        // horde is self-keyed and just overwrite-posed.
        let mut truck_adds = pm::Adds::default();
        let mut heli_adds = pm::Adds::default();
        let mut truck_removes: pm::Removes<Truck> = Default::default();
        let mut heli_removes: pm::Removes<Heli> = Default::default();
        let mut hog_removes: pm::Removes<Hog> = Default::default();
        let mut flyer_removes: pm::Removes<Flyer> = Default::default();
        let mut depot_removes: pm::Removes<Depot> = Default::default();
        pm.task_add("colliders", 6.5, 0.0, move |pm| {
            let m = models.get();
            let (tp, hp) = (m.protos("truck"), m.protos("heli"));
            let (hog_p, flyer_p) = (m.protos("hog")[0], m.protos("flyer")[0]);
            // Vehicles leaving: free their part children (the pool
            // entries go with the ids at end-of-tick flush — the same
            // one-tick ghost window the server's janitor allows).
            let mut dead: Vec<Id> =
                truck_removes.drain(&truck_draw.get()).into_iter().map(|(i, _)| i).collect();
            dead.extend(heli_removes.drain(&heli_draw.get()).into_iter().map(|(i, _)| i));
            for vid in dead {
                if let Some(p) = cparts.get_mut().remove(vid) {
                    for i in 0..p.n as usize {
                        pm.id_remove(p.ids[i]);
                    }
                }
            }
            // Vehicles arriving: register parts (no guards held —
            // parts_add takes the handles itself).
            let born: Vec<(Id, Truck)> = {
                let d = truck_draw.get();
                truck_adds.drain(&d).into_iter().filter_map(|i| d.get(i).map(|t| (i, *t))).collect()
            };
            for (vid, t) in born {
                let hulls = posed(&tp, t.body.pos.x, 0.0, t.body.pos.z, t.heading());
                parts_add(pm, &colliders, &cparts, vid, CAT_VEHICLE, &hulls);
            }
            let born: Vec<(Id, Heli)> = {
                let d = heli_draw.get();
                heli_adds.drain(&d).into_iter().filter_map(|i| d.get(i).map(|h| (i, *h))).collect()
            };
            for (vid, h) in born {
                let b = h.body;
                let hulls = posed(&hp, b.pos.x, b.pos.y, b.pos.z, b.yaw());
                parts_add(pm, &colliders, &cparts, vid, CAT_VEHICLE, &hulls);
            }
            // Re-pose everything at this frame's draw state.
            {
                let mut cols = colliders.get_mut();
                let pts = cparts.get();
                for (vid, t) in truck_draw.get().iter() {
                    if let Some(p) = pts.get(vid) {
                        for (i, proto) in tp.iter().take(p.n as usize).enumerate() {
                            if let Some(mut c) = cols.get_mut(p.ids[i]) {
                                c.hull = proto.pose(t.body.pos.x, 0.0, t.body.pos.z, t.heading());
                            }
                        }
                    }
                }
                for (vid, h) in heli_draw.get().iter() {
                    if let Some(p) = pts.get(vid) {
                        let b = h.body;
                        for (i, proto) in hp.iter().take(p.n as usize).enumerate() {
                            if let Some(mut c) = cols.get_mut(p.ids[i]) {
                                c.hull = proto.pose(b.pos.x, b.pos.y, b.pos.z, b.yaw());
                            }
                        }
                    }
                }
                for (id, _) in hog_removes.drain(&hog_draw.get()) {
                    cols.remove(id);
                }
                for (id, _) in flyer_removes.drain(&flyer_draw.get()) {
                    cols.remove(id);
                }
                for (id, h) in hog_draw.get().iter() {
                    // The boss is a hog wearing a bigger hull — same
                    // grow the server sweeps, so hold-fire and the
                    // debug cage agree with what shots actually hit.
                    let mut hull = hog_p.pose(h.x, 0.0, h.z, h.heading);
                    if boss.get_id(id).is_some() {
                        hull = hull.grow(BOSS_GROW);
                    }
                    cols.add(
                        id,
                        Collider { owner: id, part: PART_BODY, cat: CAT_HOG, hull },
                    );
                }
                for (id, f) in flyer_draw.get().iter() {
                    cols.add(
                        id,
                        Collider {
                            owner: id,
                            part: PART_BODY,
                            cat: CAT_HOG,
                            hull: flyer_p.pose(f.x, f.y, f.z, f.heading),
                        },
                    );
                }
                // The depot: static self-keyed entry off the raw
                // replica (no interp — it doesn't move), so bot
                // hold-fire won't spray through it.
                for (id, _) in depot_removes.drain(&depot.get()) {
                    cols.remove(id);
                }
                for (id, d) in depot.get().iter() {
                    cols.add(
                        id,
                        Collider {
                            owner: id,
                            part: PART_BODY,
                            cat: CAT_VEHICLE,
                            hull: depot_hull(d),
                        },
                    );
                }
            }
            index.get_mut().sync(&colliders.get());
        });
    }

    ClientWorld {
        params,
        hog,
        flyer,
        health,
        bullet,
        impact,
        depot,
        boss,
        session,
        truck_draw,
        heli_draw,
        hog_draw,
        flyer_draw,
        bullet_draw,
        hunt,
        input,
        respawn,
        param_set,
        pred,
        pred_heli,
        models,
        index,
    }
}

/// Headless bot: hunt the nearest hog — drive at it, shoot when lined
/// up, wander when the wave is dead. Trucks also engage flyers that
/// swoop into flat-shot height; pilot bots (n >= 2) gimbal the chin
/// gun onto them wherever they are.
pub fn run_bot(n: u32, link: (f32, f32), addr: &str, password: &str) {
    let mut pm = Pm::client(addr, 1.0 / FIXED_DT);
    if !password.is_empty() {
        pm.password(password);
    }
    if link != (0.0, 0.0) {
        pm.link_lag(link.0, link.1);
    }
    let w = client_setup(&mut pm);
    let net = pm.net();
    // Predictor handles for the prof task below (`w` moves into the bot
    // task first).
    let w2_pred = w.pred.clone();
    let w2_pred_heli = w.pred_heli.clone();

    // Building-jam recovery state: seconds spent commanding thrust while
    // not moving, and seconds of back-out left once we give up.
    let mut jam = 0.0f32;
    let mut back = 0.0f32;
    // Which way the back-out drives: opposite whatever was jammed.
    let mut back_dir = -1.0f32;
    // Pilot bots: one-shot "give me the heli" request state.
    let mut asked_heli = false;

    pm.task_add("bot", 4.0, 0.0, move |pm| {
        let t = pm.tick() as f32 / 60.0 + n as f32 * 1.7;
        let p = w.params.get();

        // Trigger discipline — friendly fire is live: hold when a
        // teammate's (grown) hull crosses the line of fire. Same hulls
        // and sweep the server judges with, generous margin: a held
        // shot costs a quarter second, a connected one costs a quarter
        // of a buddy's hp.
        // The SAME sweep judgment the server applies to shots, asked of
        // the client's own index: pad 0.5 as the courtesy margin, own
        // vehicle skipped the way a shooter's is — one query vocabulary
        // on both ends of the wire.
        let line_clear = |x: f32, y: f32, z: f32, bearing: f32, pitch: f32, reach: f32| {
            let dy = pitch.tan() * reach;
            w.index
                .get()
                .sweep(x, z, y, bearing, reach, dy, 0.5, CAT_VEHICLE, net.mine())
                .is_none()
        };

        // Bots n >= 2 are PILOTS: swap to the heli once spawned, then
        // fly strafing runs — deliberately exercising the whole 3D path
        // headlessly (vehicle swap, heli prediction, diving bullets,
        // low hover inside hog-leap range on sloppy pull-ups).
        if n >= 2 {
            if let Some(hl) = w.pred_heli.get().state() {
                let b = hl.body;
                let (yaw, pitch_now, _) = b.rot.to_yaw_pitch_roll();
                // Candidates: ground hogs (aimed the old way — nose
                // over until the gun line meets the dirt) and FLYERS
                // (the gimbal's job: hold the airframe, train the chin
                // gun on them). (x, z, target altitude, is_air).
                let target = w
                    .hog
                    .get()
                    .values()
                    .iter()
                    .map(|h| (h.x, h.z, 0.0, false))
                    .chain(
                        w.flyer
                            .get()
                            .values()
                            .iter()
                            .map(|f| (f.x, f.z, f.y, true)),
                    )
                    .map(|(x, z, y, air)| {
                        let (dx, dz) = (x - b.pos.x, z - b.pos.z);
                        ((x, z, y, air), (dx * dx + dz * dz).sqrt())
                    })
                    .min_by(|a, b| a.1.total_cmp(&b.1));
                let (turn, pitch, fire, aim_pitch) = match target {
                    Some(((hx, hz, hy, air), d)) => {
                        let bearing = (hx - b.pos.x).atan2(hz - b.pos.z);
                        let err = wrap_angle(bearing - yaw);
                        if air {
                            // Air-to-air: the climb the shot needs,
                            // converted to a gimbal angle against the
                            // CURRENT airframe pitch (muzzle climb =
                            // aim_pitch − pitch; re-solved every tick,
                            // so it servos as the nose wanders).
                            let want = ((hy - (b.pos.y - 0.35)) / d.max(2.0)).atan();
                            let ap =
                                (want + pitch_now).clamp(-HELI_AIM_PITCH, HELI_AIM_PITCH);
                            let aligned = err.abs() < 0.3
                                && d < p.gun_range * 0.9
                                && line_clear(b.pos.x, b.pos.y, b.pos.z, bearing, want, d);
                            (err.clamp(-1.0, 1.0), 0.1, aligned as i32 as f32, ap)
                        } else {
                            // Nose over far enough that the gun line
                            // meets the ground at the hog.
                            let dive = (b.pos.y / d.max(3.0)).atan();
                            let aligned = err.abs() < 0.25
                                && d < p.gun_range * 0.9
                                && line_clear(b.pos.x, b.pos.y, b.pos.z, bearing, -dive, d);
                            (
                                err.clamp(-1.0, 1.0),
                                (dive / HELI_PITCH_MAX).clamp(-1.0, 1.0),
                                aligned as i32 as f32,
                                0.0,
                            )
                        }
                    }
                    None => ((t * 0.3).sin() * 0.5, 0.15, 0.0, 0.0),
                };
                // Hold a working altitude; drift low on purpose now and
                // then so leaping hogs get their shot at us.
                let lift = if b.pos.y < 6.0 + (t * 0.11).sin() * 4.0 {
                    1.0
                } else if b.pos.y > 14.0 {
                    -0.5
                } else {
                    0.05
                };
                w.input.set(Drive {
                    thrust: 0.0,
                    turn,
                    fire,
                    aim: 0.0, // azimuth by yawing the airframe
                    boost: 0.0,
                    bot: 1.0,
                    pitch,
                    lift,
                    aim_pitch,
                });
                return;
            }
            if !asked_heli && w.pred.get().state().is_some() {
                w.respawn.send(Respawn { vehicle: VEH_HELI });
                asked_heli = true;
            }
            // Fall through and drive the truck until the swap lands.
        }

        let Some(me) = w.pred.get().state() else {
            return;
        };
        // Nearest target, from the authoritative replicas (bots don't
        // render, so no reason to read the draw pools): every ground
        // hog AND every flyer — the turret elevates now, so the flock
        // is in the truck's envelope wherever the stops can reach it.
        // (x, z, heading, speed, y): chase + lead read the first four,
        // the elevation solution reads y (1.45 = flat for the horde).
        let target = w
            .hog
            .get()
            .values()
            .iter()
            .map(|h| (h.x, h.z, h.heading, h.speed, 1.45))
            .chain(
                w.flyer
                    .get()
                    .values()
                    .iter()
                    .map(|f| (f.x, f.z, f.heading, f.speed, f.y)),
            )
            .map(|(x, z, heading, speed, y)| {
                let (dx, dz) = (x - me.body.pos.x, z - me.body.pos.z);
                ((x, z, heading, speed, y), (dx * dx + dz * dz).sqrt())
            })
            .min_by(|a, b| a.1.total_cmp(&b.1));

        let (mut turn, mut thrust, mut fire, aim_pitch) = match target {
            Some(((hx, hz, hheading, hspeed, hy), d)) => {
                // Lead the shot: aim where the hog will be when the
                // bullet arrives, not where it is — the pod carries its
                // velocity. (Humans lead by watching tracers; a bot that
                // insta-aims at the current bearing only ever hits hogs
                // charging straight down the ray.)
                let tof = d / p.bullet_speed;
                let (lx, lz) = (
                    hx + hheading.sin() * hspeed * tof,
                    hz + hheading.cos() * hspeed * tof,
                );
                let bearing = (lx - me.body.pos.x).atan2(lz - me.body.pos.z);
                let err = wrap_angle(bearing - me.heading());
                // Only pull the trigger when the hog's body actually
                // subtends the aim error (with slack) — a fixed gate
                // either never hits at range or wastes every shot.
                let aligned = err.abs() < (HOG_R / d.max(2.0)).atan() * 2.0;
                // Elevation solution off the barrel hinge; a flyer
                // outside the turret stops (straight overhead) simply
                // isn't fired at — it comes back into the envelope on
                // its swoop or as the truck opens distance.
                let climb = ((hy - 1.45) / d.max(2.0)).atan();
                let feasible = (-TRUCK_AIM_DOWN..=TRUCK_AIM_UP).contains(&climb);
                // GRIP-PHYSICS throttle: momentum carries now, so the
                // old two-speed throttle glided into every close hog
                // and ate the bite. Chase a TARGET SPEED instead —
                // proportional standoff close in (backing up point
                // blank), full chase far out — and slow to turn: a big
                // bearing error caps the target so the chassis rotates
                // ahead of the momentum instead of orbiting the hog.
                // Standoff at 12u: outside a bite lunge, inside easy
                // gun range — and full reverse when a charge closes
                // (soak data: parking at spit distance = death by bite).
                let mut want = ((d - 12.0) * 0.9).clamp(-7.0, p.vmax);
                if err.abs() > 0.9 {
                    want = want.min(7.0);
                }
                (
                    err.clamp(-1.0, 1.0),
                    ((want - me.speed()) * 0.4).clamp(-1.0, 1.0),
                    (aligned
                        && feasible
                        && d < p.gun_range * 0.95
                        && line_clear(me.body.pos.x, 1.45, me.body.pos.z, bearing, climb, d))
                        as i32 as f32,
                    climb.clamp(-TRUCK_AIM_DOWN, TRUCK_AIM_UP),
                )
            }
            // Wave's dead: lazy sine wander until the next one lands.
            None => ((t * 0.43).sin() * 0.8, 0.6, 0.0, 0.0),
        };
        // Wall recovery beats the hunt (drive's lesson: a pure chaser
        // grinds a wall forever).
        let r = (me.body.pos.x * me.body.pos.x + me.body.pos.z * me.body.pos.z).sqrt();
        if r > ARENA - 10.0 {
            let home = (-me.body.pos.x).atan2(-me.body.pos.z);
            turn = wrap_angle(home - me.heading()).clamp(-1.0, 1.0);
            thrust = 0.7;
            fire = 0.0;
        }
        // RACE missions: bots are teammates — they run the loop with
        // you instead of brawling (the mission's whole lesson). This
        // outranks wall recovery on purpose: the rim beacons sit inside
        // its trigger zone and any vehicle can score the co-op beacon;
        // the jam back-out below still saves a bot off a building face.
        let sb = w.hunt.get();
        if sb.phase == PHASE_PLAYING && sb.kind == MISSION_RACE {
            let (cx, cz) = RACE_LOOP[sb.done as usize % RACE_LOOP.len()];
            let bearing = (cx - me.body.pos.x).atan2(cz - me.body.pos.z);
            turn = wrap_angle(bearing - me.heading()).clamp(-1.0, 1.0);
            thrust = 1.0;
            fire = 0.0;
        }
        // Building jam beats even that: commanding thrust while going
        // nowhere means a wall is eating it — back out turning, then
        // resume the chase from the new angle.
        if back > 0.0 {
            back -= FIXED_DT;
            thrust = 0.7 * back_dir;
            turn = 1.0;
            fire = 0.0;
        } else {
            // Standoff drives REVERSE now too — a wall can eat either
            // direction, so jam on commanded thrust of any sign and
            // back out the opposite way.
            if thrust.abs() > 0.2 && me.speed().abs() < 1.0 {
                jam += FIXED_DT;
            } else {
                jam = 0.0;
            }
            if jam > 0.8 {
                jam = 0.0;
                back = 1.2;
                back_dir = -thrust.signum();
            }
        }
        w.input.set(Drive {
            thrust,
            turn,
            fire,
            aim: 0.0,   // bots shoot over the hood
            boost: 0.0, // and drive responsibly
            bot: 1.0,   // AI: steering lags
            pitch: 0.0, // bots keep their wheels on the ground
            lift: 0.0,
            aim_pitch, // turret elevation onto the chosen target
        });
    });

    // TODO(refactor): duplicated in server.rs — share one prof task fn.
    // PM_PROF=1: where a CLIENT's tick goes (prediction, the two interp
    // pools riding the horde) — bot 0 only, so the dumps don't
    // interleave. The corrections line is the PREDICTION health gauge:
    // it should stay flat while driving; a live shared-step param write
    // may step it once, never stream (the documented shared-step blip).
    if n == 0 && std::env::var("PM_PROF").is_ok() {
        let mut prev: std::collections::HashMap<String, pm::TaskStat> = Default::default();
        let pred = w2_pred.clone();
        let pred_heli = w2_pred_heli.clone();
        pm::task!(pm, "prof", 91.0, 5.0, [], move |pm| {
            eprintln!(
                "-- bot0 task stats (last 5s) --  corrections={}",
                pred.get().corrections + pred_heli.get().corrections
            );
            let mut tick_total = 0.0f32;
            for (name, s) in pm.task_stats() {
                let p = prev.get(&name).cloned().unwrap_or_default();
                let calls = s.calls - p.calls;
                if calls > 0 {
                    let avg_us = (s.ns_total - p.ns_total) as f32 / calls as f32 / 1000.0;
                    tick_total += (s.ns_total - p.ns_total) as f32 / 300.0 / 1000.0;
                    eprintln!(
                        "  {name:<16} {avg_us:>8.1} us/call  {calls:>5} calls  max {:>8.1} us",
                        s.ns_max as f32 / 1000.0
                    );
                }
                prev.insert(name, s);
            }
            eprintln!("  ~{tick_total:.0} us/tick of the 16667 us budget");
        });
    }

    pm.loop_rate = 60;
    pm.run().expect("connect");
}
