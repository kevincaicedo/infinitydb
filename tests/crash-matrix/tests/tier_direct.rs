//! Review 2026-08-30, F-L04-14: the tier and blob write fault points on a
//! **`Direct`-mode** file. `O_DIRECT` takes whole aligned blocks only
//! (ADR-0054 D2); a point that tears a write to a sub-block length is
//! refused `EINVAL` by a real fd and asserted by the simulator once it
//! honors the mode — so the "torn frame" physics never occurred on the
//! default tier mode, and the rows in `tier.rs` proved it on `MemFs`
//! alone. Each row here runs on `SimDisk` with `TierIoMode::Direct`
//! (alignment-asserted) and re-proves the `m4.toml` contract; one leg
//! runs on a real `O_DIRECT` fd where the filesystem offers it.
//! Every test states its goal and method in its first sentence.

use std::path::Path;

use inf_foundation::fault::{self, FaultSpec};
use inf_log::fs::sim::SimDisk;
use inf_log::fs::{SegmentFile, SegmentFs};
use inf_log::{
    ExtentId, ExtentWriter, NsId, SealReason, TIER_FOOTER_BYTES, TIER_FRAME_BYTES, TIER_FRAME_DATA,
    TierDecodeError, TierIdentity, TierIoMode, TierWriter, inspect_extent_bytes,
    inspect_tier_bytes, probe_tier_file,
};
use inf_store::LogicalAddr;

const NS: NsId = NsId(21);
const SHARD: &str = "shard-0";

fn identity() -> TierIdentity {
    TierIdentity { cell: 0, ns: NS, base: LogicalAddr::ZERO }
}

fn direct_writer(disk: &SimDisk) -> TierWriter<SimDisk> {
    TierWriter::create(disk, Path::new(SHARD), 0, 0, NS, LogicalAddr::ZERO, TierIoMode::Direct)
        .expect("create Direct tier file")
}

/// Three full frames, synced — the manifested prefix the claim rule
/// covers.
fn manifested_prefix(w: &mut TierWriter<SimDisk>) -> u64 {
    let manifested = 3 * TIER_FRAME_DATA as u64;
    w.append(LogicalAddr::ZERO, &vec![0x11; manifested as usize]).expect("append");
    w.sync().expect("sync");
    assert_eq!(w.confirmable_len(), manifested);
    manifested
}

/// Goal: `tier_torn_frame` injects on a Direct file — a prefix lands,
/// the call succeeds, and recovery reseals at the manifested watermark
/// (`reseal-at-watermark`). Method: the sim asserts the O_DIRECT
/// contract on every write of a Direct inode; before the fix the point
/// wrote `4096 × 2 / 3` bytes and this test died in that assertion.
#[test]
fn tier_torn_frame_injects_on_a_direct_tier_file() {
    let disk = SimDisk::new();
    let mut w = direct_writer(&disk);
    let manifested = manifested_prefix(&mut w);
    fault::arm("tier_torn_frame", FaultSpec::Nth(1));
    let mut at = manifested;
    for _ in 0..2 {
        w.append(LogicalAddr::from_raw(at).expect("fits"), &[0x22; TIER_FRAME_DATA])
            .expect("append");
        w.sync().expect("a torn write reports success (lying-disk physics)");
        at += TIER_FRAME_DATA as u64;
    }
    assert!(fault::fired("tier_torn_frame") >= 1, "the row is not vacuous");
    fault::disarm_all();
    let path = w.path().to_path_buf();
    drop(w); // crash
    let image = disk.contents(&path).expect("file exists");
    let summary = inspect_tier_bytes(&image).expect("unsealed image parses");
    assert!(summary.sealed.is_none(), "no footer — the file is unsealed");
    assert_eq!(
        summary.first_bad_frame,
        Some(3),
        "the torn frame is the first un-manifested one and CRC-detected"
    );
    assert_eq!(image.len(), 4096 + 5 * TIER_FRAME_BYTES, "the clean frame landed past the tear");
    let path = TierWriter::<SimDisk>::recover_seal_existing(
        &disk,
        Path::new(SHARD),
        0,
        identity(),
        manifested,
        TierIoMode::Direct,
    )
    .expect("recover");
    let image = disk.contents(&path).expect("file exists");
    let summary = inspect_tier_bytes(&image).expect("sealed image parses");
    let footer = summary.sealed.expect("resealed");
    assert_eq!(footer.data_len, manifested);
    assert_eq!(footer.reason, SealReason::Recovered);
    assert_eq!(summary.first_bad_frame, None, "every retained frame verifies");
    assert_eq!(image.len(), 4096 + 3 * TIER_FRAME_BYTES + TIER_FOOTER_BYTES);
}

/// Goal: the torn prefix is sector-granular and keeps the block's prior
/// content beyond the tear — a rewritten partial tail frame that tears
/// carries the *old* CRC trailer, never zeros or the new one. Method:
/// fill a tail frame, sync, extend it, tear the rewrite, compare
/// sectors.
#[test]
fn tier_torn_frame_keeps_the_old_sectors_beyond_the_tear() {
    let disk = SimDisk::new();
    let mut w = direct_writer(&disk);
    w.append(LogicalAddr::ZERO, &[0x11; 1000]).expect("append");
    w.sync().expect("sync");
    let path = w.path().to_path_buf();
    let before = disk.contents(&path).expect("file exists");
    fault::arm("tier_torn_frame", FaultSpec::Nth(1));
    w.append(LogicalAddr::from_raw(1000).expect("fits"), &[0x22; 3000]).expect("append");
    w.sync().expect("torn rewrite reports success");
    assert_eq!(fault::fired("tier_torn_frame"), 1);
    fault::disarm_all();
    let after = disk.contents(&path).expect("file exists");
    assert_eq!(after.len(), before.len(), "one frame, rewritten in place");
    let frame = 4096..4096 + TIER_FRAME_BYTES;
    let cut = TIER_FRAME_BYTES * 2 / 3 / 512 * 512;
    assert_eq!(cut, 2560, "the tear rounds down to the 512 B sector grid");
    assert_eq!(&after[frame.start..frame.start + 1000], &[0x11; 1000][..]);
    assert_eq!(&after[frame.start + 1000..frame.start + cut], &[0x22; 1560][..], "new sectors");
    assert_eq!(
        &after[frame.start + cut..frame.end],
        &before[frame.start + cut..frame.end],
        "the sectors beyond the tear hold the frame's prior content, old CRC included"
    );
    let summary = inspect_tier_bytes(&after).expect("parses");
    assert_eq!(summary.first_bad_frame, Some(0), "CRC-detected");
}

/// Goal: `tier_short_write` injects on a Direct file for a single-frame
/// write — `4096 / 2` is not a block — and the row's contract holds
/// (`append-fails-typed`: the range is never appended). Method: one
/// partial-tail-frame sync under the point.
#[test]
fn tier_short_write_injects_on_a_direct_tier_file() {
    let disk = SimDisk::new();
    let mut w = direct_writer(&disk);
    w.append(LogicalAddr::ZERO, &[0x11; 500]).expect("append");
    fault::arm("tier_short_write", FaultSpec::Nth(1));
    let err = w.sync().expect_err("a short write fails the sync");
    assert!(err.to_string().contains("tier_short_write"), "typed + named: {err}");
    assert!(fault::fired("tier_short_write") >= 1, "the row is not vacuous");
    fault::disarm_all();
    assert_eq!(w.confirmable_len(), 0, "nothing claimable");
    let image = disk.contents(w.path()).expect("file exists");
    assert_eq!(image.len(), 4096 + TIER_FRAME_BYTES, "the prefix landed, block-shaped");
    let summary = inspect_tier_bytes(&image).expect("parses");
    assert_eq!(summary.first_bad_frame, Some(0), "the torn frame never verifies");
    w.sync().expect("the retry rewrites the frame whole");
    let image = disk.contents(w.path()).expect("file exists");
    assert_eq!(inspect_tier_bytes(&image).expect("parses").first_bad_frame, None);
}

/// Goal: `tier_footer_torn` injects on a Direct file — the footer block
/// lands with its CRC cover torn, the probe reads it as unsealed (its
/// CRC-refuse branch), and recovery reseals at the watermark. Method:
/// the footer's 12-byte prefix is written as one legal block whose tail
/// is zero.
#[test]
fn tier_footer_torn_injects_on_a_direct_tier_file() {
    let disk = SimDisk::new();
    let mut w = direct_writer(&disk);
    let manifested = manifested_prefix(&mut w);
    fault::arm("tier_footer_torn", FaultSpec::Nth(1));
    let path = w.path().to_path_buf();
    let err = w.seal(SealReason::Shutdown).expect_err("the seal dies mid-footer");
    assert!(err.to_string().contains("tier_footer_torn"), "typed + named: {err}");
    assert!(fault::fired("tier_footer_torn") >= 1, "the row is not vacuous");
    fault::disarm_all();
    let image = disk.contents(&path).expect("file exists");
    assert_eq!(image.len(), 4096 + 3 * TIER_FRAME_BYTES + TIER_FOOTER_BYTES, "footer block landed");
    assert_eq!(inspect_tier_bytes(&image), Err(TierDecodeError::BadCrc), "refused typed");
    let (_, footer) = probe_tier_file(&disk, &path).expect("header intact");
    assert!(footer.is_none(), "a torn footer never reads as sealed");
    let path = TierWriter::<SimDisk>::recover_seal_existing(
        &disk,
        Path::new(SHARD),
        0,
        identity(),
        manifested,
        TierIoMode::Direct,
    )
    .expect("recover");
    let image = disk.contents(&path).expect("file exists");
    let footer = inspect_tier_bytes(&image).expect("parses").sealed.expect("resealed");
    assert_eq!(footer.data_len, manifested);
    assert_eq!(footer.reason, SealReason::Recovered);
}

/// Goal: `blob_short_write` injects on a Direct extent for a one-frame
/// extent (`4096 / 2` again) and the extent is abandoned typed. Method:
/// a 100-byte extent's finish under the point.
#[test]
fn blob_short_write_injects_on_a_direct_extent() {
    let disk = SimDisk::new();
    let mut w =
        ExtentWriter::create(&disk, Path::new(SHARD), ExtentId(1), 0, NS, 100, TierIoMode::Direct)
            .expect("create Direct extent");
    w.append_chunk(&[0x33; 100]).expect("chunk");
    fault::arm("blob_short_write", FaultSpec::Nth(1));
    let path = w.path().to_path_buf();
    let err = w.finish().expect_err("a short write abandons the extent");
    assert!(err.to_string().contains("blob_short_write"), "typed + named: {err}");
    assert!(fault::fired("blob_short_write") >= 1, "the row is not vacuous");
    fault::disarm_all();
    let image = disk.contents(&path).expect("file exists");
    let summary = inspect_extent_bytes(&image).expect("header parses");
    assert!(!summary.complete, "the torn frame never verifies");
}

/// Goal: on a real `O_DIRECT` fd the torn-frame point *lands* (the
/// finding: it answered `EINVAL`, and the row observed the write-failure
/// path instead of the torn-write path). Method: `StdSegmentFs` under
/// `TMPDIR`; skips, disclosed, where the filesystem refuses `O_DIRECT`
/// (tmpfs — run with `TMPDIR` on a real device for the leg).
#[cfg(target_os = "linux")]
#[test]
fn tier_torn_frame_lands_on_a_real_direct_fd() {
    use inf_log::fs::StdSegmentFs;
    let dir = std::env::temp_dir().join(format!("inf-tier-direct-torn-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let fs = StdSegmentFs;
    let created = TierWriter::create(&fs, &dir, 0, 0, NS, LogicalAddr::ZERO, TierIoMode::Direct);
    let mut w = match created {
        Ok(w) => w,
        Err(error)
            if error.kind() == std::io::ErrorKind::Unsupported
                || error.raw_os_error() == Some(22) =>
        {
            eprintln!("skipping: {error} (fs without O_DIRECT; set TMPDIR to a real device)");
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
        Err(error) => panic!("create_tier(Direct): {error}"),
    };
    let manifested = 3 * TIER_FRAME_DATA as u64;
    w.append(LogicalAddr::ZERO, &vec![0x11; manifested as usize]).expect("append");
    w.sync().expect("sync");
    fault::arm("tier_torn_frame", FaultSpec::Nth(1));
    w.append(LogicalAddr::from_raw(manifested).expect("fits"), &[0x22; TIER_FRAME_DATA])
        .expect("append");
    let outcome = w.sync();
    let fired = fault::fired("tier_torn_frame");
    fault::disarm_all();
    assert_eq!(fired, 1, "the point fired once");
    outcome.expect("the torn write lands on the O_DIRECT fd (EINVAL = the finding)");
    let file = fs.open_read(w.path()).expect("reopen buffered");
    let len = file.file_size().expect("size") as usize;
    let mut image = vec![0u8; len];
    let mut read = 0;
    while read < len {
        let n = file.read_at(read as u64, &mut image[read..]).expect("read");
        assert!(n > 0);
        read += n;
    }
    let summary = inspect_tier_bytes(&image).expect("parses");
    assert_eq!(summary.first_bad_frame, Some(3), "the torn frame is CRC-detected on the device");
    drop(w);
    let _ = std::fs::remove_dir_all(&dir);
}
