//! Hellfire client side: pool registration, smoothing, and diag
//! reporting shared by every client flavor (SDL player, headless bot),
//! plus the bot's input task. The transport is pm's net module; the SDL
//! window/input/render live in sdl_client.rs.

use pm::{ClientNet, Pm, PmClient, Rng, SingleRx, Vec2, vec2};

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
        // Draw siblings, local and never networked: the same pools
        // `interp_pool` fills in add_client_tasks (name-keyed, so these
        // handles alias the ones it creates).
        monster_draw: pm.pool("monster.draw"),
        bullet_draw: pm.pool("bullet.draw"),
        player_draw: pm.pool("player.draw"),
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

    // Snapshot interpolation into the draw siblings (`"<name>.draw"`), a
    // beat behind the newest authoritative sample with a small capped
    // extrapolation to ride loss bursts. Hellfire has no prediction, so
    // the local player rides the same path as everyone else.
    let lerp2 = |a: Vec2, b: Vec2, t: f32| a + (b - a) * t;
    pm.interp_pool(
        &pools.monster,
        move |a, b, t| Monster {
            pos: lerp2(a.pos, b.pos, t),
            vel: lerp2(a.vel, b.vel, t),
            ..*b
        },
        0.05,
        0.05,
    );
    pm.interp_pool(
        &pools.bullet,
        move |a, b, t| Bullet {
            pos: lerp2(a.pos, b.pos, t),
            ..*b
        },
        0.05,
        0.05,
    );
    pm.interp_pool(
        &pools.player,
        move |a, b, t| {
            if a.pos.dist(b.pos) > 120.0 {
                *b // respawn/level jump: snap, don't slide across the map
            } else {
                Player {
                    pos: lerp2(a.pos, b.pos, t),
                    ..*b
                }
            }
        },
        0.05,
        0.05,
    );

    net
}

/// Headless bot: wanders, hunts the nearest monster it can see in its
/// own replicated state, restarts the game when it ends.
pub fn run_bot(n: u32) {
    let mut pm = Pm::client(ADDR, 60.0);
    let pools = client_pools(&mut pm);
    let net = add_client_tasks(&mut pm, &pools, format!("bot{n}"));
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
        move |pm| {
            let dt = pm.loop_dt();
            // The server marks our player via own_set; mine() is the
            // loss-robust "which entity is me" — no pod scan needed.
            let pos = net
                .mine()
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
