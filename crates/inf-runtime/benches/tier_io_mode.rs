//! M4-S09 tier-file I/O mode A/B (ADR-0054) — target root.
//!
//! The body drives `UringDriver` + a registered `AlignedPool` +
//! `ColdReads`, all of which exist only under
//! `cfg(all(target_os = "linux", feature = "uring"))`. `required-features`
//! keeps Cargo from building this target without `uring`, but that is not
//! enough on its own: a `--workspace` build unifies features (inf-server's
//! dev-dependency turns `uring` on), so this target *is* built on macOS,
//! where the platform half of the cfg is false.
//!
//! So the body is a gated module and this root always defines `main`
//! (ADR-0065 D4). A crate-level `#![cfg(...)]` here emptied the whole
//! target off Linux, leaving a `harness = false` bench with no `main` —
//! hard E0601.
//!
//! Docs, phases, and env knobs live with the body in `tier_io_mode/linux.rs`.

#[cfg(all(target_os = "linux", feature = "uring"))]
#[path = "tier_io_mode/linux.rs"]
mod linux;

#[cfg(all(target_os = "linux", feature = "uring"))]
fn main() {
    linux::run();
}

#[cfg(not(all(target_os = "linux", feature = "uring")))]
fn main() {
    println!("tier_io_mode: needs Linux + io_uring — nothing to run on {}", std::env::consts::OS);
}
