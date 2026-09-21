//! Boot-read prefetch scoping (ADR-0109; L17-02 of the 2026-08-30 review).
//!
//! The M2.5-S08 prefetch wrapper used to be the plane's filesystem for the
//! node's life — "boot-scoped" was a comment. Now it is a type: only
//! `Recovery::boot_reads` holds it, and `Recovery::finish` hands the plane
//! a `SegmentRotor<F>` over the bare tier. This test pins the two
//! observable halves on a real on-disk log with `StdSegmentFs`:
//!
//! - with `boot_prefetch` on, the prefetch thread is spawned for the boot
//!   readers (the counter moves) and **every one of them is joined by the
//!   time recovery completes** — no `inf-readahead` thread survives
//!   `finish` (boot-scoped in fact, not just in type);
//! - with it off (the DST/default arm) no thread is ever spawned;
//! - both arms recover the same state (L7: the hint changes no byte).
//!
//! The typed half is the `let _: SegmentRotor<StdSegmentFs>` below: the
//! rotor the plane inherits is over the bare tier by signature.
#![cfg(target_os = "linux")]

use std::path::{Path, PathBuf};

use inf_foundation::time::Nanos;
use inf_log::fs::StdSegmentFs;
use inf_log::{
    CkptConfig, MutationEffect, NsId, SegmentConfig, SegmentRotor, StagingConfig, StagingRing,
    create_cell_dirs,
};
use inf_server::{DurableConfig, Recovery, RecoveryProgress, boot_prefetch_threads_spawned};
use inf_store::{FsyncClass, Keyspace, NsMode, NsSpec, StateDigest, StoreConfig, WallAnchor};

const NS: NsId = NsId(16);
const CELL: u16 = 0;

fn now() -> Nanos {
    Nanos::from_millis(1)
}

fn anchor() -> WallAnchor {
    WallAnchor { internal_ms: 0, unix_ms: 1_750_000_000_000 }
}

/// A real directory under the target tree (never `/tmp`: the durable
/// dirs there ride a tmpfs quota on the dev box).
fn scratch(tag: &str) -> PathBuf {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/e2e-readahead")
        .join(format!("{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("scratch dir");
    root
}

fn cfg(root: &Path, boot_prefetch: bool) -> DurableConfig {
    DurableConfig {
        data_dir: root.to_path_buf(),
        staging: StagingConfig::default(),
        segment: SegmentConfig { segment_bytes: 64 << 10, ..Default::default() },
        ckpt: CkptConfig::default(),
        recover: inf_server::RecoverConfig { boot_prefetch, ..Default::default() },
        flush_bound: 1,
        fua_p50_us_probed: 0,
        device: Default::default(),
        fill: Default::default(),
        group: Default::default(),
    }
}

fn fresh_keyspace() -> Keyspace {
    let mut ks = Keyspace::new(StoreConfig::default());
    ks.ns_create(NsSpec {
        id: NS,
        name: b"ledger".to_vec(),
        mode: NsMode::Durable,
        fsync: Some(FsyncClass::Always),
        policy: None,
        maxmemory: None,
        tier: None,
    })
    .expect("ns");
    ks
}

/// Writes `frames` frames of `per_frame` string sets across a few
/// segments on the real disk.
fn write_log(root: &Path, frames: usize, per_frame: usize) {
    let config = cfg(root, false);
    let shard = root.join(format!("shard-{CELL}"));
    let dirs = create_cell_dirs(&StdSegmentFs, &shard).expect("dirs");
    let mut rotor =
        SegmentRotor::create_fresh(StdSegmentFs, dirs.log.clone(), config.segment).expect("rotor");
    let mut ring = StagingRing::new(config.staging);
    let value = vec![0x5Au8; 512];
    for frame in 0..frames {
        for i in 0..per_frame {
            let key = format!("k:{frame:04}:{i:03}");
            ring.stage(&MutationEffect::StringSet { ns: NS, key: key.as_bytes(), value: &value })
                .expect("stage");
        }
        rotor.maintain(0).expect("maintain");
        let lease = ring.flush_into(&mut rotor, 0).expect("flush").expect("a frame");
        ring.release(lease);
    }
    drop(rotor);
}

/// The kernel's `PF_EXITING` task flag (`include/linux/sched.h`).
const PF_EXITING: u64 = 0x4;

/// Names of every thread of this process still running user code, from
/// `/proc/self/task`. A task with `PF_EXITING` set is excluded: `join`
/// returns when the kernel clears the thread's tid — inside `do_exit`,
/// before the task is unhashed from `/proc` — so a joined worker can be
/// listed for a few more microseconds (measured under load: state `R`,
/// flags `0x40004c`, gone after 94 µs; 2 of 300 runs counted it). A
/// worker that was never stopped carries no such flag and is still
/// counted, which is what `assert_a_live_prefetch_thread_is_counted` pins.
fn thread_names() -> Vec<String> {
    let mut names = Vec::new();
    for entry in std::fs::read_dir("/proc/self/task").expect("procfs") {
        let task = entry.expect("task").path();
        let Ok(name) = std::fs::read_to_string(task.join("comm")) else { continue };
        let Ok(stat) = std::fs::read_to_string(task.join("stat")) else { continue };
        // Fields after the parenthesised comm: state is 0, flags is 6.
        let exiting = stat
            .rsplit_once(')')
            .and_then(|(_, rest)| rest.split_whitespace().nth(6))
            .and_then(|flags| flags.parse::<u64>().ok())
            .is_some_and(|flags| flags & PF_EXITING != 0);
        if !exiting {
            names.push(name.trim().to_string());
        }
    }
    names
}

fn live_prefetch_threads() -> usize {
    thread_names().iter().filter(|n| n.as_str() == "inf-readahead").count()
}

/// The oracle's canary: a worker that is alive is counted — excluding
/// exiting tasks must not blind the scan to a leaked one. Called from the
/// scoping test itself (one scanner per process: a second test's live
/// worker would be this one's false positive).
fn assert_a_live_prefetch_thread_is_counted() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let stop = std::sync::Arc::new(AtomicBool::new(false));
    let started = std::sync::Arc::new(AtomicBool::new(false));
    let (seen, running) = (std::sync::Arc::clone(&stop), std::sync::Arc::clone(&started));
    let worker = std::thread::Builder::new()
        .name("inf-readahead".into())
        .spawn(move || {
            // The name is set by the thread itself, before this closure runs.
            running.store(true, Ordering::Release);
            while !seen.load(Ordering::Acquire) {
                std::thread::park_timeout(std::time::Duration::from_millis(1));
            }
        })
        .expect("spawn");
    while !started.load(Ordering::Acquire) {
        std::thread::yield_now();
    }
    assert_eq!(live_prefetch_threads(), 1, "a parked worker is a live thread");
    stop.store(true, Ordering::Release);
    worker.thread().unpark();
    worker.join().expect("join");
    assert_eq!(live_prefetch_threads(), 0, "a joined worker is not");
}

/// Recovers the log at `root` with the given arm; returns the digest and
/// the number of prefetch threads spawned during the run.
fn recover(root: &Path, boot_prefetch: bool) -> (StateDigest, u64) {
    let before = boot_prefetch_threads_spawned();
    let mut ks = fresh_keyspace();
    let mut rec = Recovery::new(StdSegmentFs, CELL, &cfg(root, boot_prefetch), anchor(), now());
    while rec.step(&mut ks, u64::MAX).expect("step") == RecoveryProgress::Working {}
    // The typed half: the rotor the plane inherits is over the bare tier.
    let (rotor, _stats, _seed): (SegmentRotor<StdSegmentFs>, _, _) = rec.finish();
    drop(rotor);
    (ks.state_digest(now()), boot_prefetch_threads_spawned() - before)
}

#[test]
fn boot_prefetch_is_scoped_to_recovery_and_changes_no_byte() {
    assert_a_live_prefetch_thread_is_counted();
    let root = scratch("scoped");
    write_log(&root, 24, 16);
    assert_eq!(live_prefetch_threads(), 0, "clean start");

    // The DST/default arm: pure delegation, no thread at all.
    let (digest_off, spawned_off) = recover(&root, false);
    assert_eq!(spawned_off, 0, "boot_prefetch = false never spawns");
    assert_eq!(live_prefetch_threads(), 0);

    // The infinityd single-cell arm: the boot readers prefetch …
    let (digest_on, spawned_on) = recover(&root, true);
    assert!(spawned_on >= 1, "the segment/audit readers opened through the wrapper: {spawned_on}");
    // … and every worker was joined before `finish` returned: the wrapper
    // died with the machine. Before ADR-0109 the plane kept the wrapper
    // (and its spawn path) for the node's life.
    assert_eq!(live_prefetch_threads(), 0, "every prefetch thread joined by recovery's end");

    // L7: the hint populates the page cache and changes nothing the
    // readers see.
    assert_eq!(digest_on, digest_off, "prefetch changed a recovered byte");

    // A second boot with prefetch on (the GC'd, reopened directory) —
    // still scoped, still identical.
    let (digest_again, spawned_again) = recover(&root, true);
    assert!(spawned_again >= 1);
    assert_eq!(live_prefetch_threads(), 0);
    assert_eq!(digest_again, digest_off);
    let _ = std::fs::remove_dir_all(&root);
}
