//! Client-side prediction with rewind-replay reconciliation, hoisted
//! from the demo once hellfire confirmed the shape. Generic over the
//! replicated state pod `S` and the command pod `C`; the game supplies
//! THE step function (the same one the server runs — determinism is the
//! whole trick) and an error metric.
//!
//! Flow per tick, from the net task:
//! - on each applied snapshot: `reconcile(auth_state, applied.input_seq, ...)`
//! - after sending input:      `predict(seq, cmd, ...)`
//! - rendering reads `state()` for the local avatar (instant input
//!   response); remote entities go through `pool_mirror` instead.

use std::collections::VecDeque;

/// Prediction ring for one locally-controlled entity.
pub struct Predictor<S, C> {
    state: Option<S>,
    /// (input seq, cmd, predicted state after applying it)
    ring: VecDeque<(u32, C, S)>,
    cap: usize,
    /// Reconciliations that found divergence and replayed. A steadily
    /// climbing count means the client step doesn't match the server's.
    pub corrections: u32,
}

impl<S, C> Default for Predictor<S, C> {
    fn default() -> Self {
        // ~4 s of unacked input at 60 Hz before the ring caps.
        Self {
            state: None,
            ring: VecDeque::new(),
            cap: 240,
            corrections: 0,
        }
    }
}

impl<S: Copy, C: Copy> Predictor<S, C> {
    pub fn with_cap(cap: usize) -> Self {
        Self {
            cap,
            ..Self::default()
        }
    }

    /// Current predicted state — what rendering should show for the
    /// local avatar. None until the first snapshot seeds it.
    pub fn state(&self) -> Option<S> {
        self.state
    }

    /// Drop everything and reseed from authority (e.g. after respawn
    /// teleports where replaying old inputs makes no sense).
    pub fn reset(&mut self, auth: S) {
        self.state = Some(auth);
        self.ring.clear();
    }

    /// A snapshot carrying `auth` arrived with the server's echo of the
    /// last input seq it applied. Compares what we predicted at that
    /// seq against authority; if `err` exceeds `tolerance`, rewinds to
    /// authority and replays the still-unacked commands through `step`.
    /// Returns true if a correction happened.
    pub fn reconcile(
        &mut self,
        auth: S,
        echo_seq: u32,
        mut step: impl FnMut(&mut S, C),
        err: impl FnOnce(&S, &S) -> f32,
        tolerance: f32,
    ) -> bool {
        if self.state.is_none() {
            self.reset(auth);
            return false;
        }
        // Drop ring entries the server has consumed, keeping the one
        // matching the echo for comparison.
        let mut predicted_then: Option<S> = None;
        while let Some(&(seq, _, state)) = self.ring.front() {
            if seq > echo_seq {
                break;
            }
            if seq == echo_seq {
                predicted_then = Some(state);
            }
            self.ring.pop_front();
        }
        match predicted_then {
            Some(was) => {
                if err(&was, &auth) <= tolerance {
                    return false;
                }
                // Rewind to authority and replay unacked inputs.
                let mut replayed = auth;
                for entry in self.ring.iter_mut() {
                    step(&mut replayed, entry.1);
                    entry.2 = replayed;
                }
                self.state = Some(replayed);
                self.corrections += 1;
                true
            }
            None => {
                if self.ring.is_empty() && echo_seq > 0 {
                    self.state = Some(auth); // long stall: adopt authority
                }
                false
            }
        }
    }

    /// Input `cmd` was just sent as `seq`: predict its result instantly
    /// and record it for future replay.
    pub fn predict(&mut self, seq: u32, cmd: C, mut step: impl FnMut(&mut S, C)) {
        let Some(mut state) = self.state else { return };
        step(&mut state, cmd);
        self.state = Some(state);
        self.ring.push_back((seq, cmd, state));
        if self.ring.len() > self.cap {
            self.ring.pop_front();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // 1-D "physics": position integrates velocity commands.
    fn step(s: &mut f32, c: f32) {
        *s += c;
    }

    fn err(a: &f32, b: &f32) -> f32 {
        (a - b).abs()
    }

    #[test]
    fn clean_echo_means_no_corrections() {
        let mut p: Predictor<f32, f32> = Predictor::default();
        p.reconcile(0.0, 0, step, err, 1e-6); // seed
        for seq in 1..=100u32 {
            p.predict(seq, 1.0, step);
        }
        // Server applied through seq 50 and (deterministically) agrees.
        assert!(!p.reconcile(50.0, 50, step, err, 1e-6));
        assert_eq!(p.corrections, 0);
        assert_eq!(p.state(), Some(100.0));
    }

    #[test]
    fn divergence_rewinds_and_replays() {
        let mut p: Predictor<f32, f32> = Predictor::default();
        p.reconcile(0.0, 0, step, err, 1e-6);
        for seq in 1..=10u32 {
            p.predict(seq, 1.0, step);
        }
        // Server says position was 3.5 at seq 5 (we predicted 5.0):
        // rewind to 3.5, replay seqs 6..=10 -> 8.5.
        assert!(p.reconcile(3.5, 5, step, err, 1e-6));
        assert_eq!(p.corrections, 1);
        assert_eq!(p.state(), Some(8.5));
    }

    #[test]
    fn stall_adopts_authority() {
        let mut p: Predictor<f32, f32> = Predictor::default();
        p.reconcile(0.0, 0, step, err, 1e-6);
        p.predict(1, 1.0, step);
        // Echo far past everything we have queued.
        p.reconcile(42.0, 99, step, err, 1e-6);
        assert_eq!(p.state(), Some(42.0));
    }
}
