//! Hellfire client side: pool registration, smoothing, and diag
//! reporting shared by every client flavor (SDL player, headless bot),
//! plus the bot's input task. The transport is pm's net module; the SDL
//! window/input/render live in sdl_client.rs.

use pm::{ClientNet, Pm, PmClient, Rng, SingleRx, coast_blend, pool_mirror, vec2};

use crate::common::*;

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
        Self {
            center: vec2(W * 0.5, H * 0.5),
            zoom: 1.0,
            target_zoom: 1.0,
        }
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
        vec2(
            (sx - W * 0.5) / self.zoom + self.center.x,
            (sy - H * 0.5) / self.zoom + self.center.y,
        )
    }
}

/// Client-side UI toggles (debug overlay etc.).
#[derive(Default)]
pub struct Ui {
    pub show_debug: bool,
}

pub struct Pools {
    pub player: pm::PoolHandle<Player>,
    pub monster: pm::PoolHandle<Monster>,
    pub bullet: pm::PoolHandle<Bullet>,
    pub status: SingleRx<Status>,
    pub dbg: SingleRx<Dbg>,
    pub roster: pm::PoolHandle<Roster>,
    pub monster_draw: pm::PoolHandle<Monster>,
    pub bullet_draw: pm::PoolHandle<Bullet>,
    pub player_draw: pm::PoolHandle<Player>,
}

pub fn client_pools(pm: &mut PmClient) -> Pools {
    Pools {
        // Replicated (registered for sync). status/dbg are the server's
        // synced singles; `sync_single` on a client hands back the typed
        // read side.
        player: pm.sync_pool("player"),
        monster: pm.sync_pool("monster"),
        bullet: pm.sync_pool("bullet"),
        status: pm.sync_single("status"),
        dbg: pm.sync_single("dbg"),
        roster: pm.sync_pool("roster"),
        // Local draw pools (smoothing targets, never networked).
        monster_draw: pm.pool("monster_draw"),
        bullet_draw: pm.pool("bullet_draw"),
        player_draw: pm.pool("player_draw"),
    }
}

/// Smoothing + diag tasks shared by player and bot clients; the
/// transport is the net module. The input layer (SDL or bot) sets the
/// input channel; rendering reads the draw pools.
pub fn add_client_tasks(pm: &mut PmClient, pools: &Pools, name: String) -> ClientNet {
    let net = pm.net();
    // No connect here — `run` does that once the schema is complete.
    // Queued before the handshake even exists — the module holds
    // reliable events until connected. Names are a fixed pod on a typed
    // channel; there are no var-len events.
    pm.event::<Name>("name").send(Name::new(&name));

    let report_name = name;

    // One-shot diag report when the game ends (HELLFIRE_REPORT_DIR).
    pm.task_add("diag", 90.0, 0.5, {
        let status = pools.status.clone();
        let name = report_name;
        let net = net.clone();
        let mut written = false;
        move |_pm| {
            let st = status.get();
            if st.flags & FLAG_GAME_OVER == 0 || written {
                return;
            }
            written = true;
            let dir =
                std::env::var("HELLFIRE_REPORT_DIR").unwrap_or_else(|_| "target/work/reports".into());
            let _ = std::fs::create_dir_all(&dir);
            let json = format!(
                "{{\n  \"role\": \"client\",\n  \"name\": \"{name}\",\n  \"peer\": {},\n  \"snapshots\": {},\n  \"rtt_ms\": {:.1},\n  \"score\": {},\n  \"win\": {}\n}}\n",
                net.peer(),
                net.snapshots(),
                net.rtt_ms(),
                st.score,
                st.flags & FLAG_WIN != 0,
            );
            let _ = std::fs::write(format!("{dir}/{name}.json"), json);
        }
    });

    // Dead-reckon the draw pools between (budget-rotated) refreshes:
    // pm::pool_mirror handles add/blend/stale-drop; the closures are
    // just the per-type blend math.
    pm.task_add("smooth", 30.0, 0.0, {
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
                let pos = if snap {
                    a.pos
                } else {
                    d.pos + (a.pos - d.pos) * 0.35
                };
                Player { pos, ..*a }
            });
        }
    });

    net
}

/// Headless bot: wanders, hunts the nearest monster it can see in its
/// own replicated state, restarts the game when it ends.
pub fn run_bot(n: u32) {
    let mut pm = Pm::client(ADDR, 60.0);
    let pools = client_pools(&mut pm);
    let _net = add_client_tasks(&mut pm, &pools, format!("bot{n}"));
    // Every client registers the full channel set — the handshake schema
    // is the connection's contract.
    let input = pm.input::<InputCmd>("input");
    let starts = pm.event::<Start>("start");
    let restarts = pm.event::<Restart>("restart");

    pm.task_add("bot", 4.0, 0.0, {
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
                me = player
                    .get()
                    .iter()
                    .find(|(_, p)| p.peer == my_peer)
                    .map(|(id, _)| id);
            }
            let pos = me
                .and_then(|id| player.get().get(id).map(|p| p.pos))
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
            for (_, m) in monster.get().iter() {
                let d = m.pos.dist(pos);
                if d < best {
                    best = d;
                    aim = m.pos;
                }
            }
            let st = status.get();
            input.set(InputCmd {
                dx: dir.x,
                dy: dir.y,
                ax: aim.x,
                ay: aim.y,
                buttons: BTN_SHOOT,
            });
            if st.flags & FLAG_GAME_OVER != 0 && rng.rf() < 0.005 {
                restarts.send(Restart::default());
            }
            // Nobody to press ENTER in a bot lobby: start after a beat.
            if st.flags & FLAG_STARTED == 0 && rng.rf() < 0.02 {
                starts.send(Start::default());
            }
        }
    });

    pm.loop_rate = 60;
    let _ = pm.run(); // a bot with no server to reach just exits
}
