//! Fire-and-forget sound effects on SDL3 audio streams: a fixed pool of
//! push streams ("voices") and short mono clips mixed by SDL itself
//! (every `SDL_OpenAudioDeviceStream` is its own logical device; SDL
//! mixes logical devices onto the physical one). No callback, no ring
//! buffer, no audio thread of ours: `play` pushes a clip's samples into
//! an idle voice and returns.
//!
//! House format: mono f32 at [`MIX_HZ`]. Clips convert ONCE at load
//! (`Clip::from_wav` decodes/downmixes/resamples); `play` then only
//! scales for volume — and optionally steps at a non-1.0 `rate`, the
//! cheap pitch knob that keeps a repeated shot from sounding like a
//! sample loop.
//!
//! Pairs with `pm::Births` on the client: replication converges state,
//! `Births` turns "a new entry appeared in the pool" into the edge a
//! one-shot sound wants.

use sdl3::audio::{AudioFormat, AudioSpec, AudioSpecWAV, AudioStreamOwner};

/// The mixer's sample rate. Clips are stored at this rate.
pub const MIX_HZ: u32 = 48_000;

/// How many sounds can ring at once. A 13th `play` steals the oldest
/// voice round-robin — for game sfx nobody hears the difference.
const VOICES: usize = 12;

/// A decoded sound: mono f32 samples at [`MIX_HZ`].
pub struct Clip {
    samples: Vec<f32>,
}

impl Clip {
    /// Wrap raw mono samples (at [`MIX_HZ`]) — the synth-a-placeholder
    /// path: games can ship without assets and swap WAVs in later.
    pub fn from_samples(samples: Vec<f32>) -> Clip {
        Clip { samples }
    }

    /// Load a WAV file and convert to the house format (any channel
    /// count downmixed by averaging, any rate linearly resampled).
    /// Supports u8 / i16 / i32 / f32 little-endian PCM — re-export
    /// anything more exotic as 16-bit PCM.
    pub fn from_wav(path: &str) -> Result<Clip, String> {
        let wav = AudioSpecWAV::load_wav(path).map_err(|e| e.to_string())?;
        let raw = wav.buffer();
        let to_f32: Vec<f32> = match wav.format {
            AudioFormat::U8 => raw.iter().map(|&b| (b as f32 - 128.0) / 128.0).collect(),
            AudioFormat::S16LE => raw
                .chunks_exact(2)
                .map(|c| i16::from_le_bytes([c[0], c[1]]) as f32 / 32768.0)
                .collect(),
            AudioFormat::S32LE => raw
                .chunks_exact(4)
                .map(|c| i32::from_le_bytes([c[0], c[1], c[2], c[3]]) as f32 / 2_147_483_648.0)
                .collect(),
            AudioFormat::F32LE => raw
                .chunks_exact(4)
                .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                .collect(),
            f => return Err(format!("{path}: unsupported WAV format {f:?}")),
        };
        // Downmix interleaved channels to mono by averaging each frame.
        let ch = wav.channels.max(1) as usize;
        let mono: Vec<f32> = to_f32
            .chunks_exact(ch)
            .map(|frame| frame.iter().sum::<f32>() / ch as f32)
            .collect();
        // Linear resample to MIX_HZ.
        Ok(Clip {
            samples: resample(&mono, wav.freq as f32 / MIX_HZ as f32),
        })
    }

    pub fn seconds(&self) -> f32 {
        self.samples.len() as f32 / MIX_HZ as f32
    }
}

/// Step through `src` at `rate` samples per output sample, lerping.
fn resample(src: &[f32], rate: f32) -> Vec<f32> {
    if (rate - 1.0).abs() < 1e-3 || src.is_empty() {
        return src.to_vec();
    }
    let n = (src.len() as f32 / rate) as usize;
    (0..n)
        .map(|i| {
            let t = i as f32 * rate;
            let k = t as usize;
            let frac = t - k as f32;
            let a = src[k.min(src.len() - 1)];
            let b = src[(k + 1).min(src.len() - 1)];
            a + (b - a) * frac
        })
        .collect()
}

/// The voice pool. `open()` returns `None` when there is no audio
/// device (headless box, CI) — keep the `Option` and skip `play`, the
/// game runs silent instead of crashing.
pub struct Audio {
    voices: Vec<AudioStreamOwner>,
    next: usize,
}

impl Audio {
    pub fn open() -> Option<Audio> {
        let sdl = sdl3::init().ok()?;
        let audio = sdl.audio().ok()?;
        let spec = AudioSpec {
            freq: Some(MIX_HZ as i32),
            channels: Some(1),
            format: Some(AudioFormat::F32LE),
        };
        let mut voices = Vec::with_capacity(VOICES);
        for _ in 0..VOICES {
            let stream = audio
                .default_playback_device()
                .open_device_stream(Some(&spec))
                .ok()?;
            stream.resume().ok()?;
            voices.push(stream);
        }
        Some(Audio { voices, next: 0 })
    }

    /// Fire a clip: `vol` scales amplitude (0..1 nominal), `rate` bends
    /// pitch/speed (1.0 = as recorded; try `rng.rfr(0.9, 1.1)` to
    /// de-machine-gun repeats). Prefers an idle voice; steals
    /// round-robin when all twelve are ringing.
    pub fn play(&mut self, clip: &Clip, vol: f32, rate: f32) {
        let idle = self
            .voices
            .iter()
            .position(|v| v.queued_bytes().unwrap_or(1) == 0);
        let i = idle.unwrap_or_else(|| {
            self.next = (self.next + 1) % self.voices.len();
            self.next
        });
        let samples = resample(&clip.samples, rate);
        let scaled: Vec<f32> = samples.iter().map(|s| s * vol).collect();
        if idle.is_none() {
            let _ = self.voices[i].clear(); // stolen: drop what it was saying
        }
        let _ = self.voices[i].put_data_f32(&scaled);
    }
}
