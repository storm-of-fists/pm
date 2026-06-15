//! Shared hellfire definitions: replicated components (the wire format —
//! see README "Networking model"), constants, and level table. Faithful
//! port of hellfire_common.hpp onto the pm Rust stack.

use bytemuck::{Pod, Zeroable};
use pm::Vec2;

pub const ADDR: &str = "127.0.0.1:47999";

pub const W: f32 = 900.0;
pub const H: f32 = 700.0;

pub const MAX_PLAYERS: usize = 8;
pub const WIN_SCORE: i32 = 8000;

pub const PLAYER_SPEED: f32 = 280.0;
pub const PLAYER_SIZE: f32 = 64.0;
pub const PLAYER_HP: f32 = 100.0;
pub const PLAYER_COOLDOWN: f32 = 0.12;
pub const PLAYER_INVULN: f32 = 0.4;

pub const PBULLET_SPEED: f32 = 750.0;
pub const PBULLET_SIZE: f32 = 4.0;
pub const PBULLET_LIFE: f32 = 1.5;

pub const MBULLET_SPEED: f32 = 220.0;
pub const MBULLET_SIZE: f32 = 5.0;
pub const MBULLET_LIFE: f32 = 4.0;

pub const CONTACT_DMG: f32 = 1.0;
pub const BULLET_DMG: f32 = 3.0;

pub const MONSTER_MIN_SZ: f32 = 8.0;
pub const MONSTER_MAX_SZ: f32 = 16.0;
pub const MONSTER_SPEED: f32 = 60.0;

/// Typed events on the reliable stream (>= pm::EVENT_USER_BASE).
pub const EV_NAME: u16 = 16; // client -> server: utf8 player name
pub const EV_RESTART: u16 = 17; // client -> server: restart after game over
pub const EV_START: u16 = 18; // client -> server: leave lobby, start game

pub const PCOL: [[u8; 3]; 8] = [
    [0, 220, 255],
    [0, 255, 120],
    [255, 160, 40],
    [255, 80, 200],
    [255, 220, 40],
    [255, 80, 80],
    [160, 80, 255],
    [220, 220, 220],
];

pub const SPAWN_X: [f32; 8] = [180.0, 360.0, 540.0, 720.0, 180.0, 360.0, 540.0, 720.0];
pub const SPAWN_Y: [f32; 8] = [245.0, 245.0, 245.0, 245.0, 455.0, 455.0, 455.0, 455.0];

pub struct LevelDef {
    pub threshold: i32,
    pub speed_mult: f32,
    pub spawn_mult: f32,
    pub max_monsters: usize,
    pub size_mult: f32,
}

pub const LEVELS: [LevelDef; 5] = [
    LevelDef {
        threshold: 0,
        speed_mult: 0.6,
        spawn_mult: 0.4,
        max_monsters: 60,
        size_mult: 0.8,
    },
    LevelDef {
        threshold: 500,
        speed_mult: 0.8,
        spawn_mult: 0.7,
        max_monsters: 120,
        size_mult: 0.9,
    },
    LevelDef {
        threshold: 1500,
        speed_mult: 1.0,
        spawn_mult: 1.2,
        max_monsters: 200,
        size_mult: 1.0,
    },
    LevelDef {
        threshold: 3000,
        speed_mult: 1.3,
        spawn_mult: 2.0,
        max_monsters: 300,
        size_mult: 1.1,
    },
    LevelDef {
        threshold: 5500,
        speed_mult: 1.6,
        spawn_mult: 3.0,
        max_monsters: 400,
        size_mult: 1.2,
    },
];

// --- replicated components (Pod = wire format) --------------------------

#[derive(Clone, Copy, PartialEq, Debug, Default, Pod, Zeroable)]
#[repr(C)]
pub struct Player {
    pub pos: Vec2,
    pub hp: f32,
    pub peer: u32,
    pub alive: u32,
    pub color: [u8; 4],
}

#[derive(Clone, Copy, PartialEq, Debug, Default, Pod, Zeroable)]
#[repr(C)]
pub struct Monster {
    pub pos: Vec2,
    pub vel: Vec2,
    pub size: f32,
    pub color: [u8; 4],
}

#[derive(Clone, Copy, PartialEq, Debug, Default, Pod, Zeroable)]
#[repr(C)]
pub struct Bullet {
    pub pos: Vec2,
    pub vel: Vec2,
    pub size: f32,
    pub player_owned: u32,
}

pub const FLAG_STARTED: u32 = 1;
pub const FLAG_GAME_OVER: u32 = 2;
pub const FLAG_WIN: u32 = 4;

/// Single-entity pool: whole-game scoreboard state. Pool state (not an
/// event) so late joiners see it — replication is the multicast.
#[derive(Clone, Copy, PartialEq, Debug, Default, Pod, Zeroable)]
#[repr(C)]
pub struct Status {
    pub time: f32,
    pub score: i32,
    pub kills: i32,
    pub level: i32,
    pub round: u32,
    pub flags: u32,
    pub level_flash: f32,
}

#[derive(Clone, Copy, PartialEq, Debug, Default, Pod, Zeroable)]
#[repr(C)]
pub struct Roster {
    pub peer: u32,
    pub name: [u8; 12],
}

impl Roster {
    pub fn new(peer: u8, name: &str) -> Self {
        let mut r = Roster {
            peer: peer as u32,
            name: [0; 12],
        };
        let bytes = name.as_bytes();
        let n = bytes.len().min(12);
        r.name[..n].copy_from_slice(&bytes[..n]);
        r
    }

    #[allow(dead_code)] // debug overlay / lobby UI will read names
    pub fn name_str(&self) -> &str {
        let end = self.name.iter().position(|&b| b == 0).unwrap_or(12);
        std::str::from_utf8(&self.name[..end]).unwrap_or("?")
    }
}

/// Replicated server diagnostics, refreshed ~1 Hz — feeds the client
/// debug overlay (the old PKT_DBG).
#[derive(Clone, Copy, PartialEq, Debug, Default, Pod, Zeroable)]
#[repr(C)]
pub struct Dbg {
    pub monsters: u32,
    pub bullets: u32,
    pub tick_ms: f32,
}

pub const BTN_SHOOT: u32 = 1;

/// Command-frame input payload: movement axes, aim point (world coords),
/// button bits.
#[derive(Clone, Copy, PartialEq, Debug, Default, Pod, Zeroable)]
#[repr(C)]
pub struct InputCmd {
    pub dx: f32,
    pub dy: f32,
    pub ax: f32,
    pub ay: f32,
    pub buttons: u32,
}

/// Server- or client-side only (NOT synced): per-monster shoot timer.
#[derive(Clone, Copy, Default)]
pub struct MonsterSrv {
    pub shoot_timer: f32,
}

/// Server-side only: per-bullet remaining lifetime.
#[derive(Clone, Copy, Default)]
pub struct BulletSrv {
    pub life: f32,
}

/// Server-side only: per-player fire cooldown and invulnerability.
#[derive(Clone, Copy, Default)]
pub struct PlayerSrv {
    pub cooldown: f32,
    pub invuln: f32,
}

pub fn spawn_index(peer: u8) -> usize {
    (peer.max(1) as usize - 1) % MAX_PLAYERS
}
