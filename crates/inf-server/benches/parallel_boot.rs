//! M2-S15 parallel cold-boot rehearsal — target root.
//!
//! The rehearsal boots a real N-cell node (`UringDriver` + SO_REUSEPORT
//! listeners + loop-resident recovery), so its body compiles only under
//! `cfg(all(target_os = "linux", feature = "uring"))` — the cfg on
//! `inf_runtime::UringDriver`, which this crate's dev-dependency turns on.
//!
//! That body therefore lives in a gated module rather than behind a
//! crate-level `#![cfg(target_os = "linux")]`: an inner attribute empties
//! the *whole* target on other platforms, and a `harness = false` bench
//! with no items has no `main`, which is a hard E0601 on every non-Linux
//! build (the macOS CI leg). A root that always defines `main` and gates
//! only the body keeps the target buildable everywhere and honest about
//! what it will not run.
//!
//! Docs, env knobs, and the gate context live with the body in
//! `parallel_boot/linux.rs`.

#[cfg(target_os = "linux")]
#[path = "parallel_boot/linux.rs"]
mod linux;

#[cfg(target_os = "linux")]
fn main() {
    linux::run();
}

#[cfg(not(target_os = "linux"))]
fn main() {
    println!(
        "parallel_boot: Linux-only rehearsal (io_uring cell node + SO_REUSEPORT) — \
         nothing to run on {}",
        std::env::consts::OS
    );
}
