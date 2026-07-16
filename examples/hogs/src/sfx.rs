//! Hogs' sound effects: `pm::Births` turns replicated state into the
//! edges one-shot sounds want — a bullet entry appearing IS the gunshot,
//! an impact entry appearing IS the hit/kill/bite/boom. No sound events
//! on the wire, no cleanup: the same TTL'd facts the renderer draws.
//!
//! Clips: drop WAVs into `examples/hogs/assets/` (shot.wav, hit.wav,
//! kill.wav, bite.wav, boom.wav) and they're picked up at launch;
//! anything missing falls back to a synthesized placeholder so the game
//! makes noise with zero assets. No audio device (headless) = silent.
//!
//! Your OWN shot's bang rides the client-local cosmetic tracer pool
//! (born the frame the trigger reads down — see player_client's
//! cosmetic gun), NOT the replicated bullet: the replicated twin
//! arrives ~RTT/2 + interp late and is skipped here by `Bullet::owner`.
//! Remote players' shots still bang off replication — for them that
//! delay is just distance.

use pm::{Births, Rng, task};
use pm_sdl::audio::{Audio, Clip, MIX_HZ};

use crate::bot_client::ClientWorld;
use crate::common::*;

/// Sample a closure over `len` seconds at the mixer rate.
fn synth(len: f32, mut f: impl FnMut(f32) -> f32) -> Clip {
    let n = (len * MIX_HZ as f32) as usize;
    Clip::from_samples((0..n).map(|i| f(i as f32 / MIX_HZ as f32)).collect())
}

/// A WAV from the assets dir if present, else the synthesized stand-in.
fn clip_or(name: &str, fallback: Clip) -> Clip {
    for dir in ["examples/hogs/assets", "assets"] {
        let path = format!("{dir}/{name}.wav");
        if let Ok(c) = Clip::from_wav(&path) {
            eprintln!("[sfx] {path} ({:.2}s)", c.seconds());
            return c;
        }
    }
    fallback
}

fn make_clips() -> [Clip; 5] {
    let tau = std::f32::consts::TAU;

    // Gunshot: a noise crack with a fast exponential tail.
    let mut r = Rng::new(11);
    let shot = clip_or(
        "shot",
        synth(0.10, move |t| r.rfr(-1.0, 1.0) * (-t * 55.0).exp() * 0.8),
    );
    // Hit: a short bright tick — mostly tone so it cuts through.
    let mut r = Rng::new(23);
    let hit = clip_or(
        "hit",
        synth(0.05, move |t| {
            ((t * 2200.0 * tau).sin() * 0.5 + r.rfr(-1.0, 1.0) * 0.3) * (-t * 90.0).exp()
        }),
    );
    // Kill: a falling chirp (phase-accumulated so the sweep is clean).
    let mut phase = 0.0f32;
    let kill = clip_or(
        "kill",
        synth(0.28, move |t| {
            let freq = (900.0 - 2400.0 * t).max(180.0);
            phase += freq * tau / MIX_HZ as f32;
            phase.sin() * (-t * 12.0).exp() * 0.6
        }),
    );
    // Bite: a low thud with a noise transient on top.
    let mut r = Rng::new(37);
    let bite = clip_or(
        "bite",
        synth(0.18, move |t| {
            (t * 85.0 * tau).sin() * (-t * 16.0).exp() * 0.8
                + r.rfr(-1.0, 1.0) * (-t * 70.0).exp() * 0.25
        }),
    );
    // Boom: low-passed noise rumble over a 55 Hz fundamental.
    let mut r = Rng::new(53);
    let mut lp = 0.0f32;
    let boom = clip_or(
        "boom",
        synth(0.85, move |t| {
            lp += (r.rfr(-1.0, 1.0) - lp) * 0.08;
            lp * (-t * 5.0).exp() * 2.2 + (t * 55.0 * tau).sin() * (-t * 4.0).exp() * 0.5
        }),
    );
    [shot, hit, kill, bite, boom]
}

/// Register the sfx task: births off the raw replicas (plus the local
/// tracer pool for our own instant bang), attenuate by distance to our
/// truck, jitter the rate so repeats don't machine-gun.
pub fn install(pm: &mut pm::PmClient, w: &ClientWorld, tracer: &pm::PoolHandle<Tracer>) {
    let Some(mut audio) = Audio::open() else {
        eprintln!("[sfx] no audio device — running silent");
        return;
    };
    let [shot, hit, kill, bite, boom] = make_clips();
    let mut bullet_births = Births::default();
    let mut tracer_births = Births::default();
    let mut impact_births = Births::default();

    let bullet = w.bullet.clone();
    let impact = w.impact.clone();
    let pred = w.pred.clone();
    let pred_heli = w.pred_heli.clone();
    let tracer = tracer.clone();
    let net = pm.net();
    task!(pm, "sfx", 60.0, [bullet, impact, pred, pred_heli, tracer], move |pm| {
        let mut rng = Rng::new(pm.tick() | 1);
        // Ears at our vehicle (whichever predictor is live); before
        // spawn, at the arena center. Ground-plane distance is plenty —
        // altitude attenuation isn't worth the code yet.
        let (ex, ez) = pred
            .get()
            .state()
            .map(|t| (t.body.pos.x, t.body.pos.z))
            .or_else(|| {
                pred_heli
                    .get()
                    .state()
                    .map(|h| (h.body.pos.x, h.body.pos.z))
            })
            .unwrap_or((0.0, 0.0));
        // Inverse-linear falloff with a floor: distant fights stay
        // audible as texture, close ones bark.
        let att = |x: f32, z: f32| {
            let d = ((x - ex).powi(2) + (z - ez).powi(2)).sqrt();
            (1.0 - d / 160.0).clamp(0.08, 1.0)
        };

        // Our gun: the local tracer IS the click — bang now.
        for id in tracer_births.drain(&tracer.get()) {
            if let Some(tr) = tracer.get_id(id) {
                audio.play(&shot, 0.5 * att(tr.x, tr.z), rng.rfr(0.92, 1.12));
            }
        }
        // Everyone else's guns, off replication; our replicated twins
        // already banged above, skip them.
        let me = net.peer() as f32;
        for id in bullet_births.drain(&bullet.get()) {
            if let Some(b) = bullet.get_id(id)
                && b.owner != me
            {
                audio.play(&shot, 0.5 * att(b.x, b.z), rng.rfr(0.92, 1.12));
            }
        }
        for id in impact_births.drain(&impact.get()) {
            let Some(c) = impact.get_id(id) else { continue };
            let (clip, vol) = if c.kind == IMPACT_BOOM {
                (&boom, 1.0)
            } else if c.kind == IMPACT_KILL {
                (&kill, 0.7)
            } else if c.kind == IMPACT_BITE {
                (&bite, 0.8)
            } else {
                (&hit, 0.45)
            };
            audio.play(clip, vol * att(c.x, c.z), rng.rfr(0.95, 1.06));
        }
    });
}
