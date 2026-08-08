//! Thread-affinity escape hatch for boot-scoped helper threads
//! (M2.5-S08): a cell thread is core-pinned, and any thread it spawns
//! inherits that single-core mask — a recovery prefetch thread would then
//! timeshare with the apply loop it exists to overlap. Clearing the
//! inherited mask lets the helper float on the remaining cores.

/// Reset the calling thread's CPU affinity to all online CPUs (Linux;
/// no-op elsewhere). Best-effort: a failure just leaves the inherited
/// mask, costing overlap, never correctness.
pub fn unpin_current_thread() {
    #[cfg(target_os = "linux")]
    {
        // SAFETY: cpu_set_t is a plain local bitmask; sched_setaffinity
        // reads it by pointer with the matching size and touches no other
        // caller memory. tid 0 = the calling thread. An EINVAL for CPUs
        // not actually online is impossible with a full mask on Linux
        // (the kernel intersects with the online set).
        unsafe {
            let mut set: libc::cpu_set_t = core::mem::zeroed();
            for cpu in 0..libc::CPU_SETSIZE as usize {
                libc::CPU_SET(cpu, &mut set);
            }
            let _ = libc::sched_setaffinity(0, core::mem::size_of::<libc::cpu_set_t>(), &set);
        }
    }
}
