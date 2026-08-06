//! M4-S13 accounting-vs-block-layer validation — target root.
//!
//! The measurement is the database's own per-namespace write counters
//! against `/proc/diskstats` — the instrument the database does not
//! control. That file is Linux-only, so the body is a gated module and
//! this root always defines `main` (ADR-0065 D4).
//!
//! Gating the body rather than the target matters twice over: an inner
//! `#![cfg(...)]` would leave a `harness = false` bench with no `main`
//! at all off Linux (E0601), and leaving the body ungated but unused
//! makes every helper, constant, and import dead code under
//! `-D warnings`. Neither is a lint to silence — the code genuinely does
//! not belong on a platform without `/proc/diskstats`.
//!
//! Docs, env knobs, and the methodology live with the body in
//! `write_accounting/linux.rs`.

#[cfg(target_os = "linux")]
#[path = "write_accounting/linux.rs"]
mod linux;

#[cfg(target_os = "linux")]
fn main() {
    linux::run();
}

#[cfg(not(target_os = "linux"))]
fn main() {
    println!(
        "M4-S13 block-layer validation needs /proc/diskstats (Linux) — \
         nothing to run on {}",
        std::env::consts::OS
    );
}
