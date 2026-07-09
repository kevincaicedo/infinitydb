//! M2-S18 sim-disk model tests (ADR-0020 D6): barrier semantics, tear
//! granularity, rename/dir-fsync ordering, metadata prefix ordering,
//! driver-op execution, the dead switch — each verified against the
//! documented model — plus the determinism CI assert (same seed ⇒
//! byte-identical surviving disk image, two independent runs).

use std::path::{Path, PathBuf};

use inf_log::fs::sim::{SimDisk, SimDiskConfig};
use inf_log::fs::{SegmentFile, SegmentFs};

const DIR: &str = "shard-0/log";

fn disk() -> SimDisk {
    let disk = SimDisk::new();
    disk.create_dir_all(Path::new(DIR)).expect("dirs");
    disk
}

fn path(name: &str) -> PathBuf {
    Path::new(DIR).join(name)
}

/// fsync is the only data barrier: synced bytes survive EVERY cut;
/// un-synced bytes vanish on at least one seed and survive on another.
#[test]
fn fsync_is_the_only_data_barrier() {
    let mut lost = 0u32;
    let mut kept = 0u32;
    for seed in 0..32u64 {
        let disk = disk();
        let mut file = disk.create_segment(&path("seg-000000.ilog"), 4096).expect("create");
        disk.sync_dir(Path::new(DIR)).expect("dir barrier");
        file.write_at(0, b"durable-bytes").expect("write");
        file.sync_data().expect("fsync");
        file.write_at(1024, b"volatile-bytes").expect("write");
        disk.power_cut(seed);

        let bytes = disk.contents(&path("seg-000000.ilog")).expect("named + synced ⇒ survives");
        assert_eq!(&bytes[..13], b"durable-bytes", "seed {seed}: fsync-covered bytes are law");
        if bytes[1024..1024 + 14] == *b"volatile-bytes" {
            kept += 1;
        } else {
            assert!(
                bytes[1024..1024 + 14].iter().all(|&b| b == 0),
                "seed {seed}: a lost write leaves the prior (zero) bytes"
            );
            lost += 1;
        }
    }
    assert!(lost > 0, "no seed lost the un-synced write — the model is not conservative");
    assert!(kept > 0, "no seed kept the un-synced write — the model is not physical");
}

/// Tears are sector-granular: a multi-sector un-synced write can survive
/// partially, and every surviving piece is sector-aligned within the
/// write; a torn middle reads as the prior bytes (zeros).
#[test]
fn tears_have_sector_granularity() {
    let sector = 512usize;
    let mut partial = 0u32;
    for seed in 0..64u64 {
        let disk = SimDisk::with_config(SimDiskConfig { sector_bytes: sector as u32 });
        disk.create_dir_all(Path::new(DIR)).expect("dirs");
        let mut file = disk.create_segment(&path("seg-000000.ilog"), 4096).expect("create");
        disk.sync_dir(Path::new(DIR)).expect("dir barrier");
        let payload = vec![0xAB; 4 * sector];
        file.write_at(0, &payload).expect("write");
        disk.power_cut(seed);

        let bytes = disk.contents(&path("seg-000000.ilog")).expect("survives");
        let mut survived_sectors = 0;
        for s in 0..4 {
            let piece = &bytes[s * sector..(s + 1) * sector];
            if piece.iter().all(|&b| b == 0xAB) {
                survived_sectors += 1;
            } else {
                assert!(
                    piece.iter().all(|&b| b == 0),
                    "seed {seed}: sector {s} is neither fully applied nor fully absent — \
                     tear granularity violated"
                );
            }
        }
        if survived_sectors > 0 && survived_sectors < 4 {
            partial += 1;
        }
    }
    assert!(partial > 0, "no seed produced a partial-sector tear across the sweep");
}

/// Reorder: writeback may persist an OLDER version of a range while the
/// newer overwrite is lost. Across seeds the surviving byte must take
/// all three values: old, new, and neither.
#[test]
fn un_synced_overwrites_reorder() {
    let mut saw = [false; 3]; // [neither, old, new]
    for seed in 0..96u64 {
        let disk = disk();
        let mut file = disk.create_segment(&path("seg-000000.ilog"), 512).expect("create");
        disk.sync_dir(Path::new(DIR)).expect("dir barrier");
        file.write_at(0, &[0x11; 64]).expect("older version");
        file.write_at(0, &[0x22; 64]).expect("newer version");
        disk.power_cut(seed);
        let bytes = disk.contents(&path("seg-000000.ilog")).expect("survives");
        match bytes[0] {
            0x00 => saw[0] = true,
            0x11 => saw[1] = true,
            0x22 => saw[2] = true,
            other => panic!("seed {seed}: impossible surviving byte {other:#x}"),
        }
        // The OS view before the cut always read the newest version; only
        // the surviving image reorders.
    }
    assert!(saw[0], "no seed lost both versions");
    assert!(saw[1], "no seed persisted the OLD version over the new (reorder never observed)");
    assert!(saw[2], "no seed kept the newest version");
}

/// Rename is a metadata op ordered only by dir-fsync: before the
/// barrier, a cut may keep the old name or the new (never both, never
/// neither); after the barrier, the rename is law. fdatasync on the
/// FILE does not commit the name.
#[test]
fn rename_is_ordered_only_by_dir_fsync() {
    let mut old_name = 0u32;
    let mut new_name = 0u32;
    for seed in 0..32u64 {
        let disk = disk();
        let mut file = disk.create_meta(&path("META.new")).expect("staging");
        file.write_at(0, b"catalog-v2").expect("write");
        file.sync_data().expect("data barrier");
        disk.sync_dir(Path::new(DIR)).expect("create committed");
        disk.rename(&path("META.new"), &path("META")).expect("swap");
        disk.power_cut(seed);

        let old = disk.contents(&path("META.new"));
        let new = disk.contents(&path("META"));
        match (old, new) {
            (Some(_), None) => old_name += 1,
            (None, Some(bytes)) => {
                assert_eq!(bytes, b"catalog-v2", "seed {seed}: renamed file keeps synced data");
                new_name += 1;
            }
            (Some(_), Some(_)) => panic!("seed {seed}: rename duplicated the file"),
            (None, None) => panic!("seed {seed}: rename lost the file entirely"),
        }
    }
    assert!(old_name > 0, "no seed kept the old name — un-barriered rename must be volatile");
    assert!(new_name > 0, "no seed kept the new name");

    // After the dir barrier the rename survives every cut.
    for seed in 0..16u64 {
        let disk = disk();
        let mut file = disk.create_meta(&path("META.new")).expect("staging");
        file.write_at(0, b"catalog-v2").expect("write");
        file.sync_data().expect("data barrier");
        disk.sync_dir(Path::new(DIR)).expect("create committed");
        disk.rename(&path("META.new"), &path("META")).expect("swap");
        disk.sync_dir(Path::new(DIR)).expect("rename committed");
        disk.power_cut(seed);
        assert!(disk.contents(&path("META")).is_some(), "seed {seed}: committed rename is law");
        assert!(disk.contents(&path("META.new")).is_none(), "seed {seed}: old name gone");
    }
}

/// The fsync-the-directory lesson: a created file whose parent was never
/// dir-fsynced may vanish wholesale — even with its data fdatasync'd.
#[test]
fn create_without_dir_fsync_may_vanish_despite_fdatasync() {
    let mut vanished = 0u32;
    let mut survived = 0u32;
    for seed in 0..32u64 {
        let disk = disk();
        let mut file = disk.create_segment(&path("seg-000000.ilog"), 512).expect("create");
        file.write_at(0, b"synced-but-orphaned").expect("write");
        file.sync_data().expect("fdatasync");
        // NO sync_dir: the name is volatile.
        disk.power_cut(seed);
        match disk.contents(&path("seg-000000.ilog")) {
            None => vanished += 1,
            Some(bytes) => {
                assert_eq!(&bytes[..19], b"synced-but-orphaned", "surviving name ⇒ synced data");
                survived += 1;
            }
        }
    }
    assert!(vanished > 0, "no seed dropped the un-barriered create");
    assert!(survived > 0, "no seed kept the un-barriered create");
}

/// A remove without dir-fsync may resurrect (ADR-0017: boot GC
/// re-collects); after the barrier it is permanent.
#[test]
fn remove_without_dir_fsync_may_resurrect() {
    let mut resurrected = 0u32;
    for seed in 0..32u64 {
        let disk = disk();
        let mut file = disk.create_segment(&path("seg-000000.ilog"), 512).expect("create");
        file.write_at(0, b"stale").expect("write");
        file.sync_data().expect("fsync");
        disk.sync_dir(Path::new(DIR)).expect("committed");
        disk.remove_file(&path("seg-000000.ilog")).expect("remove");
        assert!(disk.contents(&path("seg-000000.ilog")).is_none(), "OS view: gone");
        disk.power_cut(seed);
        if disk.contents(&path("seg-000000.ilog")).is_some() {
            resurrected += 1;
        }
    }
    assert!(resurrected > 0, "no seed resurrected the un-barriered remove");

    let disk = disk();
    let mut file = disk.create_segment(&path("seg-000000.ilog"), 512).expect("create");
    file.sync_data().expect("fsync");
    disk.sync_dir(Path::new(DIR)).expect("committed");
    disk.remove_file(&path("seg-000000.ilog")).expect("remove");
    disk.sync_dir(Path::new(DIR)).expect("remove committed");
    disk.power_cut(7);
    assert!(disk.contents(&path("seg-000000.ilog")).is_none(), "committed remove is law");
}

/// Metadata ops survive as a per-directory PREFIX of issue order
/// (journal-commit truncation): a later op never survives a lost earlier
/// one — no rename-without-create, no create-B-without-create-A.
#[test]
fn metadata_survival_is_a_prefix_of_issue_order() {
    for seed in 0..64u64 {
        let disk = disk();
        drop(disk.create_meta(&path("a")).expect("create a"));
        drop(disk.create_meta(&path("b")).expect("create b"));
        disk.rename(&path("b"), &path("c")).expect("rename b->c");
        disk.power_cut(seed);
        let a = disk.contents(&path("a")).is_some();
        let b = disk.contents(&path("b")).is_some();
        let c = disk.contents(&path("c")).is_some();
        // Legal prefixes: {} / {a} / {a,b} / {a,c}.
        assert!(
            matches!(
                (a, b, c),
                (false, false, false)
                    | (true, false, false)
                    | (true, true, false)
                    | (true, false, true)
            ),
            "seed {seed}: survived state (a={a}, b={b}, c={c}) is not a prefix of issue order"
        );
    }
}

/// The determinism CI assert (L7): the same seed over the same op
/// sequence — two INDEPENDENT runs — yields a byte-identical surviving
/// image and digest.
#[test]
fn same_seed_yields_byte_identical_surviving_image() {
    let build = |seed: u64| {
        let disk = disk();
        let mut seg = disk.create_segment(&path("seg-000000.ilog"), 8192).expect("create");
        disk.sync_dir(Path::new(DIR)).expect("barrier");
        let mut payload = Vec::new();
        for i in 0..2048u64 {
            payload.push((i.wrapping_mul(31) & 0xFF) as u8);
        }
        seg.write_at(0, &payload).expect("write");
        seg.sync_data().expect("fsync");
        seg.write_at(2048, &payload).expect("volatile 1");
        seg.write_at(2048 + 512, &[0x77; 1024]).expect("volatile 2, overlapping");
        let mut meta = disk.create_meta(&path("META.new")).expect("staging");
        meta.write_at(0, b"unit-2").expect("meta write");
        disk.rename(&path("META.new"), &path("META")).expect("swap, un-barriered");
        disk.power_cut(seed);
        (disk.image(), disk.image_digest())
    };
    for seed in [0u64, 1, 0xC0FFEE, 0xD15C_0BAD] {
        let (image_a, digest_a) = build(seed);
        let (image_b, digest_b) = build(seed);
        assert_eq!(image_a, image_b, "seed {seed:#x}: surviving images diverged (L7)");
        assert_eq!(digest_a, digest_b, "seed {seed:#x}: digests diverged");
    }
    // And different seeds must be able to diverge (the draws are real).
    let (_, d0) = build(0);
    let (_, d1) = build(1);
    let (_, d2) = build(2);
    assert!(d0 != d1 || d1 != d2, "three seeds, one image — the model is not drawing");
}

/// The dead switch: after `cut_after_ops(n)`, op n+1 fails with the
/// named error and the disk stays dead until `power_cut` revives it.
#[test]
fn cut_after_ops_kills_then_power_cut_revives() {
    let disk = disk();
    let mut file = disk.create_segment(&path("seg-000000.ilog"), 512).expect("create");
    disk.sync_dir(Path::new(DIR)).expect("barrier");
    file.sync_data().expect("fsync");
    disk.cut_after_ops(1);
    file.write_at(0, b"last-op").expect("op within budget");
    let err = file.write_at(64, b"beyond").expect_err("dead disk");
    assert!(err.to_string().contains("power lost"), "{err}");
    let err = disk.sync_dir(Path::new(DIR)).expect_err("everything fails dead");
    assert!(err.to_string().contains("power lost"), "{err}");
    disk.power_cut(3);
    // Revived: the surviving image serves again.
    let mut file = disk.open_write(&path("seg-000000.ilog")).expect("reopen");
    file.write_at(0, b"reborn").expect("post-reboot write");
}

/// Driver-op execution (ADR-0020 D7): `driver_write_at` is page-cache
/// (volatile until `driver_fdatasync`); a dir fd routes the barrier.
#[test]
fn driver_ops_execute_against_the_same_layers() {
    let mut lost = 0u32;
    for seed in 0..24u64 {
        let disk = disk();
        let seg = disk.create_segment(&path("seg-000000.ilog"), 4096).expect("create");
        let dir = disk.open_dir(Path::new(DIR)).expect("dir handle");
        let seg_fd = seg.raw_fd().expect("sim fds exist");
        let dir_fd = dir.raw_fd().expect("dir fd");
        // Commit the create through the DRIVER dir barrier.
        disk.driver_fdatasync(dir_fd).expect("dir barrier via driver");
        disk.driver_write_at(seg_fd, 0, b"frame-one").expect("LogWrite");
        disk.driver_fdatasync(seg_fd).expect("linked fsync");
        disk.driver_write_at(seg_fd, 512, b"frame-two-unsynced").expect("LogWrite");
        disk.power_cut(seed);
        let bytes = disk.contents(&path("seg-000000.ilog")).expect("committed name survives");
        assert_eq!(&bytes[..9], b"frame-one", "seed {seed}: synced driver write survives");
        if bytes.len() < 512 + 18 || bytes[512..512 + 18] != *b"frame-two-unsynced" {
            lost += 1;
        }
    }
    assert!(lost > 0, "no seed lost the un-synced driver write");

    let disk = disk();
    let err = disk.driver_write_at(42, 0, b"x").expect_err("unknown fd");
    assert!(err.to_string().contains("not a sim file fd"), "{err}");
}

/// Cross-directory renames are rejected loudly (the documented model
/// constraint), and an open handle follows its inode across a rename
/// (POSIX fd semantics) — syncing through it syncs the renamed file.
#[test]
fn handles_follow_inodes_and_cross_dir_renames_refuse() {
    let disk = disk();
    disk.create_dir_all(Path::new("shard-0/ckpt")).expect("other dir");
    let mut file = disk.create_meta(&path("META.new")).expect("staging");
    let err = disk
        .rename(&path("META.new"), Path::new("shard-0/ckpt/META"))
        .expect_err("cross-dir rename");
    assert!(err.to_string().contains("same-directory"), "{err}");

    disk.rename(&path("META.new"), &path("META")).expect("same-dir swap");
    file.write_at(0, b"through-old-handle").expect("write via pre-rename handle");
    file.sync_data().expect("sync via pre-rename handle");
    disk.sync_dir(Path::new(DIR)).expect("commit");
    disk.power_cut(11);
    let bytes = disk.contents(&path("META")).expect("renamed + committed");
    assert_eq!(&bytes[..18], b"through-old-handle", "handle followed the inode");
}
