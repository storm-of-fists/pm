//! Shared drive definitions: the replicated car pod, the command pod,
//! and THE step function — the same code advances the car on the server
//! and in client prediction replay; determinism is what makes
//! reconciliation byte-exact (the demo's lesson, now in 3D).

use bytemuck::{Pod, Zeroable};

pub const ADDR: &str = "127.0.0.1:48222";
/// Fixed simulation step on both sides (prediction replays it).
pub const FIXED_DT: f32 = 1.0 / 60.0;
/// Half-extent of the square arena (walls at +-ARENA on x and z).
pub const ARENA: f32 = 38.0;

/// Replicated car state. Ground-plane physics: heading 0 faces +z,
/// forward = (sin h, cos h) on (x, z). y stays 0 — the presentation is
/// 3D, the simulation deliberately isn't (yet).
#[derive(Clone, Copy, PartialEq, Debug, Default, Pod, Zeroable)]
#[repr(C)]
pub struct Car {
    pub x: f32,
    pub z: f32,
    pub heading: f32,
    pub speed: f32,
}

/// Command-frame input payload.
#[derive(Clone, Copy, PartialEq, Debug, Default, Pod, Zeroable)]
#[repr(C)]
pub struct Drive {
    pub thrust: f32, // -1..1
    pub turn: f32,   // -1..1 (positive = left)
}

/// Server event: your car's id (sent once at join).
pub const EV_VEHICLE: u16 = 16;

/// THE step. Speed-scaled steering so the car doesn't spin in place,
/// quadratic-ish drag, hard arena walls that scrub speed.
pub fn drive_step(c: &mut Car, cmd: Drive, dt: f32) {
    c.speed = (c.speed + cmd.thrust * 14.0 * dt) * (1.0 - 1.2 * dt);
    c.speed = c.speed.clamp(-7.0, 18.0);
    let steer = (c.speed.abs() / 6.0).min(1.0);
    c.heading += cmd.turn * 2.2 * steer * dt * c.speed.signum();
    c.x += c.heading.sin() * c.speed * dt;
    c.z += c.heading.cos() * c.speed * dt;
    if c.x.abs() > ARENA {
        c.x = c.x.clamp(-ARENA, ARENA);
        c.speed *= 0.4;
    }
    if c.z.abs() > ARENA {
        c.z = c.z.clamp(-ARENA, ARENA);
        c.speed *= 0.4;
    }
}

/// Per-peer body tints.
pub const PCOL: [(f32, f32, f32); 8] = [
    (0.98, 0.82, 0.16), // you (peer colors start at 1; index peer-1)
    (0.36, 0.55, 0.86),
    (0.85, 0.35, 0.42),
    (0.42, 0.78, 0.47),
    (0.78, 0.45, 0.85),
    (0.95, 0.55, 0.25),
    (0.35, 0.78, 0.78),
    (0.85, 0.75, 0.55),
];

pub fn peer_color(peer: u8) -> (f32, f32, f32) {
    PCOL[(peer as usize).saturating_sub(1) % PCOL.len()]
}

/// Spawn slot for a peer: spread along the back wall, facing +z.
pub fn spawn_car(peer: u8) -> Car {
    Car { x: (peer as f32 - 4.5) * 5.0, z: -ARENA + 6.0, heading: 0.0, speed: 0.0 }
}
