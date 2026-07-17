//! Shared hogs definitions: the replicated pods, THE truck step (same
//! code on server and in client prediction replay — drive's lesson), and
//! the pure geometry both sides use. Hogs are server-owned NPCs: clients
//! never step them, only interpolate — so `hog` state has no client-side
//! step function at all, just a lerp.

use pm::{Body, Id, Quat, vec3};

pub const ADDR: &str = "127.0.0.1:48223";
/// Fixed simulation step on both sides (prediction replays it).
pub const FIXED_DT: f32 = 1.0 / 60.0;
/// Half-extent of the square arena (walls at +-ARENA on x and z).
/// Big: the horde needs room to flank and the trucks need room to run,
/// with buildings breaking up the sightlines.
pub const ARENA: f32 = 100.0;

/// Remote interpolation delay (seconds) — same shared-constant contract
/// as drive: the client hands it to `interp_pool` (trucks AND hogs), the
/// server subtracts it (in ticks) from a peer's acked tick to judge that
/// peer's shots against the world they were aiming at. `PM_INTERP_MS`
/// overrides for feel A/B's.
pub const INTERP_DELAY: f32 = 0.05;

/// [`INTERP_DELAY`] with the `PM_INTERP_MS` override applied.
pub fn interp_delay() -> f32 {
    std::env::var("PM_INTERP_MS")
        .ok()
        .and_then(|v| v.parse::<f32>().ok())
        .map_or(INTERP_DELAY, |ms| ms / 1000.0)
}

/// The interp delay in whole sim ticks — what the server subtracts from
/// a peer's acked tick to find the tick that peer was *seeing*.
pub fn interp_ticks() -> u32 {
    (interp_delay() / FIXED_DT).round() as u32
}

/// First-wave horde size (`PM_HOGS` overrides — the stress knob).
pub fn wave_base() -> u32 {
    std::env::var("PM_HOGS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(40)
}
/// Extra hogs per wave past the first.
pub const WAVE_GROW: u32 = 15;

/// Parsed CLI flags every client run cares about (see main.rs header
/// for the grammar). One struct so signatures stop growing a parameter
/// per knob.
#[derive(Clone)]
pub struct Flags {
    /// (one-way lag ms, loss fraction) — the simulated link.
    pub link: (f32, f32),
    /// Day-night cycle length, seconds.
    pub day: f32,
    /// Interp delay in force, ms (report-only; frozen at creation).
    pub interp_ms: f32,
    /// Telemetry monitor address (`mon=IP:PORT`).
    pub mon: String,
}

/// Live-tunable client knobs, bridged from the telemetry node's signals
/// into a pm single (`"hogs.tune"`) that game tasks read each frame.
#[derive(Clone, Copy)]
pub struct Tune {
    pub day_secs: f32,
}

impl Default for Tune {
    fn default() -> Self {
        Tune { day_secs: 480.0 }
    }
}

// --- replicated pods -----------------------------------------------------

/// Replicated truck state — the PREDICTED substate only, same discipline
/// as drive's Car: every field is something `truck_step` evolves. The
/// kinematic chunk is the shared [`pm::Body`] (embedded, per the
/// predicted-pod contract — pose and velocity must live in the pod the
/// step evolves): a truck is `Body` with the ground-vehicle constraints
/// (pos.y = 0, rot pure yaw, vel along forward) applied by its step.
#[pm::pod]
pub struct Truck {
    pub body: Body,
    /// Filtered steering (bots lag; humans are crisp) — replicated so a
    /// truck's near future is determined, like drive.
    pub steer: f32,
    /// Turret angle relative to heading (the mouse-aim seam). Evolved
    /// by `truck_step` from the command frame like everything else, so
    /// it predicts and replicates for free — remote players see your
    /// turret swing.
    pub aim: f32,
    /// Boost heat, 0..1 — rises while boosting, cools otherwise, all in
    /// `truck_step`, so the meter predicts smoothly. Hitting 1.0 is the
    /// SERVER's cue to explode the truck (consequences aren't predicted;
    /// see `Health` for why).
    pub heat: f32,
}

impl Truck {
    /// The 2D heading gameplay reads everywhere (yaw of the body).
    pub fn heading(&self) -> f32 {
        self.body.yaw()
    }

    /// Signed forward speed — the forward component of the momentum.
    /// (`vel` may also carry a lateral sliding component; grip in
    /// `truck_step` is what bleeds it. Speedometers and gameplay
    /// checks want this, not `vel.len()`.)
    pub fn speed(&self) -> f32 {
        self.body.vel.dot(self.body.fwd())
    }
}

/// Server-owned truck vitals, deliberately NOT in the predicted pod:
/// damage comes from server events (bites), not from replaying commands,
/// so predicting it is impossible — and a non-predicted field inside a
/// Predictor's state pod freezes between corrections. Separate synced
/// pool, same id as the truck; clients read it raw.
#[pm::pod]
pub struct Health {
    pub hp: f32,
}

/// Replicated helicopter state — the other player vehicle, and the
/// engine's first full-3D predicted pod. It is EXACTLY a [`pm::Body`]:
/// attitude lives in the quaternion (pitch/roll limits are enforced by
/// the step via yaw-pitch-roll extract/clamp/rebuild — a jet would skip
/// the extraction and integrate body rates on the quat directly).
/// Deliberately NOT quantized: predicted pools stay full precision so
/// reconcile error never sits at the quantization step. Flight model:
/// one rotor-thrust vector along body-up vs gravity, fly-by-wire hover
/// trim, collective burns above it — see `heli_step`.
#[pm::pod]
pub struct Heli {
    pub body: Body,
}

/// A biomod feral hog: server-owned, never predicted — clients read it
/// through `interp_pool` only. At horde scale this pod IS the bandwidth
/// experiment, so it rides the wire quantized (the `#[wire]` field
/// attributes make `#[pm::pod]` derive `pm::Wire`; register with
/// `wire_pool`): 20 B of f32s → a 9 B repr, 13 B/entry with the id →
/// ~90 entities per 1200 B snapshot instead of ~45. Coords at 1/64 u
/// (±512 u range — the walls sit at ±ARENA), angles at 1e-4 rad (the
/// server wraps `heading` to [-pi, pi) at every write — i16 saturates
/// past ±3.27), hp at 1/200 over its 0..=HOG_HP range.
#[pm::pod]
pub struct Hog {
    #[wire(i16, scale = 64.0)]
    pub x: f32,
    #[wire(i16, scale = 64.0)]
    pub z: f32,
    #[wire(i16, scale = 10000.0)]
    pub heading: f32,
    #[wire(i16, scale = 256.0)]
    pub speed: f32,
    /// 0..HOG_HP; clients tint by it. Dead hogs are REMOVED, not hp==0.
    #[wire(u8, scale = 200.0)]
    pub hp: f32,
}

/// Server-owned co-op scoreboard, replicated as a synced single (the
/// SingleRx path drive never exercised): one shared score, the live hog
/// count, and the wave number.
#[pm::pod]
pub struct Hunt {
    pub points: f32,
    pub alive: u32,
    pub wave: u32,
}

/// A live bullet: server-owned like the hogs — the server steps it,
/// judges its hits (lag-compensated per shooter, each tick of flight),
/// and removes it on impact or at max range; clients only interpolate
/// and draw the tracer. Which peer fired it is server-local state
/// (`id.peer()` is recycling, not control), so the pod stays lean —
/// and quantized like the hogs (bullets are the other every-tick pool;
/// `heading` is wrapped at spawn and never changes in flight).
#[pm::pod]
pub struct Bullet {
    #[wire(i16, scale = 64.0)]
    pub x: f32,
    /// Muzzle height at spawn, then integrated by `pitch` — the 3D part.
    /// Truck shots fly flat at barrel height; heli shots descend along
    /// the nose. Hits require the shot's altitude inside the hog's
    /// `HOG_H` band, so the pod carries the whole trajectory.
    #[wire(i16, scale = 64.0)]
    pub y: f32,
    #[wire(i16, scale = 64.0)]
    pub z: f32,
    #[wire(i16, scale = 10000.0)]
    pub heading: f32,
    /// Climb angle: dy per unit of travel is `sin(pitch)`. 0 for trucks.
    #[wire(i16, scale = 10000.0)]
    pub pitch: f32,
    /// Which peer fired it. A client HIDES its own replicated bullets —
    /// it already drew a local [`Tracer`] at the click (the ~RTT-late
    /// twin would double-draw) — and skips their bang in sfx the same
    /// way. Whole small numbers, so the u8 roundtrip is exact.
    #[wire(u8)]
    pub owner: f32,
}

/// CLIENT-LOCAL cosmetic tracer — never synced, no wire repr: your own
/// shot, spawned at the CLICK from the predicted muzzle so the gun
/// answers your finger at 0 ms. The authoritative [`Bullet`] (hits,
/// damage, what other players see) still round-trips; `Bullet::owner`
/// is what keeps the two from both drawing. Flies and dies on the same
/// walls as the real one (`tracer_step`), minus hog tests — the kill
/// flash is the server's word and arrives when it arrives.
#[pm::pod]
pub struct Tracer {
    pub x: f32,
    pub y: f32,
    pub z: f32,
    pub heading: f32,
    pub pitch: f32,
    pub left: f32,
}

/// A transient replicated FACT (the contact-points pattern): the server
/// spawns one on a fresh id where something landed and `ttl_pool`
/// removes it. Clients render whatever entries exist, clean up nothing.
#[pm::pod]
pub struct Impact {
    #[wire(i16, scale = 64.0)]
    pub x: f32,
    #[wire(i16, scale = 64.0)]
    pub z: f32,
    /// What happened here — see the `IMPACT_*` constants. Small whole
    /// numbers, so the u8 roundtrip is exact and `==` still works.
    #[wire(u8)]
    pub kind: f32,
}

pub const IMPACT_HIT: f32 = 0.0; // a shot connected
pub const IMPACT_KILL: f32 = 1.0; // a hog died here
pub const IMPACT_BITE: f32 = 2.0; // a hog rammed a truck
pub const IMPACT_BOOM: f32 = 3.0; // a truck exploded (overheat or hp 0)
/// Marker lifetime — comfortably above one resend window so lossy
/// clients see every flash before it expires.
pub const IMPACT_TTL: f32 = 1.0;

// --- channels --------------------------------------------------------------

/// Command-frame input payload: driving plus the turret. `fire` is held
/// state, not an event — the server's gun cooldown turns it into shots.
/// `aim` is the turret angle the client wants THIS frame: the hold-to-aim
/// accumulation and the smooth snap-back on release are both client-side
/// animation; the server just gets a stream of absolute angles.
#[pm::pod]
pub struct Drive {
    pub thrust: f32, // -1..1 (truck only)
    pub turn: f32,   // -1..1: steer (truck) / yaw (heli)
    pub fire: f32,   // 0/1: trigger held
    pub aim: f32,    // turret angle relative to heading, +-AIM_MAX (truck only)
    pub boost: f32,  // 0/1: burn heat for speed (truck only)
    pub bot: f32,    // 0/1: AI controller — its steering lags
    // Heli axes, dead weight in a truck. ONE continuous channel per
    // connection is the input doctrine, so the pod is the union of every
    // vehicle's axes and each step reads its own — the seam input-map
    // will eventually own (per-vehicle key contexts live client-side).
    pub pitch: f32, // -1..1: nose down (forward) / up (heli only)
    pub lift: f32,  // -1..1: collective climb / descend (heli only)
}

/// Reliable client→server event: respawn as the chosen vehicle (the
/// server swaps your ENTITY — see the server's respawn task for why a
/// swap must be a fresh id).
#[pm::pod]
pub struct Respawn {
    pub vehicle: u32, // VEH_TRUCK | VEH_HELI
}

pub const VEH_TRUCK: u32 = 0;
pub const VEH_HELI: u32 = 1;

// --- tuning ----------------------------------------------------------------

/// Truck top speed (forward), and boosted.
pub const VMAX: f32 = 18.0;
pub const BOOST_VMAX: f32 = 30.0;
/// Tire grip: how fast LATERAL velocity bleeds (1/s exponential rate).
/// This is the whole "physics" of the truck — steering turns the
/// chassis, grip drags the momentum around after it. High = rails;
/// low = ice. Boosting loosens the rear (powerslide), which is why
/// boost-turning through a horde now feels like something.
pub const TRUCK_GRIP: f32 = 8.0;
pub const TRUCK_GRIP_BOOST: f32 = 3.2;
/// Gravity (also the heli's hover-trim baseline).
pub const G: f32 = 9.81;
/// Heat per second while boosting / cooling per second while not. Full
/// burn to explosion in ~2.5 s; a full cooldown takes ~4 s.
pub const HEAT_RATE: f32 = 0.4;
pub const HEAT_COOL: f32 = 0.25;
/// Truck hitpoints and what one bite takes.
pub const TRUCK_HP: f32 = 1.0;
pub const BITE_DMG: f32 = 0.25;
/// Points an exploded truck costs the team (on top of the bites that
/// probably caused it).
pub const DEATH_COST: f32 = 30.0;
/// Truck collision capsule: half-length along forward, radius.
pub const TRUCK_HL: f32 = 0.8;
pub const TRUCK_R: f32 = 0.9;
/// Steering control-lag time constant for bot drivers (seconds).
pub const STEER_TAU: f32 = 0.18;

/// Hog body radius (they're round; the biomod part is the attitude).
pub const HOG_R: f32 = 0.7;
/// Shots to drop a hog: HOG_HP / GUN_DMG.
pub const HOG_HP: f32 = 1.0;
/// A truck inside this range gets charged.
pub const HOG_AGGRO: f32 = 26.0;
/// Charge / roam speeds.
pub const HOG_FAST: f32 = 11.0;
pub const HOG_ROAM: f32 = 4.5;
/// Hog turn rate (rad/s) — slower than a truck can steer, so you can
/// juke a charge.
pub const HOG_TURN: f32 = 2.6;
/// After a bite the hog breaks off for this long (seconds).
pub const HOG_FLEE: f32 = 1.5;
/// Per-hog re-bite lockout (seconds) — debounces the overlap flicker.
pub const BITE_CD: f32 = 1.0;
/// Points a bite costs the team.
pub const BITE_COST: f32 = 15.0;
/// Points a kill earns the team.
pub const KILL_POINTS: f32 = 10.0;
/// While roaming, a hog walks to a random goal and picks a new one
/// inside this many seconds (or on arrival) — real wandering, not the
/// old stand-and-wiggle.
pub const ROAM_REPICK: f32 = 9.0;

/// Turret gun: refire period, damage per shot, and the projectile —
/// bullets are real replicated entities now, so range is max travel.
pub const GUN_CD: f32 = 0.25;
pub const GUN_DMG: f32 = 0.5;
pub const GUN_RANGE: f32 = 45.0;
pub const BULLET_SPEED: f32 = 70.0;
/// Turret swing limit either side of straight ahead.
pub const AIM_MAX: f32 = 2.6;
/// Hog GAMEPLAY hit ceiling: a shot connects if its altitude is inside
/// [0, HOG_H] at the hit point. Taller than the drawn hog on purpose —
/// truck barrels sit at ~1.45 and flat shots must keep connecting (2D
/// behavior preserved); it's a hitbox, not a silhouette.
pub const HOG_H: f32 = 1.8;
/// Extra hit-circle padding on top of `HOG_R`, per shooter platform —
/// forgiveness tuning, not simulation. The heli gets more: it fires on
/// the move from altitude, so near-misses that FEEL on target should
/// connect. Server-only (lives in the Shot pool), so retuning it never
/// touches the wire.
pub const HIT_PAD_TRUCK: f32 = 0.35;
pub const HIT_PAD_HELI: f32 = 0.8;

// --- helicopter tuning -------------------------------------------------------

/// Tail-rotor yaw rate (rad/s) and how hard the cyclic chases the stick
/// (1/s) — attitude is still first-order servo'd; the FORCES are honest.
pub const HELI_YAW: f32 = 1.9;
pub const HELI_ATT_K: f32 = 5.0;
/// Attitude limits: pitch tilts up to ~40°, banks up to ~29°. Tilt is
/// the throttle now (it vectors the rotor), so the nose gets more range
/// than the old cosmetic lean.
pub const HELI_PITCH_MAX: f32 = 0.70;
pub const HELI_ROLL_MAX: f32 = 0.50;
/// Main-rotor thrust: collective stick authority (u/s² added on top of
/// the fly-by-wire hover trim) and the rotor's absolute ceiling (~3.5 g
/// — biomod-hunting spec). Descent is gravity's job; thrust never
/// points down.
pub const HELI_LIFT: f32 = 16.0;
pub const HELI_T_MAX: f32 = 34.0;
/// Airframe drag, split by axis: the rotor disc brakes horizontal
/// motion gently (full nose-down cruises ≈ 30 u/s — still the fastest
/// thing in the arena), induced drag damps vertical (this is what makes
/// centered-stick hover settle instead of bobbing).
pub const HELI_HDRAG: f32 = 0.28;
pub const HELI_VDRAG: f32 = 1.6;
/// Hard horizontal airspeed cap (advancing-blade limit, flavor-wise):
/// full collective + full tilt would otherwise run away.
pub const HELI_VCAP: f32 = 34.0;
/// Altitude band: skid height when landed, hard ceiling.
pub const HELI_GROUND: f32 = 0.6;
pub const HELI_CEIL: f32 = 45.0;
/// Hull circle for buildings/bites, and how high a biomod hog can nip —
/// hover low over the horde at your peril.
pub const HELI_R: f32 = 1.4;
pub const HOG_LEAP: f32 = 2.4;

// --- buildings ---------------------------------------------------------------

/// Static obstacles as `(center x, center z, half w, half d, height)`.
/// Shared const data compiled into BOTH binaries — server and clients
/// collide against the same walls, so nothing about them replicates
/// (height is render-only). The south strip (z < -85) stays clear: that's
/// where trucks spawn.
pub const BUILDINGS: [(f32, f32, f32, f32, f32); 14] = [
    (10.0, 8.0, 4.0, 4.0, 11.0), // the downtown tower
    (0.0, -22.0, 11.0, 4.0, 6.0),
    (-40.0, -30.0, 8.0, 6.0, 5.0),
    (35.0, -45.0, 6.0, 9.0, 4.0),
    (-20.0, -60.0, 5.0, 5.0, 4.0),
    (-80.0, -55.0, 6.0, 6.0, 5.0),
    (75.0, -20.0, 4.0, 8.0, 6.0),
    (-65.0, 10.0, 7.0, 7.0, 8.0),
    (60.0, 20.0, 9.0, 5.0, 5.0),
    (20.0, 45.0, 5.0, 5.0, 7.0),
    (-25.0, 55.0, 8.0, 4.0, 4.0),
    (45.0, 70.0, 7.0, 6.0, 9.0),
    (-55.0, 75.0, 5.0, 8.0, 5.0),
    (80.0, 60.0, 6.0, 6.0, 7.0),
];

/// Whether `(x, z)` is inside any building footprint grown by `pad`.
pub fn in_building(x: f32, z: f32, pad: f32) -> bool {
    BUILDINGS
        .iter()
        .any(|&(bx, bz, hw, hd, _)| (x - bx).abs() < hw + pad && (z - bz).abs() < hd + pad)
}

/// Push a circle at `(x, z)` radius `r` out of every building it
/// overlaps. Returns the corrected position and the last push normal
/// (zero if nothing touched) — callers use the normal to scrub speed
/// (trucks) or slide the heading along the wall (hogs).
pub fn building_push(x: f32, z: f32, r: f32) -> (f32, f32, f32, f32) {
    let (mut x, mut z) = (x, z);
    let (mut nx, mut nz) = (0.0, 0.0);
    for &(bx, bz, hw, hd, _) in &BUILDINGS {
        // Closest point on the box to the circle center.
        let cx = x.clamp(bx - hw, bx + hw);
        let cz = z.clamp(bz - hd, bz + hd);
        let (dx, dz) = (x - cx, z - cz);
        let d2 = dx * dx + dz * dz;
        if d2 >= r * r {
            continue;
        }
        if d2 > 1e-8 {
            // Center outside the box: push straight away from the wall.
            let d = d2.sqrt();
            nx = dx / d;
            nz = dz / d;
            x = cx + nx * r;
            z = cz + nz * r;
        } else {
            // Center INSIDE the box (tunneled): exit by the nearest face.
            let ex = hw + r - (x - bx).abs();
            let ez = hd + r - (z - bz).abs();
            if ex < ez {
                nx = (x - bx).signum();
                nz = 0.0;
                x = bx + nx * (hw + r);
            } else {
                nx = 0.0;
                nz = (z - bz).signum();
                z = bz + nz * (hd + r);
            }
        }
    }
    (x, z, nx, nz)
}

/// `building_push` for something at altitude `y`: only buildings whose
/// roof is above you shove the hull — above the roofline you overfly.
/// Same closest-point math so ground-level callers stay byte-identical.
pub fn building_push_below(x: f32, z: f32, r: f32, y: f32) -> (f32, f32, f32, f32) {
    let (mut x, mut z) = (x, z);
    let (mut nx, mut nz) = (0.0, 0.0);
    for &(bx, bz, hw, hd, bh) in &BUILDINGS {
        if y >= bh {
            continue;
        }
        let cx = x.clamp(bx - hw, bx + hw);
        let cz = z.clamp(bz - hd, bz + hd);
        let (dx, dz) = (x - cx, z - cz);
        let d2 = dx * dx + dz * dz;
        if d2 >= r * r {
            continue;
        }
        if d2 > 1e-8 {
            let d = d2.sqrt();
            nx = dx / d;
            nz = dz / d;
            x = cx + nx * r;
            z = cz + nz * r;
        } else {
            let ex = hw + r - (x - bx).abs();
            let ez = hd + r - (z - bz).abs();
            if ex < ez {
                nx = (x - bx).signum();
                nz = 0.0;
                x = bx + nx * (hw + r);
            } else {
                nx = 0.0;
                nz = (z - bz).signum();
                z = bz + nz * (hd + r);
            }
        }
    }
    (x, z, nx, nz)
}

/// Roof height at `(x, z)`: the tallest building whose footprint covers
/// the point, 0.0 in the open — the bullets' altitude gate for walls.
pub fn building_top(x: f32, z: f32) -> f32 {
    BUILDINGS
        .iter()
        .filter(|&&(bx, bz, hw, hd, _)| (x - bx).abs() < hw && (z - bz).abs() < hd)
        .map(|&(_, _, _, _, h)| h)
        .fold(0.0, f32::max)
}

// --- muzzles + cosmetic tracers ----------------------------------------------

/// Muzzle pose, `(x, y, z, heading, climb)` — ONE definition so the
/// server's real bullet and the client's cosmetic tracer (spawned at
/// the click from PREDICTED pose) leave the same barrel the same way.
/// Turret muzzle at the barrel tip: flat shot.
pub fn truck_muzzle(t: &Truck) -> (f32, f32, f32, f32, f32) {
    let dir = t.heading() + t.aim;
    let (x, z) = (t.body.pos.x, t.body.pos.z);
    (x + dir.sin() * 1.9, 1.45, z + dir.cos() * 1.9, dir, 0.0)
}

/// Heli nose gun fires where the nose points — dive to strafe the
/// horde. Body pitch>0 = nose down, so the bullet's climb is its
/// negation.
pub fn heli_muzzle(h: &Heli) -> (f32, f32, f32, f32, f32) {
    let b = h.body;
    let (yaw, pitch, _) = b.rot.to_yaw_pitch_roll();
    (
        b.pos.x + yaw.sin() * 2.3,
        (b.pos.y - 0.35).max(0.2),
        b.pos.z + yaw.cos() * 2.3,
        yaw,
        -pitch,
    )
}

/// Advance a cosmetic [`Tracer`] one `dt`; `false` = expired. Dies on
/// exactly the walls the real bullet dies on (ground, buildings below
/// the roofline, arena, ceiling, range) so the visual never outlives
/// where the shot could truthfully be — hogs excepted, on purpose.
pub fn tracer_step(tr: &mut Tracer, dt: f32) -> bool {
    let step = BULLET_SPEED * dt;
    tr.x += tr.heading.sin() * tr.pitch.cos() * step;
    tr.z += tr.heading.cos() * tr.pitch.cos() * step;
    tr.y += tr.pitch.sin() * step;
    tr.left -= step;
    tr.left > 0.0
        && tr.y > 0.0
        && !(tr.y < building_top(tr.x, tr.z) && in_building(tr.x, tr.z, 0.0))
        && tr.x.abs() <= ARENA
        && tr.z.abs() <= ARENA
        && tr.y <= HELI_CEIL
}

// --- THE truck step ----------------------------------------------------------

/// THE step — force-based ground vehicle: bot steering lags (first-order
/// filter, so the near future is a real prediction), humans steer crisp.
/// Steering turns the CHASSIS; the momentum vector follows through tire
/// grip (lateral velocity decays at `TRUCK_GRIP`), so hard corners at
/// speed carry sideways momentum, boost loosens into a powerslide, and a
/// server shove (bite scrub, knockback) is real momentum the tires then
/// grip out — friction, not scripting. Ground constraints still project
/// into the shared `Body` (pos.y = 0, rot pure yaw); `vel` is now the
/// true 2D momentum, and `Truck::speed()` reads its forward component.
pub fn truck_step(t: &mut Truck, cmd: Drive, dt: f32) {
    // COMPILE-TIME COVERAGE: an exhaustive destructure (no `..`), so
    // adding a Truck field refuses to compile until it's named here —
    // and the rule this line sends you here to obey is: every field in
    // the predicted pod must be EVOLVED BY THIS FUNCTION from the
    // command. If the server writes it outside this step (damage,
    // pickups), it does NOT belong in Truck — give it its own
    // authoritative pool (that's why hp lives in `Health`). Then cover
    // the new field in `err_metric` and `truck_lerp` below.
    let Truck {
        body: _,
        steer: _,
        aim: _,
        heat: _,
    } = *t;
    let mut heading = t.heading();
    let speed = t.speed();

    if cmd.bot > 0.5 {
        let k = 1.0 - (-dt / STEER_TAU).exp();
        t.steer += (cmd.turn - t.steer) * k;
    } else {
        t.steer = cmd.turn;
    }
    // Turret: crisp copy of the commanded angle — the client animates
    // the hold/snap-back, so replaying commands reproduces it exactly.
    t.aim = cmd.aim.clamp(-AIM_MAX, AIM_MAX);
    // Boost: extra shove and a higher ceiling, paid in heat. Heat is
    // predicted state (this is THE shared step), so the client's meter
    // is live; the EXPLOSION at 1.0 is the server's move alone.
    let boosting = cmd.boost > 0.5 && cmd.thrust > 0.0 && t.heat < 1.0;
    t.heat = if boosting {
        (t.heat + HEAT_RATE * dt).min(1.0)
    } else {
        (t.heat - HEAT_COOL * dt).max(0.0)
    };
    let (accel, vmax) = if boosting {
        (26.0, BOOST_VMAX)
    } else {
        (14.0, VMAX)
    };
    // Steering turns the chassis (front-wheel authority still scales
    // with forward speed) — the momentum vector is caught up below.
    let authority = (speed.abs() / 6.0).min(1.0);
    heading = wrap_angle(heading + t.steer * 2.2 * authority * dt * speed.signum());
    // Decompose the world-frame momentum against the NEW chassis axes:
    // engine force + rolling drag act along forward, tire grip bleeds
    // whatever is left pointing out the doors.
    let (mut vx, mut vz) = (t.body.vel.x, t.body.vel.z);
    let (fx, fz) = (heading.sin(), heading.cos());
    let (rx, rz) = (heading.cos(), -heading.sin());
    let vf = ((vx * fx + vz * fz) + cmd.thrust * accel * dt) * (1.0 - 1.2 * dt);
    let vf = vf.clamp(-7.0, vmax);
    let grip = if boosting { TRUCK_GRIP_BOOST } else { TRUCK_GRIP };
    let vl = (vx * rx + vz * rz) * (-grip * dt).exp();
    vx = fx * vf + rx * vl;
    vz = fz * vf + rz * vl;
    let (mut x, mut z) = (t.body.pos.x, t.body.pos.z);
    x += vx * dt;
    z += vz * dt;
    if x.abs() > ARENA {
        x = x.clamp(-ARENA, ARENA);
        vx *= 0.4;
        vz *= 0.4;
    }
    if z.abs() > ARENA {
        z = z.clamp(-ARENA, ARENA);
        vx *= 0.4;
        vz *= 0.4;
    }
    // Buildings: same shared step on both sides, so driving into one
    // predicts byte-exact. The truck collides as a circle — close enough
    // at driving speeds, and capsule-vs-box isn't worth the code here.
    let (px, pz, nx, nz) = building_push(x, z, TRUCK_R + 0.3);
    if nx != 0.0 || nz != 0.0 {
        x = px;
        z = pz;
        // Momentum can point INTO the wall now (it used to ride the
        // heading): kill that component, keep the slide, and grind off
        // some of the rest.
        let into = vx * nx + vz * nz;
        if into < 0.0 {
            vx -= into * nx;
            vz -= into * nz;
        }
        vx *= 1.0 - 1.6 * dt;
        vz *= 1.0 - 1.6 * dt;
    }
    // Project back into the shared body under the ground constraints.
    t.body.pos = vec3(x, 0.0, z);
    t.body.rot = Quat::from_yaw(heading);
    t.body.vel = vec3(vx, 0.0, vz);
}

// --- THE heli step -----------------------------------------------------------

/// THE heli step — same contract as `truck_step`: shared by the server
/// and client prediction, so flying is byte-exact under replay. Rotor
/// physics: the tail rotor is the yaw rate, the cyclic servos attitude
/// (extract → clamp → rebuild on the quat), and the main rotor is ONE
/// thrust vector along body-up fighting real gravity — a fly-by-wire
/// collective trims it to hover at centered stick, the lift stick burns
/// above/below trim, and tilt vectors the force. Skids catch the ground,
/// buildings shove the hull only below their roofline.
pub fn heli_step(h: &mut Heli, cmd: Drive, dt: f32) {
    // COMPILE-TIME COVERAGE — the predicted-pod contract, same as
    // truck_step: every field here is evolved from the command by THIS
    // function. Cover new fields in `heli_err` and `heli_lerp` too.
    let Heli { body: _ } = *h;
    let b = &mut h.body;

    // Attitude on the quat via the constrained-vehicle path: extract,
    // steer, rebuild. Yaw wraps at the write like every angle; pitch
    // and roll ease toward the stick (yaw input banks the roll).
    let (yaw0, pitch0, roll0) = b.rot.to_yaw_pitch_roll();
    let yaw = wrap_angle(yaw0 + cmd.turn * HELI_YAW * dt);
    let k = 1.0 - (-HELI_ATT_K * dt).exp();
    let pitch = pitch0 + (cmd.pitch.clamp(-1.0, 1.0) * HELI_PITCH_MAX - pitch0) * k;
    let roll = roll0 + (-cmd.turn.clamp(-1.0, 1.0) * HELI_ROLL_MAX - roll0) * k;
    b.rot = Quat::from_yaw_pitch_roll(yaw, pitch, roll).norm();

    // Main rotor: ONE thrust vector along body-up, against real gravity.
    // Fly-by-wire collective trims to exactly cancel gravity at centered
    // stick (trim = G / up.y — hands-off hover by construction, level or
    // tilted); the lift stick burns above/below trim. The tilt DIRECTION
    // does everything else: nose-down vectors those newtons forward,
    // banking slides you into the turn (the tail-rotor yaw above banks
    // the roll, so turns are coordinated), and because trim follows
    // attitude, a hard dive costs you climb authority — the machine has
    // momentum and a weight now, not axes.
    let up = b.up();
    let trim = G / up.y.clamp(0.6, 1.0);
    let thrust = (trim + cmd.lift.clamp(-1.0, 1.0) * HELI_LIFT).clamp(0.0, HELI_T_MAX);
    b.vel.x = (b.vel.x + up.x * thrust * dt) * (1.0 - HELI_HDRAG * dt);
    b.vel.z = (b.vel.z + up.z * thrust * dt) * (1.0 - HELI_HDRAG * dt);
    b.vel.y = (b.vel.y + (up.y * thrust - G) * dt) * (1.0 - HELI_VDRAG * dt);
    // Advancing-blade cap: full collective + full tilt can't run away.
    let h2 = b.vel.x * b.vel.x + b.vel.z * b.vel.z;
    if h2 > HELI_VCAP * HELI_VCAP {
        let s = HELI_VCAP / h2.sqrt();
        b.vel.x *= s;
        b.vel.z *= s;
    }
    b.integrate(dt);

    // Altitude band: skids on the deck (extra drag — parked, not
    // sliding), hard ceiling.
    if b.pos.y <= HELI_GROUND {
        b.pos.y = HELI_GROUND;
        b.vel.y = b.vel.y.max(0.0);
        b.vel.x *= 1.0 - 3.0 * dt;
        b.vel.z *= 1.0 - 3.0 * dt;
    } else if b.pos.y >= HELI_CEIL {
        b.pos.y = HELI_CEIL;
        b.vel.y = b.vel.y.min(0.0);
    }
    // Arena walls stop you in the air too (biomod containment field).
    if b.pos.x.abs() > ARENA {
        b.pos.x = b.pos.x.clamp(-ARENA, ARENA);
        b.vel.x *= -0.2;
    }
    if b.pos.z.abs() > ARENA {
        b.pos.z = b.pos.z.clamp(-ARENA, ARENA);
        b.vel.z *= -0.2;
    }
    // Buildings shove the hull only below their roofline — clearing the
    // downtown tower matters, so this can't reuse the trucks' flat
    // `building_push`.
    let (px, pz, nx, nz) = building_push_below(b.pos.x, b.pos.z, HELI_R, b.pos.y);
    if nx != 0.0 || nz != 0.0 {
        b.pos.x = px;
        b.pos.z = pz;
        // Kill the velocity component into the wall; keep the slide.
        let into = b.vel.x * nx + b.vel.z * nz;
        if into < 0.0 {
            b.vel.x -= into * nx;
            b.vel.z -= into * nz;
        }
    }
}

/// Shared kinematic-chunk error term: position + velocity + attitude
/// (quat dot → 0 error when aligned; ±q counts as aligned).
pub fn body_err(a: &Body, b: &Body) -> f32 {
    (a.pos.x - b.pos.x).abs()
        + (a.pos.y - b.pos.y).abs()
        + (a.pos.z - b.pos.z).abs()
        + (a.vel.x - b.vel.x).abs()
        + (a.vel.y - b.vel.y).abs()
        + (a.vel.z - b.vel.z).abs()
        + (1.0 - a.rot.dot(b.rot).abs()) * 8.0
}

/// Shared kinematic-chunk lerp: linear pos/vel, short-arc nlerp attitude.
pub fn body_lerp(a: &Body, b: &Body, t: f32) -> Body {
    let l = |x: f32, y: f32| x + (y - x) * t;
    Body {
        pos: vec3(l(a.pos.x, b.pos.x), l(a.pos.y, b.pos.y), l(a.pos.z, b.pos.z)),
        vel: vec3(l(a.vel.x, b.vel.x), l(a.vel.y, b.vel.y), l(a.vel.z, b.vel.z)),
        rot: Quat::nlerp(a.rot, b.rot, t),
    }
}

/// Heli prediction error metric — the pod IS a body.
pub fn heli_err(a: &Heli, b: &Heli) -> f32 {
    body_err(&a.body, &b.body)
}

/// Prediction error metric: the shared body term plus the scalars.
pub fn err_metric(a: &Truck, b: &Truck) -> f32 {
    body_err(&a.body, &b.body)
        + (a.steer - b.steer).abs()
        + (a.aim - b.aim).abs()
        + (a.heat - b.heat).abs()
}

// --- geometry ---------------------------------------------------------------

// Angle helpers come from the engine; re-exported so the whole example
// reaches them through `common::*` like the rest of the shared math.
pub use pm::{lerp_angle, wrap_angle};

/// Interpolate two truck samples (`pm::pool_interp`'s lerp).
pub fn truck_lerp(a: &Truck, b: &Truck, t: f32) -> Truck {
    let l = |x: f32, y: f32| x + (y - x) * t;
    Truck {
        body: body_lerp(&a.body, &b.body, t),
        steer: l(a.steer, b.steer),
        aim: lerp_angle(a.aim, b.aim, t),
        heat: l(a.heat, b.heat),
    }
}

/// Interpolate two bullet samples.
pub fn bullet_lerp(a: &Bullet, b: &Bullet, t: f32) -> Bullet {
    let l = |x: f32, y: f32| x + (y - x) * t;
    Bullet {
        x: l(a.x, b.x),
        y: l(a.y, b.y),
        z: l(a.z, b.z),
        heading: lerp_angle(a.heading, b.heading, t),
        pitch: lerp_angle(a.pitch, b.pitch, t),
        owner: b.owner, // identity, not a quantity — never blend it
    }
}

/// Interpolate two heli samples — the pod is a body, so the shared
/// body lerp (nlerp attitude) IS the heli lerp.
pub fn heli_lerp(a: &Heli, b: &Heli, t: f32) -> Heli {
    Heli {
        body: body_lerp(&a.body, &b.body, t),
    }
}

/// Interpolate two hog samples.
pub fn hog_lerp(a: &Hog, b: &Hog, t: f32) -> Hog {
    let l = |x: f32, y: f32| x + (y - x) * t;
    Hog {
        x: l(a.x, b.x),
        z: l(a.z, b.z),
        heading: lerp_angle(a.heading, b.heading, t),
        speed: l(a.speed, b.speed),
        hp: l(a.hp, b.hp),
    }
}

/// A truck's collision capsule as its two segment endpoints (back, front).
pub fn truck_seg(t: &Truck) -> ((f32, f32), (f32, f32)) {
    let h = t.heading();
    let (fx, fz) = (h.sin() * TRUCK_HL, h.cos() * TRUCK_HL);
    let (x, z) = (t.body.pos.x, t.body.pos.z);
    ((x - fx, z - fz), (x + fx, z + fz))
}

/// Distance from point `p` to segment `a`-`b`.
pub fn seg_point_dist(a: (f32, f32), b: (f32, f32), p: (f32, f32)) -> f32 {
    let (abx, abz) = (b.0 - a.0, b.1 - a.1);
    let (apx, apz) = (p.0 - a.0, p.1 - a.1);
    let len2 = abx * abx + abz * abz;
    let t = if len2 > 1e-8 {
        ((apx * abx + apz * abz) / len2).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let (cx, cz) = (a.0 + abx * t, a.1 + abz * t);
    let (dx, dz) = (p.0 - cx, p.1 - cz);
    (dx * dx + dz * dz).sqrt()
}

/// Whether a hog (circle) touches a truck (capsule).
pub fn hog_bites_truck(h: &Hog, t: &Truck) -> bool {
    let (a, b) = truck_seg(t);
    seg_point_dist(a, b, (h.x, h.z)) < HOG_R + TRUCK_R
}

/// Ray from `(x, z)` along `heading` against hog circles: the nearest
/// hog whose body the ray crosses within `range`, as `(index into hogs,
/// hit x, hit z)`. Each hog's hit circle is `HOG_R + pad` — the pad is
/// the shooter's forgiveness (`HIT_PAD_*`). The server sweeps each
/// bullet's per-tick travel with it, against a REWOUND frame (the
/// shooter's view) — which is the whole lag-comp trick.
pub fn ray_hit_hog(
    x: f32,
    z: f32,
    heading: f32,
    range: f32,
    pad: f32,
    hogs: &[(Id, Hog)],
) -> Option<(usize, f32, f32)> {
    let (dx, dz) = (heading.sin(), heading.cos());
    let r = HOG_R + pad;
    let mut best: Option<(usize, f32)> = None;
    for (k, (_, h)) in hogs.iter().enumerate() {
        let (ox, oz) = (h.x - x, h.z - z);
        let t = ox * dx + oz * dz; // along-ray distance to closest approach
        if !(0.0..=range).contains(&t) {
            continue;
        }
        let (cx, cz) = (ox - dx * t, oz - dz * t);
        if cx * cx + cz * cz > r * r {
            continue;
        }
        if best.is_none_or(|(_, bt)| t < bt) {
            best = Some((k, t));
        }
    }
    best.map(|(k, t)| (k, x + dx * t, z + dz * t))
}

/// Friendly fire: damage a stray shot does to a teammate's vehicle
/// (gentler than `GUN_DMG` — punish spraying, don't two-shot a buddy),
/// and the truck hull's bullet height band.
pub const FRIENDLY_DMG: f32 = 0.25;
pub const TRUCK_HULL_H: f32 = 1.6;

/// A vehicle's bullet-collision shape, decoupled from which pool the
/// vehicle lives in: a ground-plane capsule (equal endpoints = a
/// cylinder) plus an altitude band. Every "does a shot touch this
/// vehicle" question — the server's friendly-fire sweep, the bots'
/// hold-fire gate — goes through [`ray_hits_hull`]; a NEW VEHICLE adds
/// its `*_hull` fn here and one registry line per side (the server's
/// `hulls` list, `ClientWorld::hulls`), and the sweep code never
/// changes.
#[derive(Clone, Copy)]
pub struct Hull {
    /// Capsule segment endpoints on the ground plane.
    pub a: (f32, f32),
    pub b: (f32, f32),
    pub r: f32,
    /// Altitude band (lo, hi) a shot must be inside at the hit point.
    pub y: (f32, f32),
}

impl Hull {
    /// The hull padded by `m` on every surface — hold-fire gates use a
    /// grown hull so bots err toward not shooting a buddy.
    pub fn grow(self, m: f32) -> Hull {
        Hull {
            r: self.r + m,
            y: (self.y.0 - m, self.y.1 + m),
            ..self
        }
    }
}

pub fn truck_hull(t: &Truck) -> Hull {
    let (a, b) = truck_seg(t);
    Hull { a, b, r: TRUCK_R, y: (0.0, TRUCK_HULL_H) }
}

pub fn heli_hull(h: &Heli) -> Hull {
    let p = h.body.pos;
    Hull {
        a: (p.x, p.z),
        b: (p.x, p.z),
        r: HELI_R,
        y: (p.y - HELI_R, p.y + HELI_R),
    }
}

/// A shot's travel — `reach` along `heading` on the ground plane, `dy`
/// total altitude change over it — against one hull, in PRESENT time
/// (vehicles aren't in the history ring; they're slow enough that
/// rewind buys little). SAMPLED, not solved: the step size rides the
/// hull radius (≤ 80% of it), so nothing tunnels whether `reach` is a
/// bullet's per-tick travel or a bot's whole line of fire. Returns the
/// hit point.
pub fn ray_hits_hull(
    x: f32,
    z: f32,
    y: f32,
    heading: f32,
    reach: f32,
    dy: f32,
    hull: &Hull,
) -> Option<(f32, f32)> {
    let (sx, sz) = (heading.sin(), heading.cos());
    let n = (reach / (hull.r * 0.8).max(0.05)).ceil().max(1.0) as usize;
    for i in 0..n {
        let frac = (i as f32 + 0.5) / n as f32;
        let (px, pz) = (x + sx * reach * frac, z + sz * reach * frac);
        let py = y + dy * frac;
        if (hull.y.0..=hull.y.1).contains(&py)
            && seg_point_dist(hull.a, hull.b, (px, pz)) < hull.r
        {
            return Some((px, pz));
        }
    }
    None
}

// --- the collider pool (docs/collisions.md) ----------------------------------

/// One collidable PART, registered into the server's collider pool by
/// its owner: detection is data the sweep iterates, never functions
/// that know a vehicle kind (docs/collisions.md §2). The entry is
/// keyed by the part's OWN id — a vehicle's parts are child entities
/// (`id_add` per part, the parent→child link lives in the server's
/// `parts` pool); a single-part swarm entity may be its own part,
/// keyed by its owner id, which makes its cleanup free. Owners
/// re-pose `hull` every tick (the heli-rotor-matrix habit applied to
/// shapes); the sweep never looks up a pose.
#[derive(Clone, Copy)]
pub struct Collider {
    /// The entity this part belongs to — response is ITS business.
    pub owner: Id,
    /// Owner-private part tag (`PART_*`); the sweep carries it through
    /// to the contact untouched and never interprets it.
    pub part: u8,
    /// Category bits — what this entry IS. Sweeps bring their own mask
    /// of what they TEST (the `MASK_SHOT` pattern, doc §9).
    pub cat: u8,
    /// World-space shape, pre-posed by the owner every tick.
    pub hull: Hull,
}

/// The only part tag so far: the whole body as one hull. Tags are
/// meaningful only to the owner's own response code.
pub const PART_BODY: u8 = 0;

/// Category bits. Add sparingly — a bit is a vocabulary word.
pub const CAT_VEHICLE: u8 = 1 << 0;

/// A hit the sweep found: who was struck, where, and how far along the
/// travel — `frac` is what orders competing hits (nearest wins).
#[derive(Clone, Copy)]
pub struct SweepHit {
    pub owner: Id,
    pub part: u8,
    pub frac: f32,
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

/// THE collisions sweep: one shot's travel against every collider
/// entry matching `mask`, nearest hit along the path winning (not
/// registry order — a hog can shield a teammate). `skip` drops the
/// shooter's own vehicle (bullets are born at its muzzle); `pad`
/// grows each tested hull QUERY-side (doc §8: the pad is the shot's
/// forgiveness — a collider doesn't know who's shooting at it).
pub fn sweep_colliders(
    x: f32,
    z: f32,
    y: f32,
    heading: f32,
    reach: f32,
    dy: f32,
    pad: f32,
    mask: u8,
    skip: Option<Id>,
    colliders: &[(Id, Collider)],
) -> Option<SweepHit> {
    let mut best: Option<SweepHit> = None;
    for (_, c) in colliders {
        if c.cat & mask == 0 || skip == Some(c.owner) {
            continue;
        }
        let Some((hx, hz)) = ray_hits_hull(x, z, y, heading, reach, dy, &c.hull.grow(pad))
        else {
            continue;
        };
        let (dx, dz) = (hx - x, hz - z);
        let frac = (dx * dx + dz * dz).sqrt() / reach.max(1e-6);
        if best.is_none_or(|b| frac < b.frac) {
            best = Some(SweepHit {
                owner: c.owner,
                part: c.part,
                frac,
                x: hx,
                y: y + dy * frac,
                z: hz,
            });
        }
    }
    best
}

/// A detected touch — written by the sweep on a fresh id, drained the
/// SAME tick by the struck entity's response task (sweep at prio 31,
/// responses at 32): transient facts as pool entries, the
/// contact-points rule. The sweep applies nothing; whoever owns
/// `owner` owns every consequence (docs/collisions.md §2).
#[derive(Clone, Copy)]
pub struct Contact {
    pub owner: Id,
    pub part: u8,
    /// What touched — `KIND_*`.
    pub kind: u8,
    /// Acting peer (the shooter); 0 for NPC causes.
    pub source_peer: u8,
    /// World hit point. `y` matters: a rotor strike at altitude is not
    /// a ground splash.
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

/// A bullet connected.
pub const KIND_BULLET: u8 = 0;

// --- presentation helpers --------------------------------------------------

/// Per-peer truck tints (peer ids start at 1; index peer-1).
pub const PCOL: [(f32, f32, f32); 8] = [
    (0.98, 0.82, 0.16),
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

/// Spawn slot for a peer: spread along the south wall, facing in
/// (identity rot = +z = north = into the arena).
pub fn spawn_truck(peer: u8) -> Truck {
    Truck {
        body: Body {
            pos: vec3((peer as f32 - 4.5) * 5.0, 0.0, -ARENA + 6.0),
            ..Body::default()
        },
        ..Truck::default()
    }
}

/// Helipad row behind the truck slots, skids down, facing in.
pub fn spawn_heli(peer: u8) -> Heli {
    Heli {
        body: Body {
            pos: vec3((peer as f32 - 4.5) * 5.0, HELI_GROUND, -ARENA + 2.5),
            ..Body::default()
        },
    }
}

// --- physics sanity ----------------------------------------------------------

/// The force model's invariants, pinned so a tuning pass can't silently
/// break them: grip actually bleeds lateral momentum, the FBW trim
/// actually hovers, tilt actually goes places (and not past the cap).
#[cfg(test)]
mod hull_tests {
    use super::*;
    use std::f32::consts::FRAC_PI_2;

    // Truck at the origin facing +z: capsule (0,∓0.8) r 0.9, band 0..1.6.
    // Shots travel +x (heading = π/2).

    #[test]
    fn sweep_hits_a_crossing_truck() {
        let t = Truck::default();
        let hit = ray_hits_hull(-5.0, 0.0, 1.0, FRAC_PI_2, 10.0, 0.0, &truck_hull(&t));
        assert!(hit.is_some(), "flat shot through the hull must connect");
    }

    #[test]
    fn altitude_band_rejects_overflight() {
        let t = Truck::default();
        let hit = ray_hits_hull(-5.0, 0.0, 5.0, FRAC_PI_2, 10.0, 0.0, &truck_hull(&t));
        assert!(hit.is_none(), "a shot 5u up overflies a 1.6u hull");
        let mut h = Heli::default();
        h.body.pos = vec3(0.0, 10.0, 0.0);
        let under = ray_hits_hull(-5.0, 0.0, 1.0, FRAC_PI_2, 10.0, 0.0, &heli_hull(&h));
        assert!(under.is_none(), "a flat shot passes under a heli at 10u");
        let level = ray_hits_hull(-5.0, 0.0, 10.0, FRAC_PI_2, 10.0, 0.0, &heli_hull(&h));
        assert!(level.is_some(), "a shot at its altitude hits it");
    }

    #[test]
    fn long_reach_cannot_tunnel() {
        // A bot's whole line of fire (GUN_RANGE), heli mid-way: the
        // sampling must scale with reach or this skips right over it.
        let mut h = Heli::default();
        h.body.pos = vec3(30.0, 8.0, 0.0);
        let dy = 8.0 / 30.0 * GUN_RANGE; // climb that crosses its altitude there
        let hit = ray_hits_hull(0.0, 0.0, 0.0, FRAC_PI_2, GUN_RANGE, dy, &heli_hull(&h));
        assert!(hit.is_some(), "45u sweep must still sample densely enough");
    }

    #[test]
    fn grow_pads_the_hold_fire_gate() {
        let t = Truck::default();
        let graze = |hull: &Hull| ray_hits_hull(-5.0, 2.0, 1.0, FRAC_PI_2, 10.0, 0.0, hull);
        assert!(graze(&truck_hull(&t)).is_none(), "1.2u off the capsule misses");
        assert!(
            graze(&truck_hull(&t).grow(0.5)).is_some(),
            "the grown gate holds fire on the same pass"
        );
    }

    /// Two trucks on the same firing line, entered far-first: the sweep
    /// must order by travel, honor the category mask, and drop the
    /// shooter's own vehicle.
    #[test]
    fn sweep_orders_masks_and_skips() {
        let near = Truck::default(); // origin
        let mut far = Truck::default();
        far.body.pos = vec3(6.0, 0.0, 0.0);
        let (nid, fid) = (Id::new(0, 0, 1), Id::new(0, 0, 2));
        let part = |owner, t: &Truck| Collider {
            owner,
            part: PART_BODY,
            cat: CAT_VEHICLE,
            hull: truck_hull(t),
        };
        let cols = vec![(fid, part(fid, &far)), (nid, part(nid, &near))];
        let shot = |mask, skip| {
            sweep_colliders(-5.0, 0.0, 1.0, FRAC_PI_2, 15.0, 0.0, 0.0, mask, skip, &cols)
        };
        let hit = shot(CAT_VEHICLE, None).expect("two hulls on the line");
        assert_eq!(hit.owner, nid, "nearest along the ray wins, not list order");
        assert!(hit.frac < 0.5, "the near truck sits in the first half");
        assert!(shot(0, None).is_none(), "an empty mask tests nothing");
        let hit = shot(CAT_VEHICLE, Some(nid)).expect("far truck still there");
        assert_eq!(hit.owner, fid, "the shooter's own vehicle is invisible");
    }

    /// The pad grows the QUERY, not the collider (doc §8) — and the hit
    /// altitude rides the climb.
    #[test]
    fn sweep_pad_is_query_side() {
        let t = Truck::default();
        let id = Id::new(0, 0, 1);
        let cols = vec![(
            id,
            Collider {
                owner: id,
                part: PART_BODY,
                cat: CAT_VEHICLE,
                hull: truck_hull(&t),
            },
        )];
        let graze = |pad| {
            sweep_colliders(-5.0, 2.0, 1.0, FRAC_PI_2, 10.0, 0.0, pad, CAT_VEHICLE, None, &cols)
        };
        assert!(graze(0.0).is_none(), "1.2u off the capsule misses unpadded");
        let hit = graze(0.5).expect("this shot's forgiveness connects it");
        assert!(
            (hit.y - 1.0).abs() < 1e-4,
            "flat shot reports its own altitude, got {}",
            hit.y
        );
    }
}

#[cfg(test)]
mod physics_tests {
    use super::*;
    const DT: f32 = 1.0 / 60.0;

    #[test]
    fn truck_grips_out_sideways_momentum() {
        let mut t = spawn_truck(1);
        t.body.vel = vec3(10.0, 0.0, 0.0); // shoved out the doors (facing +z)
        for _ in 0..60 {
            truck_step(&mut t, Drive::default(), DT);
        }
        assert!(
            t.body.vel.x.abs() < 0.3,
            "1 s of tires should grip out a 10 u/s side shove, kept {}",
            t.body.vel.x
        );
    }

    #[test]
    fn truck_slides_more_when_boosting() {
        let side = |boost: f32| {
            let mut t = spawn_truck(1);
            t.heat = 0.0;
            t.body.vel = vec3(8.0, 0.0, 0.0);
            let cmd = Drive {
                thrust: 1.0,
                boost,
                ..Default::default()
            };
            for _ in 0..12 {
                truck_step(&mut t, cmd, DT);
            }
            t.body.vel.x.abs()
        };
        assert!(
            side(1.0) > side(0.0) * 1.5,
            "boost should loosen grip: {} vs {}",
            side(1.0),
            side(0.0)
        );
    }

    #[test]
    fn heli_hovers_hands_off() {
        let mut h = spawn_heli(1);
        h.body.pos.y = 20.0;
        for _ in 0..300 {
            heli_step(&mut h, Drive::default(), DT);
        }
        assert!(
            (h.body.pos.y - 20.0).abs() < 0.5 && h.body.vel.len() < 0.2,
            "centered stick must hover (FBW trim): y {} vel {}",
            h.body.pos.y,
            h.body.vel.len()
        );
    }

    #[test]
    fn heli_full_tilt_cruises_fast_but_capped() {
        let mut h = spawn_heli(1);
        h.body.pos.y = 20.0;
        let cmd = Drive {
            pitch: 1.0,
            ..Default::default()
        };
        for _ in 0..600 {
            heli_step(&mut h, cmd, DT);
        }
        let hs = (h.body.vel.x * h.body.vel.x + h.body.vel.z * h.body.vel.z).sqrt();
        assert!(
            hs > 20.0 && hs <= HELI_VCAP + 0.1,
            "full nose-down should cruise 20..{HELI_VCAP} u/s, got {hs}"
        );
        assert!(
            (h.body.pos.y - 20.0).abs() < 2.0,
            "FBW trim should hold altitude through a full-tilt dash, y {}",
            h.body.pos.y
        );
    }
}
