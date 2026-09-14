//! `MemFs` model rows from review lane L04: the crash-at-step countdown
//! charges directory creation as the sim tier does, and a removed or
//! clobbered file gives its preallocation back to the capacity budget.

use std::path::Path;

use inf_log::fs::SegmentFs;
use inf_log::fs::mem::MemFs;

/// L04 style row: both `SegmentFs` test tiers charge `create_dir_all`
/// against the crash countdown, so a crash-at-step index means the same
/// thing on each.
#[test]
fn create_dir_all_is_a_charged_mutating_op() {
    let fs = MemFs::new();
    fs.fail_after_ops(0);
    assert!(fs.create_dir_all(Path::new("/data/shard-0")).is_err(), "the dead fs created a dir");
    fs.clear_op_fault();
    fs.create_dir_all(Path::new("/data/shard-0")).expect("alive");
    fs.fail_after_ops(1);
    fs.create_dir_all(Path::new("/data/shard-1")).expect("one op left");
    assert!(fs.create_dir_all(Path::new("/data/shard-2")).is_err(), "the budget was spent");
}

/// L04 style row: a create/remove/create cycle inside a fixed budget
/// succeeds — the removed segment's bytes return to the pool.
#[test]
fn removing_a_segment_credits_its_preallocation_back() {
    let fs = MemFs::new();
    fs.create_dir_all(Path::new("/data")).expect("dirs");
    fs.set_capacity(Some(1 << 20));
    let seg = Path::new("/data/seg-000000.ilog");
    drop(fs.create_segment(seg, 1 << 20).expect("fits exactly"));
    fs.remove_file(seg).expect("remove");
    drop(fs.create_segment(seg, 1 << 20).expect("the freed bytes are back"));
    // Still full: a second one does not fit.
    let err = fs.create_segment(Path::new("/data/seg-000001.ilog"), 1).expect_err("full");
    assert_eq!(err.kind(), std::io::ErrorKind::StorageFull);
}

/// A rename over an existing destination frees the clobbered file.
#[test]
fn rename_clobber_credits_the_replaced_file() {
    let fs = MemFs::new();
    fs.create_dir_all(Path::new("/data")).expect("dirs");
    fs.set_capacity(Some(2 << 20));
    let a = Path::new("/data/a.ilog");
    let b = Path::new("/data/b.ilog");
    drop(fs.create_segment(a, 1 << 20).expect("a"));
    drop(fs.create_segment(b, 1 << 20).expect("b"));
    fs.rename(a, b).expect("clobber b with a");
    drop(fs.create_segment(a, 1 << 20).expect("b's bytes came back"));
}
