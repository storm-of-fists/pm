//! Bakes the build manifest into `pm` so a mod and its host can each
//! report exactly which compiler/profile/target produced their copy of
//! `pm`. The TypeId-hash ABI gate (see `modload::mod_abi`) already
//! *refuses* a mismatched mod; this manifest exists so the refusal can
//! say *why* in human terms instead of printing two opaque hashes.
//!
//! Both host and mod run this same script against their own `pm`, so the
//! values are each side's own truth — when they differ, that difference
//! is the diagnosis.

use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    // Re-bake if the toolchain moves under us.
    println!("cargo:rerun-if-env-changed=RUSTC");

    let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".into());
    let rustc_version = Command::new(&rustc)
        .arg("-vV")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|vv| {
            // Compose "1.96.0 (ac68faa20 2026-05-25)" from the verbose
            // output's release + commit lines; the commit pins nightly /
            // patch rebuilds that share a release number.
            let release = field(&vv, "release: ").unwrap_or("unknown");
            match field(&vv, "commit-hash: ") {
                Some(c) => format!("{release} ({}", &c[..c.len().min(9)]) + ")",
                None => release.to_string(),
            }
        })
        .unwrap_or_else(|| "unknown".into());

    println!("cargo:rustc-env=PM_RUSTC_VERSION={rustc_version}");
    println!(
        "cargo:rustc-env=PM_PROFILE={}",
        std::env::var("PROFILE").unwrap_or_default()
    );
    println!(
        "cargo:rustc-env=PM_TARGET={}",
        std::env::var("TARGET").unwrap_or_default()
    );
}

/// First line starting with `key`, returning the remainder trimmed.
fn field<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    text.lines()
        .find_map(|l| l.strip_prefix(key))
        .map(str::trim)
}
