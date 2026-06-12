//! Hellfire client side: shared netcode + smoothing for every client
//! flavor (SDL player, headless bot), plus the bot's input task. The SDL
//! window/input/render live in sdl_client.rs.


use pm::{NetClient, Outbox, Pm, QuicClient, Rng, coast_blend, pool_mirror, vec2};

use crate::common::*;

#[derive(Default)]
pub struct CurInput(pub InputCmd);

/// Client camera: world-space center + zoom, shared by input (mouse ->
/// world aim) and render (world -> screen). Only the SDL client builds
/// use it; bots aim in raw world coords.
#[derive(Clone, Copy)]
pub struct Camera {
    pub center: pm::Vec2,
    pub zoom: f32,
    pub target_zoom: f32,
}

impl Default for Camera {
    fn default() -> Self {
        Self { center: vec2(W * 0.5, H * 0.5), zoom: 1.0, target_zoom: 1.0 }
    }
}

impl Camera {
    pub fn to_screen(self, p: pm::Vec2) -> (f32, f32) {
        (
            (p.x - self.center.x) * self.zoom + W * 0.5,
            (p.y - self.center.y) * self.zoom + H * 0.5,
        )
    }

    pub fn to_world(self, sx: f32, sy: f32) -> pm::Vec2 {
        vec2((sx - W * 0.5) / self.zoom + self.center.x, (sy - H * 0.5) / self.zoom + self.center.y)
    }
}

/// Client-side UI toggles (debug overlay etc.).
#[derive(Default)]
pub struct Ui {
    pub show_debug: bool,
}

#[derive(Default)]
pub struct NetStats {
    pub peer: u8,
    pub rtt_ms: f32,
    pub snapshots: u32,
}

pub struct Pools {
    pub player: pm::Handle<Player>,
    pub monster: pm::Handle<Monster>,
    pub bullet: pm::Handle<Bullet>,
    pub status: pm::Handle<Status>,
    pub dbg: pm::Handle<Dbg>,
    pub roster: pm::Handle<Roster>,
    pub monster_draw: pm::Handle<Monster>,
    pub bullet_draw: pm::Handle<Bullet>,
    pub player_draw: pm::Handle<Player>,
}

pub fn client_pools(pm: &mut Pm) -> (Pools, NetClient) {
    let pools = Pools {
        player: pm.pool("player"),
        monster: pm.pool("monster"),
        bullet: pm.pool("bullet"),
        status: pm.pool("status"),
        dbg: pm.pool("dbg"),
        roster: pm.pool("roster"),
        monster_draw: pm.pool("monster_draw"),
        bullet_draw: pm.pool("bullet_draw"),
        player_draw: pm.pool("player_draw"),
    };
    let mut net = NetClient::new();
    net.pool_sync("player", &pools.player);
    net.pool_sync("monster", &pools.monster);
    net.pool_sync("bullet", &pools.bullet);
    net.pool_sync("status", &pools.status);
    net.pool_sync("dbg", &pools.dbg);
    net.pool_sync("roster", &pools.roster);
    (pools, net)
}

pub fn current_status(status: &pm::Handle<Status>) -> Status {
    status.borrow().values().first().copied().unwrap_or_default()
}

/// Net + smoothing tasks shared by player and bot clients. The input
/// layer (SDL or bot) writes `CurInput`; rendering reads the draw pools.
pub fn add_client_tasks(pm: &mut Pm, mut quic: QuicClient, net: NetClient, pools: &Pools, name: String) {
    let cmd = pm.single::<CurInput>("cmd");
    let stats = pm.single::<NetStats>("net_stats");
    let outbox = pm.single::<Outbox>("outbox");

    let report_name = name.clone();
    pm.task_add("net", 5.0, {
        let cmd = cmd.clone();
        let stats = stats.clone();
        let outbox = outbox.clone();
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
                // Reliable events queued by any task (input, UI, bots,
                // mods) — drained here by the one owner of the socket.
                for (ty, payload) in outbox.borrow_mut().drain() {
                    quic.event_send(ty, &payload);
                }
            }
            // Restart request rides the reliable stream (a true must-see
            // instant — not state).
            stats.borrow_mut().rtt_ms = quic.rtt().as_secs_f32() * 1e3;
        }
    });

    // One-shot diag report when the game ends (HELLFIRE_REPORT_DIR).
    pm.task_add_every("diag", 90.0, 0.5, {
        let status = pools.status.clone();
        let stats = stats.clone();
        let name = report_name;
        let mut written = false;
        move |_pm| {
            let st = current_status(&status);
            if st.flags & FLAG_GAME_OVER == 0 || written {
                return;
            }
            written = true;
            let dir =
                std::env::var("HELLFIRE_REPORT_DIR").unwrap_or_else(|_| "target/work/reports".into());
            let _ = std::fs::create_dir_all(&dir);
            let s = stats.borrow();
            let json = format!(
                "{{\n  \"role\": \"client\",\n  \"name\": \"{name}\",\n  \"peer\": {},\n  \"snapshots\": {},\n  \"rtt_ms\": {:.1},\n  \"score\": {},\n  \"win\": {}\n}}\n",
                s.peer,
                s.snapshots,
                s.rtt_ms,
                st.score,
                st.flags & FLAG_WIN != 0,
            );
            let _ = std::fs::write(format!("{dir}/{name}.json"), json);
        }
    });

    // Dead-reckon the draw pools between (budget-rotated) refreshes:
    // pm::pool_mirror handles add/blend/stale-drop; the closures are
    // just the per-type blend math.
    pm.task_add("smooth", 30.0, {
        let pools_m = pools.monster.clone();
        let pools_b = pools.bullet.clone();
        let pools_p = pools.player.clone();
        let draw_m = pools.monster_draw.clone();
        let draw_b = pools.bullet_draw.clone();
        let draw_p = pools.player_draw.clone();
        move |pm| {
            let dt = pm.loop_dt();
            pool_mirror(&pools_m, &draw_m, |_, d, a: &Monster| Monster {
                pos: coast_blend(d.pos, d.vel, a.pos, dt, 0.15),
                ..*a
            });
            pool_mirror(&pools_b, &draw_b, |_, d, a: &Bullet| Bullet {
                pos: coast_blend(d.pos, d.vel, a.pos, dt, 0.15),
                ..*a
            });
            pool_mirror(&pools_p, &draw_p, |_, d, a: &Player| {
                let snap = d.pos.dist(a.pos) > 120.0; // respawn/level jump
                let pos = if snap { a.pos } else { d.pos + (a.pos - d.pos) * 0.35 };
                Player { pos, ..*a }
            });
        }
    });
}

/// Headless bot: wanders, hunts the nearest monster it can see in its
/// own replicated state, restarts the game when it ends.
pub fn run_bot(n: u32) {
    let mut pm = Pm::new();
    let (pools, net) = client_pools(&mut pm);
    let quic = QuicClient::connect(ADDR, &net.schema()).expect("bot connect");
    add_client_tasks(&mut pm, quic, net, &pools, format!("bot{n}"));

    let cmd = pm.single::<CurInput>("cmd");
    let outbox = pm.single::<Outbox>("outbox");
    pm.task_add("bot", 4.0, {
        let monster = pools.monster.clone();
        let player = pools.player.clone();
        let status = pools.status.clone();
        let outbox = outbox.clone();
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
                outbox.borrow_mut().send(EV_RESTART, &[]);
            }
            // Nobody to press ENTER in a bot lobby: start after a beat.
            if st.flags & FLAG_STARTED == 0 && rng.rf() < 0.02 {
                outbox.borrow_mut().send(EV_START, &[]);
            }
        }
    });

    pm.loop_rate = 60;
    pm.loop_run();
}
