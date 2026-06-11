//! Hellfire authoritative server: headless, owns all gameplay. Clients
//! send command-frame input; everything they see is replicated pool
//! state. Port of hellfire_server.cpp.

use std::collections::HashMap;

use pm::{Id, NetServer, Pm, QuicServer, Rng, SpatialGrid, Vec2, vec2};

use crate::common::*;

#[derive(Default)]
struct Game {
    time: f32,
    spawn_accum: f32,
    score: i32,
    kills: i32,
    level: usize,
    level_flash: f32,
    level_hold: f32,
    round: u32,
    started: bool,
    game_over: bool,
    win: bool,
    rng: Rng,
    players: HashMap<u8, Id>,
    rosters: HashMap<u8, Id>,
    axes: HashMap<u8, InputCmd>,
    grid: SpatialGrid,
    win_score: i32,
    samples: Vec<String>,
    events: Vec<String>,
    peak_monsters: usize,
    peak_bullets: usize,
    report_written: bool,
}

fn json_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

fn report_dir() -> String {
    std::env::var("HELLFIRE_REPORT_DIR").unwrap_or_else(|_| "work/reports".into())
}

pub fn run(quiet: bool) {
    let mut pm = Pm::new();
    let player = pm.pool_get::<Player>("player");
    let monster = pm.pool_get::<Monster>("monster");
    let bullet = pm.pool_get::<Bullet>("bullet");
    let status = pm.single::<Status>("status");
    let roster = pm.pool_get::<Roster>("roster");
    let dbg = pm.single::<Dbg>("dbg");
    // Server-only companion pools, keyed by the same ids.
    let player_srv = pm.pool_get::<PlayerSrv>("player_srv");
    let monster_srv = pm.pool_get::<MonsterSrv>("monster_srv");
    let bullet_srv = pm.pool_get::<BulletSrv>("bullet_srv");

    let mut net = NetServer::new(&mut pm);
    net.pool_sync("player", &player);
    net.pool_sync("monster", &monster);
    net.pool_sync("bullet", &bullet);
    net.pool_sync("status", status.pool());
    net.pool_sync("dbg", dbg.pool());
    net.pool_sync("roster", &roster);

    let quic = QuicServer::bind(ADDR, &net.schema()).unwrap_or_else(|e| {
        eprintln!("cannot bind {ADDR}: {e}");
        eprintln!("(a previous hellfire may still be running: pkill -x hellfire)");
        std::process::exit(1);
    });
    if !quiet {
        eprintln!("hellfire server on {ADDR}");
    }
    // Receive (prio 15) and send (prio 95) are separate tasks bracketing
    // the sim, so they share the endpoint and peer table by handle.
    let quic = std::rc::Rc::new(std::cell::RefCell::new(quic));
    let net = std::rc::Rc::new(std::cell::RefCell::new(net));

    let game = pm.single::<Game>("game");
    {
        let mut g = game.borrow_mut();
        g.grid = SpatialGrid::new(W, H, 64.0);
        g.rng = Rng::new(42);
        g.win_score = std::env::var("HELLFIRE_WIN_SCORE")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(WIN_SCORE);
    }

    // --- net receive: joins, leaves, input, acks, events (prio 15) ----
    pm.task_fn("net_recv", 15.0, {
        let game = game.clone();
        let player = player.clone();
        let player_srv = player_srv.clone();
        let roster = roster.clone();
        let monster = monster.clone();
        let bullet = bullet.clone();
        let quic = quic.clone();
        let net = net.clone();
        move |pm| {
            let mut quic = quic.borrow_mut();
            let mut net = net.borrow_mut();
            quic.pump();
            let mut g = game.borrow_mut();
            for p in quic.joined_drain() {
                net.peer_add(p);
                let rid = pm.id_add();
                roster.borrow_mut().add(rid, Roster::new(p, &format!("P{p}")));
                g.rosters.insert(p, rid);
                let i = spawn_index(p);
                let pid = pm.id_add();
                let c = PCOL[i];
                player.borrow_mut().add(
                    pid,
                    Player {
                        pos: vec2(SPAWN_X[i], SPAWN_Y[i]),
                        hp: PLAYER_HP,
                        peer: p as u32,
                        alive: 1,
                        color: [c[0], c[1], c[2], 255],
                    },
                );
                player_srv.borrow_mut().add(pid, PlayerSrv { cooldown: 0.0, invuln: 2.0 });
                g.players.insert(p, pid);
                if !quiet {
                    eprintln!("[server] peer {p} joined");
                }
            }
            for p in quic.left_drain() {
                net.peer_remove(p);
                g.axes.remove(&p);
                if let Some(id) = g.players.remove(&p) {
                    pm.id_remove(id);
                }
                if let Some(id) = g.rosters.remove(&p) {
                    pm.id_remove(id);
                }
                if !quiet {
                    eprintln!("[server] peer {p} left");
                }
            }
            for (p, seq, bytes) in quic.inputs_drain() {
                if bytes.len() == size_of::<InputCmd>() {
                    g.axes.insert(p, bytemuck::pod_read_unaligned(&bytes));
                    net.input_processed(p, seq);
                }
            }
            for (p, tick) in quic.acks_drain() {
                net.ack(p, tick);
            }
            for (p, ty, payload) in quic.events_drain() {
                match ty {
                    EV_NAME => {
                        if let (Some(&rid), Ok(name)) =
                            (g.rosters.get(&p), std::str::from_utf8(&payload))
                        {
                            roster.borrow_mut().add(rid, Roster::new(p, name));
                        }
                    }
                    EV_START if !g.started => {
                        g.started = true;
                        g.events.push(format!(
                            "{{\"t\": {:.1}, \"event\": \"started with {} players\"}}",
                            g.time,
                            g.players.len()
                        ));
                        if !quiet {
                            eprintln!("[server] game started ({} players)", g.players.len());
                        }
                    }
                    EV_RESTART if g.game_over => {
                        restart(pm, &mut g, &player, &player_srv, &monster, &bullet);
                        if !quiet {
                            eprintln!("[server] restart (round {})", g.round);
                        }
                    }
                    _ => {}
                }
            }
        }
    });

    // --- player movement + shooting (prio 29) --------------------------
    pm.task_fn("player_move", 29.0, {
        let game = game.clone();
        let player = player.clone();
        let player_srv = player_srv.clone();
        let bullet = bullet.clone();
        let bullet_srv = bullet_srv.clone();
        move |pm| {
            let g = game.borrow();
            if !g.started || g.game_over {
                return;
            }
            let dt = pm.loop_dt();
            let mut spawned = Vec::new();
            {
                let mut players = player.borrow_mut();
                let mut srv = player_srv.borrow_mut();
                for (&peer, &pid) in &g.players {
                    let Some(mut p) = players.get_mut(pid) else { continue };
                    if p.alive == 0 {
                        continue;
                    }
                    let cmd = g.axes.get(&peer).copied().unwrap_or_default();
                    let mv = vec2(cmd.dx, cmd.dy).norm();
                    if mv != Vec2::ZERO {
                        let hs = PLAYER_SIZE * 0.5;
                        let next = p.pos + mv * (PLAYER_SPEED * dt);
                        p.pos = vec2(next.x.clamp(hs, W - hs), next.y.clamp(hs, H - hs));
                    }
                    let pos = p.pos;
                    let Some(mut s) = srv.get_mut(pid) else { continue };
                    s.cooldown -= dt;
                    s.invuln -= dt;
                    if cmd.buttons & BTN_SHOOT != 0 && s.cooldown <= 0.0 {
                        s.cooldown = PLAYER_COOLDOWN;
                        let mut aim = (vec2(cmd.ax, cmd.ay) - pos).norm();
                        if aim == Vec2::ZERO {
                            aim = vec2(1.0, 0.0);
                        }
                        spawned.push((pos, aim));
                    }
                }
            }
            for (pos, aim) in spawned {
                let id = pm.id_add();
                bullet.borrow_mut().add(
                    id,
                    Bullet { pos, vel: aim * PBULLET_SPEED, size: PBULLET_SIZE, player_owned: 1 },
                );
                bullet_srv.borrow_mut().add(id, BulletSrv { life: PBULLET_LIFE });
            }
        }
    });

    // --- spawning + level progression (prio 28) ------------------------
    pm.task_fn("spawn", 28.0, {
        let game = game.clone();
        let player = player.clone();
        let player_srv = player_srv.clone();
        let monster = monster.clone();
        let monster_srv = monster_srv.clone();
        let bullet = bullet.clone();
        move |pm| {
            let mut g = game.borrow_mut();
            if !g.started || g.game_over {
                return;
            }
            let dt = pm.loop_dt();
            g.time += dt;
            g.level_flash = (g.level_flash - dt).max(0.0);
            if g.level_hold > 0.0 {
                g.level_hold -= dt;
                return;
            }
            let next = g.level + 1;
            if next < LEVELS.len() && g.score >= LEVELS[next].threshold {
                g.level = next;
                g.round += 1;
                g.spawn_accum = 0.0;
                g.level_flash = 3.0;
                g.level_hold = 3.0;
                // Breather: clear the field, regroup the living.
                clear_pool_ids(pm, &monster);
                clear_pool_ids(pm, &bullet);
                let entries: Vec<(u8, Id)> = g.players.iter().map(|(&p, &id)| (p, id)).collect();
                let mut players = player.borrow_mut();
                let mut srv = player_srv.borrow_mut();
                for (peer, pid) in entries {
                    let (jx, jy) = (g.rng.rfr(-80.0, 80.0), g.rng.rfr(-60.0, 60.0));
                    if let Some(mut p) = players.get_mut(pid)
                        && p.alive == 1
                    {
                        let i = spawn_index(peer);
                        p.pos = vec2(SPAWN_X[i] + jx, SPAWN_Y[i] + jy);
                        if let Some(mut s) = srv.get_mut(pid) {
                            s.invuln = 2.0;
                        }
                    }
                }
                if !quiet {
                    eprintln!("[server] level {}", g.level + 1);
                }
                return;
            }
            if g.score >= g.win_score && !g.win {
                g.win = true;
                g.game_over = true;
                clear_pool_ids(pm, &monster);
                clear_pool_ids(pm, &bullet);
                return;
            }
            let lvl = &LEVELS[g.level];
            if monster.borrow().len() >= lvl.max_monsters {
                return;
            }
            let intensity = ((g.time * 0.4).sin() * 0.5 + 0.5) * 0.6
                + ((g.time * 0.08).sin() * 0.5 + 0.5) * 0.4;
            g.spawn_accum += (1.0 + 8.0 * intensity) * lvl.spawn_mult * dt;
            while g.spawn_accum >= 1.0 && monster.borrow().len() < lvl.max_monsters {
                g.spawn_accum -= 1.0;
                let lvl = &LEVELS[g.level];
                let speed = MONSTER_SPEED
                    * (0.8 + intensity * 0.6)
                    * (lvl.speed_mult * lvl.size_mult).min(3.0);
                let size = g.rng.rfr(MONSTER_MIN_SZ * lvl.size_mult, MONSTER_MAX_SZ * lvl.size_mult);
                let pos = match g.rng.next_u32() % 4 {
                    0 => vec2(g.rng.rfr(0.0, W), -30.0),
                    1 => vec2(g.rng.rfr(0.0, W), H + 30.0),
                    2 => vec2(-30.0, g.rng.rfr(0.0, H)),
                    _ => vec2(W + 30.0, g.rng.rfr(0.0, H)),
                };
                let tgt = vec2(W * 0.5 + g.rng.rfr(-200.0, 200.0), H * 0.5 + g.rng.rfr(-200.0, 200.0));
                let hue = g.rng.rf();
                let c: [u8; 3] = if hue < 0.3 {
                    [255, 80, 60]
                } else if hue < 0.5 {
                    [255, 140, 40]
                } else if hue < 0.7 {
                    [255, 60, 120]
                } else if hue < 0.85 {
                    [200, 50, 200]
                } else {
                    [255, 200, 50]
                };
                let shoot_timer = g.rng.rfr(1.5, 4.0);
                let id = pm.id_add();
                monster.borrow_mut().add(
                    id,
                    Monster {
                        pos,
                        vel: (tgt - pos).norm() * speed,
                        size,
                        color: [c[0], c[1], c[2], 255],
                    },
                );
                monster_srv.borrow_mut().add(id, MonsterSrv { shoot_timer });
            }
        }
    });

    // --- bullet physics (prio 30) ---------------------------------------
    pm.task_fn("bullet_phys", 30.0, {
        let game = game.clone();
        let bullet = bullet.clone();
        let bullet_srv = bullet_srv.clone();
        move |pm| {
            let g = game.borrow();
            if !g.started || g.game_over {
                return;
            }
            let dt = pm.loop_dt();
            let mut dead = Vec::new();
            {
                let mut bullets = bullet.borrow_mut();
                let mut srv = bullet_srv.borrow_mut();
                for (id, mut b) in bullets.iter_mut() {
                    let next = Bullet { pos: b.pos + b.vel * dt, ..*b };
                    *b = next;
                    if let Some(mut s) = srv.get_mut(id) {
                        s.life -= dt;
                        if s.life <= 0.0 {
                            dead.push(id);
                        }
                    }
                }
            }
            for id in dead {
                pm.id_remove(id);
            }
        }
    });

    // --- monster AI (prio 31) --------------------------------------------
    pm.task_fn("monster_ai", 31.0, {
        let game = game.clone();
        let player = player.clone();
        let monster = monster.clone();
        let monster_srv = monster_srv.clone();
        let bullet = bullet.clone();
        let bullet_srv = bullet_srv.clone();
        move |pm| {
            let mut g = game.borrow_mut();
            if !g.started || g.game_over {
                return;
            }
            let dt = pm.loop_dt();
            let alive: Vec<Vec2> = player
                .borrow()
                .values()
                .iter()
                .filter(|p| p.alive == 1)
                .map(|p| p.pos)
                .collect();
            let mut shots = Vec::new();
            {
                let mut monsters = monster.borrow_mut();
                let mut srv = monster_srv.borrow_mut();
                for (id, mut m) in monsters.iter_mut() {
                    let (mut tgt, mut best) = (m.pos, f32::MAX);
                    for &p in &alive {
                        let d = m.pos.dist(p);
                        if d < best {
                            best = d;
                            tgt = p;
                        }
                    }
                    let desired = (tgt - m.pos).norm() * m.vel.len();
                    let vel = m.vel + (desired - m.vel) * (0.5 * dt);
                    let next = Monster { vel, pos: m.pos + vel * dt, ..*m };
                    *m = next;
                    if let Some(mut s) = srv.get_mut(id) {
                        s.shoot_timer -= dt;
                        if s.shoot_timer <= 0.0 && best < 500.0 {
                            s.shoot_timer = g.rng.rfr(2.0, 5.0);
                            let dir = (tgt - m.pos).norm();
                            let sp = g.rng.rfr(-0.15, 0.15);
                            let (cs, sn) = (sp.cos(), sp.sin());
                            let aim = vec2(dir.x * cs - dir.y * sn, dir.x * sn + dir.y * cs);
                            shots.push((m.pos, aim));
                        }
                    }
                }
            }
            for (pos, aim) in shots {
                let id = pm.id_add();
                bullet.borrow_mut().add(
                    id,
                    Bullet { pos, vel: aim * MBULLET_SPEED, size: MBULLET_SIZE, player_owned: 0 },
                );
                bullet_srv.borrow_mut().add(id, BulletSrv { life: MBULLET_LIFE });
            }
        }
    });

    // --- collision (prio 50) ----------------------------------------------
    pm.task_fn("collision", 50.0, {
        let game = game.clone();
        let player = player.clone();
        let player_srv = player_srv.clone();
        let monster = monster.clone();
        let bullet = bullet.clone();
        move |pm| {
            let mut g = game.borrow_mut();
            if !g.started || g.game_over {
                return;
            }
            let pr = PLAYER_SIZE * 0.5;
            let query_r = PBULLET_SIZE + MONSTER_MAX_SZ * 1.2 * 0.65;

            let g = &mut *g; // split-borrow grid vs rng/score
            g.grid.clear();
            for (id, m) in monster.borrow().iter() {
                g.grid.insert(id, m.pos);
            }

            // Player bullets vs monsters (broad phase via grid).
            let mut dead = Vec::new();
            {
                let monsters = monster.borrow();
                for (bid, b) in bullet.borrow().iter() {
                    if b.player_owned == 0 {
                        continue;
                    }
                    let mut hit = false;
                    let (score, kills) = (&mut g.score, &mut g.kills);
                    g.grid.query(b.pos, query_r, |mid, _| {
                        if hit {
                            return;
                        }
                        let Some(m) = monsters.get(mid) else { return };
                        if b.pos.dist(m.pos) < b.size + m.size * 0.5 {
                            dead.push(mid);
                            dead.push(bid);
                            *score += 10;
                            *kills += 1;
                            hit = true;
                        }
                    });
                }
            }

            // Players vs enemy bullets and monster contact.
            {
                let mut players = player.borrow_mut();
                let mut srv = player_srv.borrow_mut();
                let bullets = bullet.borrow();
                let monsters = monster.borrow();
                for &pid in g.players.values() {
                    let Some(mut p) = players.get_mut(pid) else { continue };
                    if p.alive == 0 {
                        continue;
                    }
                    let Some(mut s) = srv.get_mut(pid) else { continue };
                    if s.invuln > 0.0 {
                        continue;
                    }
                    for (bid, b) in bullets.iter() {
                        if b.player_owned == 0 && b.pos.dist(p.pos) < b.size + pr {
                            p.hp -= BULLET_DMG;
                            s.invuln = PLAYER_INVULN;
                            dead.push(bid);
                        }
                    }
                    for (_, m) in monsters.iter() {
                        if m.pos.dist(p.pos) < m.size * 0.5 + pr {
                            p.hp -= CONTACT_DMG;
                            s.invuln = PLAYER_INVULN * 0.5;
                        }
                    }
                    if p.hp <= 0.0 {
                        p.hp = 0.0;
                        p.alive = 0;
                    }
                }
            }
            for id in dead {
                pm.id_remove(id);
            }

            let players = player.borrow();
            let any_alive = players.values().iter().any(|p| p.alive == 1);
            if !any_alive && !players.is_empty() {
                g.game_over = true;
                drop(players);
                clear_pool_ids(pm, &monster);
                clear_pool_ids(pm, &bullet);
            }
        }
    });

    // --- cleanup out-of-bounds (prio 55) ---------------------------------
    pm.task_fn("cleanup", 55.0, {
        let game = game.clone();
        let monster = monster.clone();
        let bullet = bullet.clone();
        move |pm| {
            if !game.borrow().started {
                return;
            }
            let mut dead = Vec::new();
            for (id, m) in monster.borrow().iter() {
                if m.pos.x < -100.0 || m.pos.x > W + 100.0 || m.pos.y < -100.0 || m.pos.y > H + 100.0
                {
                    dead.push(id);
                }
            }
            for (id, b) in bullet.borrow().iter() {
                if b.pos.x < -50.0 || b.pos.x > W + 50.0 || b.pos.y < -50.0 || b.pos.y > H + 50.0 {
                    dead.push(id);
                }
            }
            for id in dead {
                pm.id_remove(id);
            }
        }
    });

    // --- publish status (prio 60): write only on change -------------------
    pm.task_fn("status_pub", 60.0, {
        let game = game.clone();
        let status = status.clone();
        move |_pm| {
            let mut g = game.borrow_mut();
            let mut flags = 0;
            if g.started {
                flags |= FLAG_STARTED;
            }
            if g.game_over {
                flags |= FLAG_GAME_OVER;
            }
            if g.win {
                flags |= FLAG_WIN;
            }
            let next = Status {
                time: (g.time * 10.0).round() / 10.0, // 0.1s granularity: don't re-sync every tick
                score: g.score,
                kills: g.kills,
                level: g.level as i32,
                round: g.round,
                flags,
                level_flash: (g.level_flash * 4.0).round() / 4.0,
            };
            if *status.borrow() != next {
                *status.borrow_mut() = next;
            }
            if g.game_over && !g.report_written {
                g.report_written = true;
                let dir = report_dir();
                let _ = std::fs::create_dir_all(&dir);
                let path = format!("{dir}/server.json");
                let json = format!(
                    "{{\n  \"role\": \"server\",\n  \"duration\": {:.1},\n  \"score\": {},\n  \"kills\": {},\n  \"level\": {},\n  \"round\": {},\n  \"win\": {},\n  \"game_over\": true,\n  \"players\": {},\n  \"peak_monsters\": {},\n  \"peak_bullets\": {},\n  \"events\": [{}],\n  \"samples\": [{}]\n}}\n",
                    g.time,
                    g.score,
                    g.kills,
                    g.level + 1,
                    g.round,
                    g.win,
                    g.players.len(),
                    g.peak_monsters,
                    g.peak_bullets,
                    g.events.join(", "),
                    g.samples.join(", "),
                );
                match std::fs::write(&path, json) {
                    Ok(()) => eprintln!("[server] report written to {path}"),
                    Err(e) => eprintln!("[server] report write failed: {e}"),
                }
            }
        }
    });

    // --- replicated diagnostics + diag sampling (1 Hz, prio 61) -----------
    pm.task_fn_every("dbg_pub", 61.0, 1.0, {
        let game = game.clone();
        let dbg = dbg.clone();
        let monster = monster.clone();
        let bullet = bullet.clone();
        move |pm| {
            let (m, b) = (monster.borrow().len(), bullet.borrow().len());
            *dbg.borrow_mut() =
                Dbg { monsters: m as u32, bullets: b as u32, tick_ms: pm.loop_dt() * 1000.0 };
            let mut g = game.borrow_mut();
            g.peak_monsters = g.peak_monsters.max(m);
            g.peak_bullets = g.peak_bullets.max(b);
            if g.started && !g.game_over {
                let alive =
                    g.players.len(); // connected; per-player alive is in the player pool
                let sample = format!(
                    "{{\"t\": {:.1}, \"monsters\": {m}, \"bullets\": {b}, \"players\": {alive}, \"score\": {}, \"frame_ms\": {:.2}}}",
                    g.time, g.score, pm.loop_dt() * 1000.0,
                );
                g.samples.push(sample);
            }
        }
    });

    // --- snapshots out (prio 95) -------------------------------------------
    pm.task_fn("net_send", 95.0, {
        let quic = quic.clone();
        let net = net.clone();
        move |pm| {
            let mut quic = quic.borrow_mut();
            let mut net = net.borrow_mut();
            let peers: Vec<u8> = net.peers().collect();
            for p in peers {
                let budget = quic.snapshot_budget(p);
                if let Some(snap) = net.snapshot_budgeted(pm, p, budget) {
                    quic.snapshot_send(p, &snap);
                }
            }
            net.prune(pm);
        }
    });

    if !quiet {
        pm.task_fn_every("status_print", 99.0, 5.0, {
            let game = game.clone();
            let monster = monster.clone();
            let bullet = bullet.clone();
            move |_pm| {
                let g = game.borrow();
                if g.started {
                    eprintln!(
                        "[server] t={:.0} score={} lvl={} m={} b={} players={} {}",
                        g.time,
                        g.score,
                        g.level + 1,
                        monster.borrow().len(),
                        bullet.borrow().len(),
                        g.players.len(),
                        if g.game_over { if g.win { "WIN" } else { "GAME OVER" } } else { "" },
                    );
                }
            }
        });
    }

    pm.loop_rate = 60;
    pm.loop_run();
}

fn restart(
    pm: &mut Pm,
    g: &mut Game,
    player: &std::rc::Rc<std::cell::RefCell<pm::Pool<Player>>>,
    player_srv: &std::rc::Rc<std::cell::RefCell<pm::Pool<PlayerSrv>>>,
    monster: &std::rc::Rc<std::cell::RefCell<pm::Pool<Monster>>>,
    bullet: &std::rc::Rc<std::cell::RefCell<pm::Pool<Bullet>>>,
) {
    clear_pool_ids(pm, monster);
    clear_pool_ids(pm, bullet);
    g.time = 0.0;
    g.spawn_accum = 0.0;
    g.score = 0;
    g.kills = 0;
    g.level = 0;
    g.level_flash = 0.0;
    g.level_hold = 0.0;
    g.round += 1;
    g.game_over = false;
    g.win = false;
    let mut players = player.borrow_mut();
    let mut srv = player_srv.borrow_mut();
    for (&peer, &pid) in &g.players {
        if let Some(mut p) = players.get_mut(pid) {
            let i = spawn_index(peer);
            p.pos = vec2(SPAWN_X[i], SPAWN_Y[i]);
            p.hp = PLAYER_HP;
            p.alive = 1;
        }
        if let Some(mut s) = srv.get_mut(pid) {
            s.cooldown = 0.0;
            s.invuln = 2.0;
        }
    }
}

/// Remove every entity in a pool via the kernel (deferred, replicated
/// through the removal log). The future `remove_all` kernel helper.
fn clear_pool_ids<T>(pm: &mut Pm, pool: &std::rc::Rc<std::cell::RefCell<pm::Pool<T>>>) {
    let ids: Vec<Id> = pool.borrow().ids().to_vec();
    for id in ids {
        pm.id_remove(id);
    }
}
