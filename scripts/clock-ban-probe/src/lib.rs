//! Every `PLANT` line is an ambient-clock spelling clippy's type-resolved
//! `disallowed-methods` must report. The text grep in
//! check-cell-denylist.sh matches the literal `Instant::now`, so it sees
//! the plain import (`a`) and the fn pointer (`e`) but not a rename,
//! an alias, `<Instant>::now()`, `elapsed()`, `UNIX_EPOCH`, the TSC or
//! libc — proven on the real tree, batch 14 of the 2026-08-30 review:
//! the grep said nothing about `Clock::now()`, `UNIX_EPOCH.elapsed()` and
//! `<Clock>::now()` planted in inf-foundation; clippy reported all three.
//! `CONTROL` lines
//! are pure `Duration` arithmetic and must stay silent; `ALLOWED` is the
//! sanctioned per-site shape. The gate parses these markers.
#![allow(dead_code, unused_imports, clippy::unnecessary_wraps)]
use std::sync::Condvar;
use std::time::SystemTime as Wall;
use std::time::{Duration, Instant};
type Alias = std::time::Instant;

pub fn a_use_import() -> Instant {
    Instant::now() // PLANT std::time::Instant::now
}
pub fn b_renamed() -> Wall {
    Wall::now() // PLANT std::time::SystemTime::now
}
pub fn c_alias() -> Alias {
    Alias::now() // PLANT std::time::Instant::now
}
pub fn d_qualified() -> Instant {
    <Instant>::now() // PLANT std::time::Instant::now
}
pub fn e_fn_pointer() -> Instant {
    let f = Instant::now; // PLANT std::time::Instant::now
    f()
}
pub fn f_epoch_elapsed() -> Duration {
    std::time::UNIX_EPOCH.elapsed().unwrap_or_default() // PLANT std::time::SystemTime::elapsed
}
pub fn g_handed_in(t: Instant) -> Duration {
    t.elapsed() // PLANT std::time::Instant::elapsed
}
pub fn h_glob() -> std::time::SystemTime {
    use std::time::*;
    SystemTime::now() // PLANT std::time::SystemTime::now
}
#[cfg(target_arch = "x86_64")]
pub fn i_tsc() -> u64 {
    unsafe { core::arch::x86_64::_rdtsc() } // PLANT core::arch::x86_64::_rdtsc
}
pub fn j_sleep() {
    std::thread::sleep(Duration::from_millis(1)) // PLANT std::thread::sleep
}
pub struct K {
    parked: Condvar, // PLANT-TYPE std::sync::Condvar
}
pub fn l_pure(a: Instant, b: Instant) -> Duration {
    a.duration_since(b) // CONTROL
}
pub fn m_pure_wall(a: std::time::SystemTime) -> Duration {
    a.duration_since(std::time::UNIX_EPOCH).unwrap_or_default() // CONTROL
}
#[allow(clippy::disallowed_methods, reason = "probe: the sanctioned per-site shape")]
pub fn n_allowed() -> Instant {
    Instant::now() // ALLOWED
}
