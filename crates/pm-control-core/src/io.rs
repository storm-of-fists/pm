//! I/O layer — port of the ST channel blocks (`InDig`, `InRes`, `InVolt`,
//! `OutCur`, `OutDig`, `OutDiag`, `ScalingGroup`, `Joystick`, `PowerSupply`,
//! `PdmRelay`).
//!
//! Shape: each ST block was a bundle of signals + faults + a little logic,
//! plus a `*Ctrl` subclass whose `ctrl_*` methods coupled it to the ifm
//! hardware library. Here the bundle is a `pm_group!` struct of `Pm*`
//! signals (non-signal config/scratch fields are `@skip`), and the hardware
//! coupling is a per-kind channel trait (`InVoltChannel`, `OutCurChannel`, …)
//! the host implements — the same move `net::SegmentPort` made.
//! `update(&mut self, ch)` once per scan replaces the cyclic `ctrl_*` call.
//!
//! Node ownership follows ST's split via the [`Stamp`](crate::Stamp) trait:
//! `Block::new().node("io")` stamps everything with the I/O-owning node
//! (ST `node_io`), then `.cfg_node("hmi")` re-stamps the saved calibration
//! signals (ST `node_cfg`) — the cfg set *is* the `.save()` set.
//!
//! Deliberate deviations, flagged for review:
//! * Diagnostics are classified through the hardware-neutral [`Diag`] enum
//!   only — the raw vendor codes (ST `ifm_diag` signals, `eDiaginfo`) are
//!   gone. Mapping code→`Diag` is the host driver's job. `DIAG_` and `ERR_`
//!   variants of the same ifm condition collapse into one. The old
//!   `ifm_code_flt` catch-all is now `diag_flt`.
//! * The `clr_flt` clear-command signal is gone: every channel pulses its
//!   hardware reset line when one of its faults falls (true→false) — the
//!   mechanism the ST outputs already used, now uniform across In*/Out*.
//! * The `clr()` icon-color methods are dropped from core (UI passengers,
//!   same call as the NetworkManager blinkers; host-side later).
//! * In* blocks fed 500 ms local timers into 0 ms faults; that collapses
//!   into `PmFault::on_delay(500)` directly. OutDiag keeps its own timers
//!   because `other_code` reads the *unlatched* debounced outputs.
//! * ST's method-local `Timer`/`Limiter` vars only work as `VAR_INST`;
//!   ported as persistent struct fields.
//! * `elgin_helpers.Limiter`/`Scale` aren't in the export; [`Slew`] and
//!   [`ScalingGroup::scale`] implement the standard semantics (slew from 0
//!   at boot; clamped piecewise-linear over ascending raw points).
//! * ifm filter enums (`FILTER_INPUT` etc.) are driver-side config, not core.
//! * `PhysicalSignal`/`ControllerSignal` metadata (fault code, connectors,
//!   location) becomes the plain [`PhysInfo`] field; the fault table shows
//!   it via `flt.describe(&info.text())` (auto-wiring per channel comes
//!   with the device-module chunk).
//! * No `unit` metadata: units are spelled in the field names (`cur_a`,
//!   `sw_lo_pm`, `raw_cnt`), which flow to peers through the net schema.
//! * `ScalingGroup` points are `.save()` now (calibration persists and
//!   re-stamps with the cfg node) — confirm against the ST save list.

use crate::clock;
use crate::fault::PmFault;
use crate::pm_group;
use crate::signal::{PmBool, PmF32, PmI32};
use crate::timers::{PmEdge, PmTimer};
use alloc::string::String;

// ------------------------------------------------------------------- diag

/// Hardware-neutral channel diagnostic classification. The host driver maps
/// vendor codes (ifm `DIAG_INFO`) onto this; core logic never sees raw codes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Diag {
    #[default]
    Ok,
    RangeOver,
    RangeUnder,
    OverVolt,
    UnderVolt,
    OverCur,
    LowCur,
    StuckHigh,
    StuckLow,
    /// ifm `DIAG_COMPARE_MISMATCH` (redundant-path disagreement).
    Mismatch,
    /// Any other not-OK code; classified per group as the catch-all fault.
    Other,
}

/// Where this channel lives on the machine — feeds fault-table descriptions
/// (ST `PhysicalSignal`/`ControllerSignal` fb_init strings).
#[derive(Clone, Default)]
pub struct PhysInfo {
    pub fault_code: String,
    pub connectors: String,
    pub location: String,
}

impl PhysInfo {
    /// One display line for a fault-table description:
    /// `ch.diag_flt.describe(&ch.info.text())`.
    pub fn text(&self) -> String {
        let mut s = String::new();
        for part in [&self.fault_code, &self.connectors, &self.location] {
            if part.is_empty() {
                continue;
            }
            if !s.is_empty() {
                s.push_str(" · ");
            }
            s.push_str(part);
        }
        s
    }
}

// ----------------------------------------------------------- fault bundles

/// Any fault in the bundle currently raised — every channel's `ok()` is the
/// negation of this over its own faults.
fn any_val(faults: &[&PmFault]) -> bool {
    faults.iter().any(|f| f.val())
}

/// Any fault in the bundle just fell (recovered or remotely cleared) — the
/// hardware error-reset pulse every channel derives the same way.
fn any_fall(faults: &[&PmFault]) -> bool {
    faults.iter().any(|f| f.fall())
}

// ------------------------------------------------------------------- slew

/// Rate limiter (ST `elgin_helpers.Limiter`): walks `out` toward the target
/// at `rate`/s, clamped to `[lo, hi]`. Starts from 0 at boot; holds its last
/// output while not updated (outputs skip it while faulted, like ST).
#[derive(Clone, Default)]
pub struct Slew {
    out: f32,
    last_ms: u64,
    primed: bool,
}

impl Slew {
    pub fn update(&mut self, target: f32, rate_per_s: f32, lo: f32, hi: f32) -> f32 {
        let now = clock::now_ms();
        let dt_s = if self.primed {
            now.saturating_sub(self.last_ms) as f32 / 1000.0
        } else {
            0.0
        };
        self.last_ms = now;
        self.primed = true;
        let step = rate_per_s * dt_s;
        self.out += (target - self.out).clamp(-step, step);
        self.out = self.out.clamp(lo, hi);
        self.out
    }
    pub fn out(&self) -> f32 {
        self.out
    }
}

// ---------------------------------------------------------------- scaling

pm_group! {
    /// 8-point raw→engineering calibration curve, every point a saved
    /// signal so the curve is remotely configurable, recorded, and
    /// persisted. Points register as `raw0..raw7` / `eng0..eng7`.
    pub struct ScalingGroup {
        @arr pub raw: [PmF32; 8] = core::array::from_fn(|_| PmF32::new().save()),
        @arr pub eng: [PmF32; 8] = core::array::from_fn(|_| PmF32::new().save()),
    }
}

impl ScalingGroup {
    /// Piecewise-linear interpolation over the points, raw ascending;
    /// clamps below the first and above the last point.
    pub fn scale(&self, raw: f32) -> f32 {
        let p: [(f32, f32); 8] = core::array::from_fn(|i| (self.raw[i].val(), self.eng[i].val()));
        if raw <= p[0].0 {
            return p[0].1;
        }
        for w in p.windows(2) {
            let ((x0, y0), (x1, y1)) = (w[0], w[1]);
            if raw <= x1 {
                let dx = x1 - x0;
                if dx <= 0.0 {
                    return y1; // degenerate/unused segment: step
                }
                return y0 + (y1 - y0) * (raw - x0) / dx;
            }
        }
        p[7].1
    }
}

// --------------------------------------------------------- channel traits

/// Config pushed to the hardware every scan (ST called the `CONFIG.*`
/// methods cyclically). Values are in engineering units / signal-native
/// types; the driver converts to whatever its registers want.
pub struct InDigCfg {
    /// Switch thresholds, per-mille of range.
    pub sw_lo_pm: i32,
    pub sw_hi_pm: i32,
    /// Diagnostic window, per-mille.
    pub diag_lo_pm: i32,
    pub diag_hi_pm: i32,
    pub diag_detect_ms: u32,
}

pub struct InResCfg {
    pub diag_lo_ohms: i32,
    pub diag_hi_ohms: i32,
    pub diag_detect_ms: u32,
}

pub struct InVoltCfg {
    pub diag_lo_v: f32,
    pub diag_hi_v: f32,
    pub diag_detect_ms: u32,
}

pub struct OutCurCfg {
    pub drive_freq_hz: i32,
    pub dither_freq_hz: i32,
    pub dither_mag_pm: i32,
    pub start_mag_pm: i32,
    pub kp: u8,
    pub ki: u8,
    pub diag_lo_a: f32,
    pub diag_hi_a: f32,
    pub diag_detect_ms: u32,
}

pub struct OutDigCfg {
    pub diag_lo_a: f32,
    pub diag_hi_a: f32,
    pub diag_detect_ms: u32,
}

/// Digital-mode input pin (ifm `inputDigital`/`inputFrequencyA/B`/… — which
/// physical pin flavor is the host's concern, hence one trait).
pub trait InDigChannel {
    fn configure(&mut self, cfg: InDigCfg);
    fn digital(&self) -> bool;
    /// Raw analog reading behind the digital interpretation (counts).
    fn analog(&self) -> f32;
}

pub trait InResChannel {
    fn configure(&mut self, cfg: InResCfg);
    fn ohms(&self) -> f32;
    fn diag(&self) -> Diag;
    fn set_reset_error(&mut self, rst: bool);
}

pub trait InVoltChannel {
    fn configure(&mut self, cfg: InVoltCfg);
    fn volt(&self) -> f32;
    fn diag(&self) -> Diag;
    fn set_reset_error(&mut self, rst: bool);
}

/// Closed-loop current output (ifm `OUT_CURRENT_CSO` on H-bridge/PWM pins).
pub trait OutCurChannel {
    fn configure(&mut self, cfg: OutCurCfg);
    fn cur_a(&self) -> f32;
    fn diag(&self) -> Diag;
    /// Driver-local short/shutdown flag (ifm `VAR_OUT.xError`).
    fn error(&self) -> bool;
    fn set_reset_error(&mut self, rst: bool);
    fn set_request_a(&mut self, a: f32);
}

/// Digital output; `sink` selects CSI (low-side) vs CSO (high-side) wiring.
pub trait OutDigChannel {
    fn configure(&mut self, cfg: OutDigCfg, sink: bool);
    fn cur_a(&self) -> f32;
    fn diag(&self) -> Diag;
    fn error(&self) -> bool;
    fn set_reset_error(&mut self, rst: bool);
    fn set_on(&mut self, on: bool);
}

// ------------------------------------------------------------------ InDig

pm_group! {
    /// Digital input with switch thresholds and a diagnostic window.
    /// ST `ok()` is unconditionally TRUE — diagnostics are advisory here.
    pub struct InDig {
        pub sts: PmBool,
        pub raw_cnt: PmF32,
        pub sw_lo_pm: PmI32 = PmI32::new().init(300).range(0, 1000).save(),
        pub sw_hi_pm: PmI32 = PmI32::new().init(700).range(0, 1000).save(),
        pub diag_lo_pm: PmI32 = PmI32::new().init(0).range(0, 1000).save(),
        pub diag_hi_pm: PmI32 = PmI32::new().init(1000).range(0, 1000).save(),
        @skip pub diag_detect_ms: u32 = 100,
        @skip pub info: PhysInfo,
    }
}

impl InDig {
    pub fn update(&mut self, ch: &mut dyn InDigChannel) {
        ch.configure(InDigCfg {
            sw_lo_pm: self.sw_lo_pm.val(),
            sw_hi_pm: self.sw_hi_pm.val(),
            diag_lo_pm: self.diag_lo_pm.val(),
            diag_hi_pm: self.diag_hi_pm.val(),
            diag_detect_ms: self.diag_detect_ms,
        });
        self.sts.set(ch.digital());
        self.raw_cnt.set(ch.analog());
    }

    pub fn ok(&self) -> bool {
        true
    }
}

// ------------------------------------------------------------------ InRes

pm_group! {
    /// Resistive analog input (temperature senders and the like): ohms →
    /// engineering units through the scaling curve, range faults from the
    /// diagnostic classification.
    pub struct InRes {
        pub eng: PmF32,
        pub ohms: PmF32,
        pub scaling: ScalingGroup,
        pub diag_lo_ohms: PmI32 = PmI32::new().range(0, 65535).save(),
        pub diag_hi_ohms: PmI32 = PmI32::new().range(0, 65535).save(),
        pub ohms_hi_flt: PmFault = PmFault::new().on_delay(500),
        pub ohms_lo_flt: PmFault = PmFault::new().on_delay(500),
        pub diag_flt: PmFault = PmFault::new().on_delay(500),
        @skip pub diag_detect_ms: u32 = 100,
        @skip pub info: PhysInfo,
    }
}

impl InRes {
    fn flts(&self) -> [&PmFault; 3] {
        [&self.ohms_hi_flt, &self.ohms_lo_flt, &self.diag_flt]
    }

    pub fn update(&mut self, ch: &mut dyn InResChannel) {
        ch.configure(InResCfg {
            diag_lo_ohms: self.diag_lo_ohms.val(),
            diag_hi_ohms: self.diag_hi_ohms.val(),
            diag_detect_ms: self.diag_detect_ms,
        });
        self.ohms.set(ch.ohms());
        self.eng.set(self.scaling.scale(self.ohms.val()));

        // A fault falling pulses the hardware error reset — no dedicated
        // clear channel.
        ch.set_reset_error(any_fall(&self.flts()));

        let d = ch.diag();
        self.ohms_hi_flt.set(matches!(d, Diag::RangeOver | Diag::Mismatch));
        self.ohms_lo_flt.set(matches!(d, Diag::RangeUnder));
        self.diag_flt
            .set(d != Diag::Ok && !(self.ohms_hi_flt.val() || self.ohms_lo_flt.val()));
    }

    pub fn ok(&self) -> bool {
        !any_val(&self.flts())
    }
}

// ----------------------------------------------------------------- InVolt

pm_group! {
    /// 0–10 V analog input: volts → engineering units through the scaling
    /// curve, over/under-voltage faults from the diagnostic classification.
    pub struct InVolt {
        pub eng: PmF32,
        pub volt: PmF32,
        pub scaling: ScalingGroup,
        pub diag_lo_v: PmF32 = PmF32::new().range(0.0, 12.0).save(),
        pub diag_hi_v: PmF32 = PmF32::new().range(0.0, 12.0).save(),
        pub volt_hi_flt: PmFault = PmFault::new().on_delay(500),
        pub volt_lo_flt: PmFault = PmFault::new().on_delay(500),
        pub diag_flt: PmFault = PmFault::new().on_delay(500),
        @skip pub diag_detect_ms: u32 = 100,
        @skip pub info: PhysInfo,
    }
}

impl InVolt {
    fn flts(&self) -> [&PmFault; 3] {
        [&self.volt_hi_flt, &self.volt_lo_flt, &self.diag_flt]
    }

    pub fn update(&mut self, ch: &mut dyn InVoltChannel) {
        ch.configure(InVoltCfg {
            diag_lo_v: self.diag_lo_v.val(),
            diag_hi_v: self.diag_hi_v.val(),
            diag_detect_ms: self.diag_detect_ms,
        });
        self.volt.set(ch.volt());
        self.eng.set(self.scaling.scale(self.volt.val()));

        // A fault falling pulses the hardware error reset — no dedicated
        // clear channel.
        ch.set_reset_error(any_fall(&self.flts()));

        let d = ch.diag();
        self.volt_hi_flt
            .set(matches!(d, Diag::OverVolt | Diag::RangeOver | Diag::Mismatch));
        self.volt_lo_flt
            .set(matches!(d, Diag::UnderVolt | Diag::RangeUnder));
        self.diag_flt
            .set(d != Diag::Ok && !(self.volt_hi_flt.val() || self.volt_lo_flt.val()));
    }

    pub fn ok(&self) -> bool {
        !any_val(&self.flts())
    }
}

// ---------------------------------------------------------------- OutDiag

/// Per-scan snapshot of everything OutDiag classifies.
pub struct OutDiagIn {
    /// The command as last sent to hardware (0/1 digital, mA current,
    /// ‰ PWM) — classification looks at what we *asked for*.
    pub req: i32,
    /// Supply voltage of the output group.
    pub group_v: f32,
    pub cur_a: f32,
    pub diag: Diag,
    /// Driver-local short/shutdown flag.
    pub err: bool,
    /// Reset pulse (fault fall edges); its rising edge suppresses
    /// classification for one scan while the hardware clears.
    pub rst_err: bool,
    /// Configured diagnostic band (A). `lo_a == 0.0` falls back to the
    /// 25 mA data-sheet floor; `hi_a == 0.0` disables the software
    /// high-side compare.
    pub lo_a: f32,
    pub hi_a: f32,
}

/// Output fault classifier (shared by current and digital outputs):
/// overload / low-current / stuck-high / other-code, each behind a 500 ms
/// debounce. `other_code` reads the *unlatched* debounced outputs, which is
/// why these timers live here and not inside the (latching) faults.
pub struct OutDiag {
    pub is_pwm: bool,
    pub is_cur: bool,
    /// Sink (low-side, CSI) wiring: no load-side diagnostics while driving.
    pub is_csi: bool,
    rst: PmEdge,
    pub cur_hi: PmTimer,
    pub cur_lo: PmTimer,
    pub volt_off: PmTimer,
    pub other_code: PmTimer,
    pub active: bool,
    pub volt: f32,
    pub res_ohms: f32,
}

impl OutDiag {
    pub fn new() -> Self {
        OutDiag {
            is_pwm: false,
            is_cur: false,
            is_csi: false,
            rst: PmEdge::default(),
            cur_hi: PmTimer::new().on_delay(500),
            cur_lo: PmTimer::new().on_delay(500),
            volt_off: PmTimer::new().on_delay(500),
            other_code: PmTimer::new().on_delay(500),
            active: false,
            volt: 0.0,
            res_ohms: 0.0,
        }
    }

    pub fn update(&mut self, s: OutDiagIn) {
        self.rst.update(s.rst_err);
        let rst_rise = self.rst.rise.val();

        self.volt = s.group_v;
        // A PWM output only "sees" the supply for its duty fraction.
        if self.is_pwm && s.req > 0 {
            self.volt /= s.req as f32;
        }
        // Load estimate; only meaningful with real current flowing.
        self.res_ohms = if s.cur_a > 1.0 { self.volt / s.cur_a } else { 1000.0 };

        // Digital: any nonzero request. Current/PWM: past the 25 mA/‰ floor.
        self.active = s.req > if self.is_pwm || self.is_cur { 25 } else { 0 };

        // Configured low limit when supplied, else the 25 mA data-sheet floor.
        let lo_lim_a = if s.lo_a > 0.0 { s.lo_a } else { 0.025 };

        self.cur_hi.update(
            self.active
                && !rst_rise
                && (s.err
                    || (!self.is_csi
                        && (
                            // Data sheet floor is 3 ohms; ADC bottoms out ~2.
                            self.res_ohms <= 2.5
                                || (s.hi_a > 0.0 && s.cur_a >= s.hi_a)
                                || matches!(s.diag, Diag::OverCur | Diag::StuckLow)
                        ))),
        );
        self.cur_lo.update(
            self.active
                && !self.is_csi
                && !rst_rise
                && (s.cur_a <= lo_lim_a || matches!(s.diag, Diag::LowCur)),
        );
        self.volt_off
            .update(!rst_rise && matches!(s.diag, Diag::StuckHigh));
        self.other_code.update(
            s.diag != Diag::Ok
                && !(self.cur_hi.out() || self.cur_lo.out() || self.volt_off.out()),
        );
    }

    /// Copy the debounced classifications into an output block's latched
    /// faults — the same four-fault set on every output kind.
    fn latch_into(&self, [cur_hi, cur_lo, volt_off, other]: [&PmFault; 4]) {
        cur_hi.set(self.cur_hi.out());
        cur_lo.set(self.cur_lo.out());
        volt_off.set(self.volt_off.out());
        other.set(self.other_code.out());
    }
}

impl Default for OutDiag {
    fn default() -> Self {
        Self::new()
    }
}

// ----------------------------------------------------------------- OutCur

pm_group! {
    /// Closed-loop current output (proportional valve driver): request in
    /// amps, slew-rate limited with a dead-band floor, forced to zero while
    /// faulted. Faults latch; clearing one emits the hardware reset pulse.
    pub struct OutCur {
        pub req_a: PmF32,
        pub cur_a: PmF32,
        pub out_rate_a_s: PmF32 = PmF32::new().range(0.0, 1000.0).save(),
        pub min_cur_a: PmF32 = PmF32::new().range(0.0, 4.0).save(),
        pub diag_lo_a: PmF32 = PmF32::new().range(0.0, 4.0).save(),
        pub diag_hi_a: PmF32 = PmF32::new().range(0.0, 4.0).save(),
        pub drive_freq_hz: PmI32 = PmI32::new().range(0, 2000).save(),
        pub dither_freq_hz: PmI32 = PmI32::new().range(0, 1000).save(),
        pub dither_mag_pm: PmI32 = PmI32::new().range(0, 1000).save(),
        pub start_mag_pm: PmI32 = PmI32::new().range(0, 1000).save(),
        pub cur_hi_flt: PmFault = PmFault::new().latch(),
        pub cur_lo_flt: PmFault = PmFault::new().latch(),
        pub volt_off_flt: PmFault = PmFault::new().latch(),
        pub diag_flt: PmFault = PmFault::new().latch(),
        @skip pub diag: OutDiag = {
            let mut d = OutDiag::new();
            d.is_cur = true;
            d
        },
        @skip pub kp: u8 = 50,
        @skip pub ki: u8 = 50,
        @skip pub diag_detect_ms: u32 = 100,
        @skip pub info: PhysInfo,
        @skip slew: Slew,
        @skip last_cmd_ma: i32,
    }
}

impl OutCur {
    fn flts(&self) -> [&PmFault; 4] {
        [&self.cur_hi_flt, &self.cur_lo_flt, &self.volt_off_flt, &self.diag_flt]
    }

    pub fn update(&mut self, ch: &mut dyn OutCurChannel, group_v: f32) {
        ch.configure(OutCurCfg {
            drive_freq_hz: self.drive_freq_hz.val(),
            dither_freq_hz: self.dither_freq_hz.val(),
            dither_mag_pm: self.dither_mag_pm.val(),
            start_mag_pm: self.start_mag_pm.val(),
            kp: self.kp,
            ki: self.ki,
            diag_lo_a: self.diag_lo_a.val(),
            diag_hi_a: self.diag_hi_a.val(),
            diag_detect_ms: self.diag_detect_ms,
        });
        self.cur_a.set(ch.cur_a());

        // Clearing a latched fault pulses the hardware error reset.
        let rst = any_fall(&self.flts());
        ch.set_reset_error(rst);

        self.diag.update(OutDiagIn {
            req: self.last_cmd_ma,
            group_v,
            cur_a: self.cur_a.val(),
            diag: ch.diag(),
            err: ch.error(),
            rst_err: rst,
            lo_a: self.diag_lo_a.val(),
            hi_a: self.diag_hi_a.val(),
        });
        self.diag.latch_into(self.flts());

        let (lo, hi) = (self.diag_lo_a.val(), self.diag_hi_a.val());
        self.req_a.set(self.req_a.val().clamp(lo, hi));
        if self.ok() {
            let floor = if self.req_a.val() > self.min_cur_a.val() * 0.5 {
                lo.max(self.min_cur_a.val()) // ON intent: floor past the dead band
            } else {
                lo // OFF intent: true floor, ramp all the way out
            };
            let out = self
                .slew
                .update(self.req_a.val(), self.out_rate_a_s.val(), floor, hi);
            ch.set_request_a(out);
            self.last_cmd_ma = (out * 1000.0) as i32;
        } else {
            ch.set_request_a(0.0);
            self.last_cmd_ma = 0;
        }
    }

    pub fn ok(&self) -> bool {
        !any_val(&self.flts())
    }
    pub fn on(&self) -> bool {
        self.cur_a.val() > 0.025
    }
}

// ----------------------------------------------------------------- OutDig

pm_group! {
    /// Digital output with current feedback. `sink()` selects low-side
    /// wiring, which disables load-side diagnostics (nothing to measure
    /// when off).
    pub struct OutDig {
        pub req: PmBool,
        pub cur_a: PmF32,
        pub diag_lo_a: PmF32 = PmF32::new().range(0.0, 4.0).save(),
        pub diag_hi_a: PmF32 = PmF32::new().range(0.0, 4.0).save(),
        pub cur_hi_flt: PmFault = PmFault::new().latch(),
        pub cur_lo_flt: PmFault = PmFault::new().latch(),
        pub volt_off_flt: PmFault = PmFault::new().latch(),
        pub diag_flt: PmFault = PmFault::new().latch(),
        @skip pub diag: OutDiag,
        @skip pub sink: bool,
        @skip pub diag_detect_ms: u32 = 100,
        @skip pub info: PhysInfo,
        @skip last_cmd: bool,
    }
}

impl OutDig {
    pub fn sink(mut self) -> Self {
        self.sink = true;
        self
    }

    fn flts(&self) -> [&PmFault; 4] {
        [&self.cur_hi_flt, &self.cur_lo_flt, &self.volt_off_flt, &self.diag_flt]
    }

    pub fn update(&mut self, ch: &mut dyn OutDigChannel, group_v: f32) {
        ch.configure(
            OutDigCfg {
                diag_lo_a: self.diag_lo_a.val(),
                diag_hi_a: self.diag_hi_a.val(),
                diag_detect_ms: self.diag_detect_ms,
            },
            self.sink,
        );
        self.cur_a.set(ch.cur_a());

        let rst = any_fall(&self.flts());
        ch.set_reset_error(rst);

        self.diag.is_csi = self.sink;
        self.diag.update(OutDiagIn {
            req: self.last_cmd as i32,
            group_v,
            cur_a: self.cur_a.val(),
            diag: ch.diag(),
            err: ch.error(),
            rst_err: rst,
            lo_a: self.diag_lo_a.val(),
            hi_a: self.diag_hi_a.val(),
        });
        self.diag.latch_into(self.flts());

        let cmd = self.ok() && self.req.val();
        ch.set_on(cmd);
        self.last_cmd = cmd;
    }

    pub fn ok(&self) -> bool {
        !any_val(&self.flts())
    }
    pub fn on(&self) -> bool {
        self.cur_a.val() > 0.025
    }
}

// --------------------------------------------------- small device bundles

pm_group! {
    /// CAN joystick signal bundle; values arrive via the device module.
    pub struct Joystick {
        pub thumb_r: PmBool,
        pub thumb_l: PmBool,
        pub presence: PmBool,
        pub axis_x: PmI32,
        pub axis_y: PmI32,
        pub online: PmBool,
    }
}

impl Joystick {
    /// Any deliberate operator input (axis past the ±100 dead band, a thumb
    /// button, or presence).
    pub fn moved(&self) -> bool {
        self.axis_y.val().abs() > 100
            || self.axis_x.val().abs() > 100
            || self.thumb_r.val()
            || self.thumb_l.val()
            || self.presence.val()
    }
}

pm_group! {
    /// Switched supply rail feeding a group of outputs. The host feeds
    /// `diag_flt` with its channel's `diag != Diag::Ok` (ST compared the raw
    /// code against DIAG_OK; the code is gone, the fault is the health
    /// signal).
    pub struct PowerSupply {
        pub req: PmBool,
        pub volt: PmF32,
        pub cur_a: PmF32,
        pub diag_flt: PmFault,
    }
}

impl PowerSupply {
    pub fn ok(&self) -> bool {
        !self.diag_flt.val()
    }
}

pm_group! {
    /// One relay/fuse position on the power distribution module; the PDM
    /// device module (chunk 3) drives the faults.
    pub struct PdmRelay {
        pub req: PmBool,
        pub sts: PmBool,
        pub fuse_flt: PmFault,
        pub mismatch_flt: PmFault,
    }
}

impl PdmRelay {
    pub fn ok(&self) -> bool {
        !(self.fuse_flt.val() || self.mismatch_flt.val())
    }
}

// ------------------------------------------------------------------ tests

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signal::{Registrar, Stamp};

    fn set_points(s: &ScalingGroup, pts: &[(f32, f32)]) {
        for (i, &(r, e)) in pts.iter().enumerate() {
            s.raw[i].set(r);
            s.eng[i].set(e);
        }
        // fill the tail so raw stays ascending-ish at the last real point
        for i in pts.len()..8 {
            s.raw[i].set(pts.last().unwrap().0);
            s.eng[i].set(pts.last().unwrap().1);
        }
    }

    #[test]
    fn scaling_interpolates_and_clamps() {
        let s = ScalingGroup::new();
        set_points(&s, &[(0.0, -40.0), (5.0, 60.0), (10.0, 160.0)]);
        assert_eq!(s.scale(-1.0), -40.0); // clamp below
        assert_eq!(s.scale(0.0), -40.0);
        assert_eq!(s.scale(2.5), 10.0); // lerp
        assert_eq!(s.scale(7.5), 110.0);
        assert_eq!(s.scale(99.0), 160.0); // clamp above
    }

    #[test]
    fn slew_ramps_at_rate_and_clamps() {
        crate::clock::set(0);
        let mut sl = Slew::default();
        assert_eq!(sl.update(2.0, 10.0, 0.5, 2.0), 0.5); // primed at floor
        crate::clock::set(100);
        assert_eq!(sl.update(2.0, 10.0, 0.5, 2.0), 1.5); // 10 A/s * 0.1 s
        crate::clock::set(200);
        assert_eq!(sl.update(2.0, 10.0, 0.5, 2.0), 2.0); // reached + clamped
    }

    struct MockCur {
        cur_a: f32,
        diag: Diag,
        err: bool,
        cmd_a: f32,
        rst: bool,
    }

    impl MockCur {
        fn ok() -> Self {
            MockCur { cur_a: 0.0, diag: Diag::Ok, err: false, cmd_a: 0.0, rst: false }
        }
    }

    impl OutCurChannel for MockCur {
        fn configure(&mut self, _cfg: OutCurCfg) {}
        fn cur_a(&self) -> f32 {
            self.cur_a
        }
        fn diag(&self) -> Diag {
            self.diag
        }
        fn error(&self) -> bool {
            self.err
        }
        fn set_reset_error(&mut self, rst: bool) {
            self.rst = rst;
        }
        fn set_request_a(&mut self, a: f32) {
            self.cmd_a = a;
        }
    }

    fn out_cur() -> OutCur {
        let oc = OutCur::new().node("io");
        oc.out_rate_a_s.set(10.0);
        oc.min_cur_a.set(0.5);
        oc.diag_lo_a.set(0.0);
        oc.diag_hi_a.set(2.0);
        oc
    }

    #[test]
    fn out_cur_ramps_with_dead_band_floor() {
        crate::clock::set(0);
        let mut oc = out_cur();
        let mut ch = MockCur::ok();
        oc.req_a.set(1.5);
        oc.update(&mut ch, 12.0); // ON intent: jump to the 0.5 A floor
        assert_eq!(ch.cmd_a, 0.5);
        crate::clock::set(50);
        ch.cur_a = ch.cmd_a;
        oc.update(&mut ch, 12.0); // +10 A/s * 50 ms
        assert_eq!(ch.cmd_a, 1.0);
        crate::clock::set(100);
        ch.cur_a = ch.cmd_a;
        oc.update(&mut ch, 12.0);
        assert_eq!(ch.cmd_a, 1.5); // reached the request
        assert!(oc.on() && oc.ok());
    }

    #[test]
    fn out_cur_latched_fault_gates_output_and_reset_pulses() {
        crate::clock::set(0);
        let mut oc = out_cur();
        let mut ch = MockCur::ok();
        oc.req_a.set(1.5);
        oc.update(&mut ch, 12.0);
        crate::clock::set(200);
        ch.cur_a = 1.5;
        oc.update(&mut ch, 12.0);

        // Hardware reports overload; 500 ms debounce, then latch + gate.
        ch.diag = Diag::OverCur;
        ch.cur_a = 3.0;
        crate::clock::set(250);
        oc.update(&mut ch, 12.0);
        assert!(oc.ok()); // debouncing
        crate::clock::set(750);
        oc.update(&mut ch, 12.0);
        crate::clock::set(800);
        oc.update(&mut ch, 12.0);
        assert!(!oc.ok());
        assert!(oc.cur_hi_flt.val());
        assert_eq!(ch.cmd_a, 0.0); // forced off

        // Condition gone but the fault latches on.
        ch.diag = Diag::Ok;
        ch.cur_a = 0.0;
        crate::clock::set(900);
        oc.update(&mut ch, 12.0);
        assert!(oc.cur_hi_flt.val());
        assert!(!ch.rst);

        // Clearing it emits the hardware reset pulse and re-enables output.
        oc.cur_hi_flt.clear();
        crate::clock::set(950);
        oc.update(&mut ch, 12.0);
        assert!(ch.rst); // fall edge → reset pulse
        assert!(oc.ok());
        assert!(ch.cmd_a > 0.0);
        crate::clock::set(1000);
        oc.update(&mut ch, 12.0);
        assert!(!ch.rst); // pulse, not level
    }

    struct MockDig {
        cur_a: f32,
        diag: Diag,
        err: bool,
        on: bool,
        rst: bool,
    }

    impl MockDig {
        fn ok() -> Self {
            MockDig { cur_a: 0.0, diag: Diag::Ok, err: false, on: false, rst: false }
        }
    }

    impl OutDigChannel for MockDig {
        fn configure(&mut self, _cfg: OutDigCfg, _sink: bool) {}
        fn cur_a(&self) -> f32 {
            self.cur_a
        }
        fn diag(&self) -> Diag {
            self.diag
        }
        fn error(&self) -> bool {
            self.err
        }
        fn set_reset_error(&mut self, rst: bool) {
            self.rst = rst;
        }
        fn set_on(&mut self, on: bool) {
            self.on = on;
        }
    }

    #[test]
    fn out_dig_sink_mode_suppresses_load_diagnostics() {
        // Driving an open load: current stays 0. Source mode must fault
        // (cur_lo); sink mode has nothing to measure and must stay ok.
        for (sink, should_fault) in [(false, true), (true, false)] {
            crate::clock::set(0);
            let mut od = if sink {
                OutDig::new().node("io").sink()
            } else {
                OutDig::new().node("io")
            };
            od.diag_lo_a.set(0.1);
            od.diag_hi_a.set(2.0);
            od.req.set(true);
            let mut ch = MockDig::ok();
            for t in 1..=15u64 {
                crate::clock::set(t * 50);
                od.update(&mut ch, 12.0);
            }
            assert_eq!(od.cur_lo_flt.val(), should_fault, "sink={sink}");
            assert_eq!(ch.on, !should_fault);
        }
    }

    struct MockVolt {
        volts: f32,
        diag: Diag,
        rst: bool,
    }

    impl InVoltChannel for MockVolt {
        fn configure(&mut self, _cfg: InVoltCfg) {}
        fn volt(&self) -> f32 {
            self.volts
        }
        fn diag(&self) -> Diag {
            self.diag
        }
        fn set_reset_error(&mut self, rst: bool) {
            self.rst = rst;
        }
    }

    #[test]
    fn in_volt_scales_and_classifies_with_debounce() {
        crate::clock::set(0);
        let mut iv = InVolt::new().node("io").cfg_node("hmi");
        set_points(&iv.scaling, &[(0.0, 0.0), (10.0, 100.0)]);
        let mut ch = MockVolt { volts: 2.5, diag: Diag::Ok, rst: false };
        iv.update(&mut ch);
        assert_eq!(iv.eng.val(), 25.0);
        assert!(iv.ok());

        ch.diag = Diag::OverVolt;
        crate::clock::set(100);
        iv.update(&mut ch);
        assert!(iv.ok()); // 500 ms debounce
        crate::clock::set(600);
        iv.update(&mut ch);
        assert!(iv.volt_hi_flt.val());
        assert!(!iv.diag_flt.val()); // classified, not "other"
        assert!(!iv.ok());

        ch.diag = Diag::Ok; // no latch on inputs: recovers
        crate::clock::set(700);
        iv.update(&mut ch);
        assert!(iv.ok());
        assert!(!ch.rst);

        // The fault falling is what pulses the hardware reset line.
        crate::clock::set(750);
        iv.update(&mut ch);
        assert!(ch.rst);
        crate::clock::set(800);
        iv.update(&mut ch);
        assert!(!ch.rst); // pulse, not level
    }

    #[test]
    fn groups_register_with_nested_names_and_nodes() {
        let iv = InVolt::new().node("drive").cfg_node("hmi");
        let reg = Registrar::collect(&iv);
        let names: Vec<String> = reg.signals.iter().map(|s| s.meta().name()).collect();
        assert!(names.contains(&"volt".to_string()));
        assert!(names.contains(&"scaling.raw0".to_string()));
        assert!(names.contains(&"scaling.eng7".to_string()));
        assert!(names.contains(&"volt_hi_flt".to_string()));
        assert_eq!(reg.faults.len(), 3);
        // node split: live I/O on "drive", calibration on "hmi"
        assert_eq!(iv.volt.meta().node(), "drive");
        assert_eq!(iv.diag_hi_v.meta().node(), "hmi");
        assert_eq!(iv.scaling.raw[3].meta().node(), "hmi");

        let oc = OutCur::new().node("drive");
        let reg = Registrar::collect(&oc);
        assert_eq!(reg.faults.len(), 4);
        assert!(oc.diag.is_cur);

        let j = Joystick::new().node("stick");
        j.axis_x.set(500);
        assert!(j.moved());
        j.axis_x.set(50);
        assert!(!j.moved());
    }
}
