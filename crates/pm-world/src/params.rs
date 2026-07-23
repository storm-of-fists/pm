//! ENGINE-HOSTED PARAMS (2026-07-23 — Connor: "they aren't just for
//! hogs"): the whole live-tuning stack for a `pm_params!` pod, as two
//! role calls. The server hosts THE set ([`PmServer::params`]): loads
//! the text file (clamped; missing file = shipped defaults),
//! replicates the pod as the `"pm.params"` synced single, and runs the
//! clamp of record over the `"pm.param.set"` event channel — including
//! the [`PARAM_SAVE`] sentinel that rewrites the file. Clients get a
//! [`ParamsClient`] ([`PmClient::params`]): the replica (shipped
//! defaults until the first snapshot) plus typed `set`/`save`.
//!
//! One mechanism, every game: the pod comes from
//! [`pm_params!`](pm_control_core::pm_params) (declare name/default/
//! range/doc per line), the [`Tunable`] trait (re-exported here) is
//! what the engine needs of it, and the pod's `PodSchema` hash rides
//! the connect handshake like every other channel — a client with a
//! drifted param list is bounced by name at the door.

use std::io;

use pm_control_core::Tunable;

use crate::blend::PodSchema;

/// The one wire event of the params system: a client asks the server
/// to set param `idx` to `value` (the server clamps; the replicated
/// single carries the applied truth back to everyone). `idx ==`
/// [`PARAM_SAVE`] instead persists the current set to the server's
/// params file.
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Debug, Default, bytemuck::Pod, bytemuck::Zeroable)]
pub struct ParamSet {
    pub idx: u32,
    pub value: f32,
}

impl PodSchema for ParamSet {
    const SCHEMA_HASH: u64 = crate::blend::schema_hash_str("ParamSet|idx:u32|value:f32");
}

/// [`ParamSet::idx`] sentinel: "save the set to disk now".
pub const PARAM_SAVE: u32 = u32::MAX;

/// Load a params pod from `path`: shipped defaults with the file's
/// `name=value` lines applied (clamped). A missing or unreadable file
/// is just the defaults — first run needs no ceremony.
pub fn params_load<P: Tunable>(path: &str) -> P {
    let mut p = P::default();
    if let Ok(text) = std::fs::read_to_string(path) {
        let applied = p.apply_save_text(&text);
        eprintln!("[pm params] {path}: {applied} params applied");
    }
    p
}

/// Persist the whole set to `path` in the platform line shape
/// (`name=value [lo..hi]` — the range tail is a human aid, not
/// parsed). Whole-file rewrite; the file belongs to the process that
/// owns the values.
pub fn params_save<P: Tunable>(path: &str, p: &P) -> io::Result<()> {
    std::fs::write(path, p.to_save_text())
}
