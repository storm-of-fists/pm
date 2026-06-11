//! Hellfire client side: shared netcode + smoothing for every client
//! flavor (SDL player, headless bot), plus the bot's input task. The SDL
//! window/input/render live in sdl_client.rs.

use std::cell::RefCell;
use std::rc::Rc;

use pm::{Id, NetClient, Pm, Pool, QuicClient, Rng, vec2};

use crate::common::*;

#[derive(Default)]
pub struct CurInput(pub InputCmd);

#[derive(Default)]
pub struct NetStats {
    pub peer: u8,
    pub rtt_ms: f32,
    pub snapshots: u32,
}

pub struct Pools {
    pub player: Rc<RefCell<Pool<Player>>>,
    pub monster: Rc<RefCell<Pool<Monster>>>,
    pub bullet: Rc<RefCell<Pool<Bullet>>>,
    pub status: Rc<RefCell<Pool<Status>>>,
    pub roster: Rc<RefCell<Pool<Roster>>>,
    pub monster_draw: Rc<RefCell<Pool<Monster>>>,
    pub bullet_draw: Rc<RefCell<Pool<Bullet>>>,
    pub player_draw: Rc<RefCell<Pool<Player>>>,
}

pub fn client_pools(pm: &mut Pm) -> (Pools, NetClient) {
    let pools = Pools {
        player: pm.pool_get("player"),
        monster: pm.pool_get("monster"),
        bullet: pm.pool_get("bullet"),
        status: pm.pool_get("status"),
        roster: pm.pool_get("roster"),
        monster_draw: pm.pool_get("monster_draw"),
        bullet_draw: pm.pool_get("bullet_draw"),
        player_draw: pm.pool_get("player_draw"),
    };
    let mut net = NetClient::new();
    net.pool_sync("player", &pools.player);
    net.pool_sync("monster", &pools.monster);
    net.pool_sync("bullet", &pools.bullet);
    net.pool_sync("status", &pools.status);
    net.pool_sync("roster", &pools.roster);
    (pools, net)
}

pub fn current_status(status: &Rc<RefCell<Pool<Status>>>) -> Status {
    status.borrow().values().first().copied().unwrap_or_default()
}

/// Net + smoothing tasks shared by player and bot clients. The input
/// layer (SDL or bot) writes `CurInput`; rendering reads the draw pools.
pub fn add_client_tasks(pm: &mut Pm, mut quic: QuicClient, net: NetClient, pools: &Pools, name: String) {
    let cmd = pm.single::<CurInput>("cmd");
    let stats = pm.single::<NetStats>("net_stats");

    pm.task_fn("net", 5.0, {
        let cmd = cmd.clone();
        let stats = stats.clone();
        let mut name_sent = false;
        move |pm| {
            quic.pump();
            if let Some(err) = quic.error() {
                eprintln!("disconnected: {err}");
                pm.loop_quit();
                return;
            }
            if quic.is_gone() {
                eprintln!("server closed the connection");
                pm.loop_quit();
                return;
            }
            for snap in quic.snapshots_drain() {
                if let Ok(applied) = net.apply(pm, &snap) {
                    quic.ack_send(applied.tick);
                    stats.borrow_mut().snapshots += 1;
                }
            }
            if let Some(peer) = quic.handshake_done() {
                pm.local_peer = peer;
                stats.borrow_mut().peer = peer;
                if !name_sent {
                    name_sent = true;
                    quic.event_send(EV_NAME, name.as_bytes());
                }
                quic.input_send(bytemuck::bytes_of(&cmd.borrow().0));
            }
            // Restart request rides the reliable stream (a true must-see
            // instant — not state).
            let mut c = cmd.borrow_mut();
            if c.0.buttons & BTN_RESTART != 0 {
                c.0.buttons &= !BTN_RESTART;
                quic.event_send(EV_RESTART, &[]);
            }
            stats.borrow_mut().rtt_ms = quic.rtt().as_secs_f32() * 1e3;
        }
    });

    // Dead-reckon the draw pools between (budget-rotated) refreshes:
    // coast along last known velocity, ease onto fresh server state.
    pm.task_fn("smooth", 30.0, {
        let pools_m = pools.monster.clone();
        let pools_b = pools.bullet.clone();
        let pools_p = pools.player.clone();
        let draw_m = pools.monster_draw.clone();
        let draw_b = pools.bullet_draw.clone();
        let draw_p = pools.player_draw.clone();
        move |pm| {
            let dt = pm.loop_dt();
            let blend = 0.15;

            {
                let auth = pools_m.borrow();
                let mut draw = draw_m.borrow_mut();
                for (id, m) in auth.iter() {
                    match draw.get_mut(id) {
                        Some(mut d) => {
                            // Locals first: `d.pos += d.vel * dt` would need
                            // simultaneous mut+shared derefs of the Mut guard.
                            let coast = d.pos + d.vel * dt;
                            let next = Monster { pos: coast + (m.pos - coast) * blend, ..*m };
                            *d = next;
                        }
                        None => draw.add(id, *m),
                    }
                }
                let stale: Vec<Id> =
                    draw.ids().iter().copied().filter(|&id| !auth.contains(id)).collect();
                for id in stale {
                    draw.remove(id);
                }
            }
            {
                let auth = pools_b.borrow();
                let mut draw = draw_b.borrow_mut();
                for (id, b) in auth.iter() {
                    match draw.get_mut(id) {
                        Some(mut d) => {
                            let coast = d.pos + d.vel * dt;
                            let next = Bullet { pos: coast + (b.pos - coast) * blend, ..*b };
                            *d = next;
                        }
                        None => draw.add(id, *b),
                    }
                }
                let stale: Vec<Id> =
                    draw.ids().iter().copied().filter(|&id| !auth.contains(id)).collect();
                for id in stale {
                    draw.remove(id);
                }
            }
            {
                let auth = pools_p.borrow();
                let mut draw = draw_p.borrow_mut();
                for (id, p) in auth.iter() {
                    match draw.get_mut(id) {
                        Some(mut d) => {
                            let snap = d.pos.dist(p.pos) > 120.0; // respawn/level jump
                            let pos =
                                if snap { p.pos } else { d.pos + (p.pos - d.pos) * 0.35 };
                            let next = Player { pos, ..*p };
                            *d = next;
                        }
                        None => draw.add(id, *p),
                    }
                }
                let stale: Vec<Id> =
                    draw.ids().iter().copied().filter(|&id| !auth.contains(id)).collect();
                for id in stale {
                    draw.remove(id);
                }
            }
        }
    });
}

/// Local pseudo-button: the input layer sets it; the net task converts
/// it to an EV_RESTART event and clears it. Never sent inside InputCmd.
pub const BTN_RESTART: u32 = 1 << 31;

/// Headless bot: wanders, hunts the nearest monster it can see in its
/// own replicated state, restarts the game when it ends.
pub fn run_bot(n: u32) {
    let mut pm = Pm::new();
    let (pools, net) = client_pools(&mut pm);
    let quic = QuicClient::connect(ADDR, &net.schema()).expect("bot connect");
    add_client_tasks(&mut pm, quic, net, &pools, format!("bot{n}"));

    let cmd = pm.single::<CurInput>("cmd");
    pm.task_fn("bot", 4.0, {
        let monster = pools.monster.clone();
        let player = pools.player.clone();
        let status = pools.status.clone();
        let mut rng = Rng::new(1000 + n);
        let mut dir = vec2(1.0, 0.0);
        let mut turn = pm::Cooldown::new(1.0);
        let mut me = None;
        move |pm| {
            let dt = pm.loop_dt();
            let my_peer = pm.local_peer as u32;
            if me.is_none() {
                me = player.borrow().iter().find(|(_, p)| p.peer == my_peer).map(|(id, _)| id);
            }
            let pos = me
                .and_then(|id| player.borrow().get(id).map(|p| p.pos))
                .unwrap_or(vec2(W * 0.5, H * 0.5));
            if turn.ready(dt) {
                turn.interval = rng.rfr(0.6, 1.6);
                dir = vec2(rng.rfr(-1.0, 1.0), rng.rfr(-1.0, 1.0)).norm();
            }
            // Steer back toward the middle near walls.
            if pos.x < 100.0 || pos.x > W - 100.0 || pos.y < 100.0 || pos.y > H - 100.0 {
                dir = (vec2(W * 0.5, H * 0.5) - pos).norm();
            }
            // Aim at the nearest monster in replicated state.
            let (mut aim, mut best) = (pos + dir * 100.0, f32::MAX);
            for (_, m) in monster.borrow().iter() {
                let d = m.pos.dist(pos);
                if d < best {
                    best = d;
                    aim = m.pos;
                }
            }
            let st = current_status(&status);
            let mut c = cmd.borrow_mut();
            c.0 = InputCmd { dx: dir.x, dy: dir.y, ax: aim.x, ay: aim.y, buttons: BTN_SHOOT };
            if st.flags & FLAG_GAME_OVER != 0 && rng.rf() < 0.005 {
                c.0.buttons |= BTN_RESTART;
            }
        }
    });

    pm.loop_rate = 60;
    pm.loop_run();
}
