//! Presentation-side helpers for replicated state: mirror an
//! authoritative pool into a draw pool with a game-supplied blend, and
//! the standard coast+blend math for dead reckoning. Both games (demo,
//! hellfire) wrote this by hand before it was hoisted here.

use crate::kernel::PoolHandle;
use crate::math::Vec2;

/// Mirror `auth` into `draw`: new entities copy in, existing ones go
/// through `blend(id, previous_draw, auth) -> next_draw`, entities gone
/// from `auth` drop out. Call once per tick from a smoothing task; the
/// draw pool is what rendering should read.
pub fn pool_mirror<T: Copy + 'static>(
    auth: &PoolHandle<T>,
    draw: &PoolHandle<T>,
    mut blend: impl FnMut(crate::Id, T, &T) -> T,
) {
    let auth = auth.get();
    let mut draw = draw.get_mut();
    for (id, a) in auth.iter() {
        match draw.get_mut(id) {
            Some(mut d) => *d = blend(id, *d, a),
            None => draw.add(id, *a),
        }
    }
    draw.retain(|id, _| auth.contains(id));
}

/// Dead-reckoning step: coast the previous draw position along its
/// velocity for `dt`, then ease toward the authoritative position by
/// `blend` (0..1). Hides the bounded staleness of budget-rotated
/// snapshots without visible snapping.
pub fn coast_blend(pos: Vec2, vel: Vec2, auth_pos: Vec2, dt: f32, blend: f32) -> Vec2 {
    let coast = pos + vel * dt;
    coast + (auth_pos - coast) * blend
}
