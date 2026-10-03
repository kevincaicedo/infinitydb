//! Boot replay's fault rows (ADR-0174 D1, D2, D3, D5) — carried at the
//! node tier per `m4.toml`:
//!
//! - `boot-refuses-typed-then-recovers` — a boot settle read that fails
//!   (`replay_settle_read_fail`) is the typed refusal: a `DEL` whose read
//!   failed has changed nothing (the key still resolves, no slot moved,
//!   the boundary stayed), the boot stops typed, and a later boot with
//!   the fault cleared recovers the same unit and applies the `DEL`.
//! - `demote-step-refuses-typed-then-recovers` — a tier write, barrier,
//!   seal or create that fails inside a boot demote step
//!   (`tier_short_write`, `tier_write_nospace`, `tier_fsync_err`,
//!   `tier_footer_torn`, `tier_dir_open_fail`) is the typed refusal:
//!   `flushed` is where the step found it, the refusal names the bytes no
//!   barrier covers and the handles held, at most an unmanifested file is
//!   left, and a later boot with the fault cleared removes it and
//!   recovers the unit.
//! - `demote-step-cut-then-recovers` — a torn tier write
//!   (`tier_torn_frame`) succeeds and stands for the last write before a
//!   power cut: the boot is cut right after it, and the next boot
//!   recovers the unit.

#[path = "../receipt.rs"]
mod receipt;

use std::path::Path;

use inf_foundation::fault::{self, FaultSpec};
use inf_log::fs::SegmentFs;
use inf_log::fs::mem::MemFs;
use inf_log::{
    CkptConfig, Lsn, Manifest, NsId, RecordView, SegmentId, SyncIckWriter, TierFlush,
    TierFlushConfig, TierFlushError, TierIoMode, decode_record, read_ick_hybrid, read_manifest,
    write_manifest,
};
use inf_store::KeyHasher;
use inf_store::{
    AddressSpaceConfig, DemotionConfig, LogicalAddr, RecoveredTier, ReplayRefusal, SettleProgress,
    TierReplay, TieredLookup, TieredTable, apply_live_set_section, apply_ref_section,
    recover_tiered_ns,
};

const NS: NsId = NsId(31);
const PAGE: u64 = 4 << 10;
const BUDGET: u64 = 1 << 20;
const SHARD: &str = "shard-0";

fn flush_config() -> TierFlushConfig {
    TierFlushConfig {
        shard_dir: Path::new(SHARD).to_path_buf(),
        cell: 0,
        ns: NS,
        mode: TierIoMode::Buffered,
        file_capacity: 256 << 10,
        slice_bytes: PAGE,
    }
}

fn demote() -> DemotionConfig {
    DemotionConfig::for_budget(BUDGET, PAGE)
}

fn space_config(origin: u64) -> AddressSpaceConfig {
    AddressSpaceConfig {
        reserve_bytes: demote().ring_reserve_bytes().expect("valid budget"),
        page_bytes: PAGE as usize,
        life_origin: LogicalAddr::from_raw(origin).expect("48-bit"),
    }
}

fn maintain(table: &mut TieredTable, flush: &mut TierFlush<MemFs>) {
    loop {
        let sealed = table.seal_slice();
        let f = table.flush_slice(flush).expect("flush slice");
        let released = table.release_slice();
        if sealed + released + f.appended_bytes + u64::from(f.gaps_crossed) == 0 {
            break;
        }
    }
}

/// The crashed life: a checkpoint of nothing, then two windows of tail
/// records around one key, and that key's `DEL` last.
fn crashed_life() -> (MemFs, Vec<u8>) {
    let fs = MemFs::new();
    fs.create_dir_all(Path::new(SHARD)).expect("shard dir");
    let mut table =
        TieredTable::new(space_config(0), demote(), 4096, KeyHasher::default()).expect("ring");
    let mut flush = TierFlush::new(fs.clone(), flush_config(), 0);
    let hasher = KeyHasher::default();
    let writer = SyncIckWriter::create_v2(
        fs.clone(),
        Path::new(SHARD),
        &CkptConfig::default(),
        0,
        1,
        Lsn::new(SegmentId(1), 64),
        &[NS.0],
    )
    .expect("create ick");
    writer.finish().expect("finish ick");
    let tier = table.tier_manifest(NS.0, &flush);
    write_manifest(
        &fs,
        Path::new(SHARD),
        &Manifest {
            ckpt_id: 1,
            begin_lsn: Lsn::new(SegmentId(1), 64),
            segments: vec![SegmentId(1)],
            tiers: vec![tier],
            key_hash_id: hasher.identity(),
        },
    )
    .expect("manifest");
    let mut tail = Vec::new();
    let mut set = |table: &mut TieredTable, key: &[u8], value: &[u8]| {
        if table.insert(key, value, hasher.hash(key)).is_err() {
            maintain(table, &mut flush);
            table.insert(key, value, hasher.hash(key)).expect("fits after MAINTAIN");
        }
        RecordView::StringPostImage { ns: NS, key, value }.encode_into(&mut tail);
    };
    set(&mut table, b"victim", &[0x44; 300]);
    let window = BUDGET + PAGE;
    let mut written = 0u64;
    let mut i = 0u64;
    while written < 2 * window {
        let key = format!("filler:{i:06}").into_bytes();
        set(&mut table, &key, &[0x22; 900]);
        written += 920;
        i += 1;
    }
    // The DEL of the demoted key: its marker names the crashed life's
    // address, the record resolves by key at replay (ADR-0174 R4, R6).
    let hash = hasher.hash(b"victim");
    let TieredLookup::Cold(addr) = table.lookup(b"victim", hash, &[]) else { panic!("demoted") };
    RecordView::ColdDisplace { ns: NS, old_addr: addr.to_raw() }.encode_into(&mut tail);
    RecordView::Delete { ns: NS, key: b"victim" }.encode_into(&mut tail);
    drop((table, flush));
    (fs, tail)
}

fn boot(fs: &MemFs) -> (TieredTable, TierReplay<MemFs>) {
    let manifest = read_manifest(fs, Path::new(SHARD)).expect("read").expect("present");
    let tier = manifest.tier_ns(NS.0).expect("tier section").clone();
    let RecoveredTier { table, replay, .. } = recover_tiered_ns(
        fs.clone(),
        &tier,
        manifest.ckpt_id,
        flush_config(),
        space_config(0),
        demote(),
        4096,
        KeyHasher::default(),
    )
    .expect("tier recovery");
    let table = std::cell::RefCell::new(table);
    let ick = Path::new(SHARD).join(inf_log::ckpt::ick_file_name(manifest.ckpt_id));
    read_ick_hybrid(
        fs,
        &ick,
        inf_log::ckpt::IckReaderConfig::default(),
        |_| Ok::<(), std::convert::Infallible>(()),
        |section| {
            apply_ref_section(&mut table.borrow_mut(), &section, tier.flushed).expect("refs");
            Ok(())
        },
        |section| {
            apply_live_set_section(&mut table.borrow_mut(), &section);
            Ok(())
        },
        |_| Ok(()),
        |_| panic!("no index-sidecar sections in this image"),
    )
    .expect("hybrid load");
    (table.into_inner(), replay)
}

/// Replays the tail; `on_delete` runs the `DEL` (the row's fault lands
/// there) and reports whether it applied.
fn replay(
    table: &mut TieredTable,
    machine: &mut TierReplay<MemFs>,
    tail: &[u8],
    mut on_delete: impl FnMut(&mut TieredTable, &mut TierReplay<MemFs>, &[LogicalAddr]) -> bool,
) -> bool {
    let hasher = KeyHasher::default();
    let mut rest = tail;
    let mut markers: Vec<LogicalAddr> = Vec::new();
    let mut deleted = false;
    while !rest.is_empty() {
        let (record, consumed) = decode_record(rest).expect("tail decodes");
        rest = &rest[consumed..];
        match record {
            RecordView::StringPostImage { key, value, .. } => {
                let hash = hasher.hash(key);
                table.replay_upsert(Some(&mut *machine), &markers, key, value, hash).expect("fits");
                markers.clear();
            }
            RecordView::ColdDisplace { old_addr, .. } => {
                markers.push(LogicalAddr::from_raw(old_addr).expect("48-bit"));
            }
            RecordView::Delete { key, .. } => {
                assert_eq!(key, b"victim");
                deleted = on_delete(table, machine, &markers);
                markers.clear();
            }
            other => panic!("{other:?}"),
        }
    }
    deleted
}

/// Row: `replay_settle_read_fail` → boot-refuses-typed-then-recovers.
#[test]
fn replay_settle_read_fail_refuses_typed_and_the_next_boot_recovers() {
    let (fs, tail) = crashed_life();
    let hash = KeyHasher::default().hash(b"victim");
    // Boot 1: the DEL's settle read fails — typed, nothing changed; the
    // machine is dropped (the boot's refusal).
    let (mut table, mut machine) = boot(&fs);
    let refused = std::cell::Cell::new(false);
    let deleted = replay(&mut table, &mut machine, &tail, |table, machine, markers| {
        let ro = table.space().ro_boundary();
        let slots = table.len();
        fault::arm("replay_settle_read_fail", FaultSpec::Nth(1));
        let err = table
            .replay_delete(Some(machine), markers, b"victim", hash)
            .expect_err("the injected read failure refuses typed");
        fault::disarm_all();
        assert!(matches!(err, ReplayRefusal::SettleRead { .. }), "{err}");
        assert!(matches!(table.lookup(b"victim", hash, &[]), TieredLookup::Cold(_)), "unchanged");
        assert_eq!(table.len(), slots, "no slot moved");
        assert_eq!(table.space().ro_boundary(), ro, "the boundary stayed");
        refused.set(true);
        false
    });
    assert!(refused.get() && !deleted);
    assert!(machine.counters().demote_steps > 0, "the unit is above the window");
    drop((table, machine));
    // Boot 2, the fault cleared: the same unit recovers and the DEL
    // applies.
    let (mut table, mut machine) = boot(&fs);
    let deleted = replay(&mut table, &mut machine, &tail, |table, machine, markers| {
        table.replay_delete(Some(machine), markers, b"victim", hash).expect("verified")
    });
    assert!(deleted, "the DEL verified and removed the demoted copy");
    assert!(machine.counters().deletes_verified >= 1);
    machine.end_of_replay(&table);
    while machine.settle_step(&mut table, PAGE).expect("settle") == SettleProgress::More {}
    let handed = machine.hand_over(&mut table).expect("hands over").handed;
    assert_eq!(handed.handles.len(), handed.flush.sealed().len());
    assert!(matches!(table.lookup(b"victim", hash, &[]), TieredLookup::Miss));
    receipt::verified("replay_settle_read_fail", "boot-refuses-typed-then-recovers");
}

/// Applies one tail record through the replay entries; markers park
/// until their mutation.
fn apply_one(
    table: &mut TieredTable,
    machine: &mut TierReplay<MemFs>,
    record: RecordView<'_>,
    markers: &mut Vec<LogicalAddr>,
) -> Result<(), ReplayRefusal> {
    let hasher = KeyHasher::default();
    match record {
        RecordView::StringPostImage { key, value, .. } => {
            let hash = hasher.hash(key);
            table.replay_upsert(Some(machine), markers, key, value, hash)?;
        }
        RecordView::ColdDisplace { old_addr, .. } => {
            markers.push(LogicalAddr::from_raw(old_addr).expect("48-bit"));
            return Ok(());
        }
        RecordView::Delete { key, .. } => {
            let hash = hasher.hash(key);
            table.replay_delete(Some(machine), markers, key, hash)?;
        }
        other => panic!("{other:?}"),
    }
    markers.clear();
    Ok(())
}

/// The tier files in the namespace's directory that the manifest does
/// not name, with their lengths.
fn unmanifested(fs: &MemFs) -> Vec<(String, usize)> {
    let manifest = read_manifest(fs, Path::new(SHARD)).expect("read").expect("present");
    let named: Vec<String> = manifest
        .tier_ns(NS.0)
        .expect("section")
        .files
        .iter()
        .map(|f| inf_log::tier_file_name(f.id))
        .collect();
    let cold = Path::new(SHARD).join("cold");
    fs.list_dir(&cold)
        .unwrap_or_default()
        .into_iter()
        .filter(|name| !named.contains(name))
        .map(|name| {
            let len = fs.contents(&cold.join(&name)).map_or(0, |bytes| bytes.len());
            (name, len)
        })
        .collect()
}

/// A later boot with every fault cleared: recovery removes what the
/// manifest does not name, the unit replays whole, the `DEL` applies,
/// and the hand-over returns a handle per sealed file.
fn the_next_boot_recovers(fs: &MemFs, tail: &[u8]) {
    let hash = KeyHasher::default().hash(b"victim");
    let (mut table, mut machine) = boot(fs);
    assert!(unmanifested(fs).is_empty(), "the next boot removed the unmanifested files");
    let mut rest = tail;
    let mut markers = Vec::new();
    let mut keys = std::collections::BTreeSet::new();
    while !rest.is_empty() {
        let (record, consumed) = decode_record(rest).expect("tail decodes");
        rest = &rest[consumed..];
        match record {
            RecordView::StringPostImage { key, .. } => {
                keys.insert(key.to_vec());
            }
            RecordView::Delete { key, .. } => {
                keys.remove(key);
            }
            _ => {}
        }
        apply_one(&mut table, &mut machine, record, &mut markers).expect("replays");
    }
    assert!(machine.counters().demote_steps > 0, "the unit is above the window");
    machine.end_of_replay(&table);
    while machine.settle_step(&mut table, PAGE).expect("settle") == SettleProgress::More {}
    let handed = machine.hand_over(&mut table).expect("hands over").handed;
    assert_eq!(handed.handles.len(), handed.flush.sealed().len());
    assert!(matches!(table.lookup(b"victim", hash, &[]), TieredLookup::Miss));
    assert_eq!(table.len(), keys.len(), "every acknowledged key, and no other");
}

/// One tier fault point fired inside a boot demote step: armed before
/// replay — a replay that fits writes nothing, so the boot's first tier
/// I/O of that kind is a demote step's — then the step refuses typed
/// with `flushed` where the step found it, and the next boot recovers.
fn demote_step_refuses_typed_then_recovers(
    point: &'static str,
    cause: fn(&TierFlushError) -> bool,
) {
    let (fs, tail) = crashed_life();
    let (mut table, mut machine) = boot(&fs);
    fault::arm(point, FaultSpec::Nth(1));
    let mut rest: &[u8] = &tail;
    let mut markers = Vec::new();
    let (refusal, flushed_before) = loop {
        assert!(!rest.is_empty(), "{point}: VACUOUS — no demote step reached the point");
        let (record, consumed) = decode_record(rest).expect("tail decodes");
        rest = &rest[consumed..];
        let flushed = table.space().flushed();
        if let Err(refusal) = apply_one(&mut table, &mut machine, record, &mut markers) {
            break (refusal, flushed);
        }
    };
    let fired = fault::fired(point);
    fault::disarm_all();
    assert_eq!(fired, 1, "{point}: the refusal is the point's");
    assert_eq!(table.space().flushed(), flushed_before, "{point}: flushed unmoved");
    match &refusal {
        ReplayRefusal::Flush { cause: c, unplaced_bytes, handles_held } => {
            assert!(cause(c), "{point}: {c}");
            assert!(*unplaced_bytes > 0, "{point}: the step's sealed bytes are named");
            assert_eq!(*handles_held, machine.sealed().len(), "{point}: the handles held");
        }
        other => panic!("{point}: {other}"),
    }
    assert!(machine.counters().demote_steps > 0, "{point}: inside a demote step");
    for (name, len) in unmanifested(&fs) {
        assert!(point != inf_log::fault::TIER_DIR_OPEN_FAIL || len == 0, "{point}: {name}");
    }
    drop((table, machine));
    the_next_boot_recovers(&fs, &tail);
    receipt::verified(point, "demote-step-refuses-typed-then-recovers");
}

/// Row: `tier_short_write` → demote-step-refuses-typed-then-recovers.
#[test]
fn tier_short_write_in_a_boot_demote_step_refuses_typed_and_the_next_boot_recovers() {
    demote_step_refuses_typed_then_recovers(inf_log::fault::TIER_SHORT_WRITE, |c| {
        matches!(c, TierFlushError::Io { .. }) && !c.is_storage_full()
    });
}

/// Row: `tier_write_nospace` → demote-step-refuses-typed-then-recovers.
#[test]
fn tier_write_nospace_in_a_boot_demote_step_refuses_typed_and_the_next_boot_recovers() {
    demote_step_refuses_typed_then_recovers(inf_log::fault::TIER_WRITE_NOSPACE, |c| {
        c.is_storage_full()
    });
}

/// Row: `tier_fsync_err` → demote-step-refuses-typed-then-recovers (a
/// failed barrier is the boot's fail-stop: `flushed` frozen at the last
/// good barrier).
#[test]
fn tier_fsync_err_in_a_boot_demote_step_refuses_typed_and_the_next_boot_recovers() {
    demote_step_refuses_typed_then_recovers(inf_log::fault::TIER_FSYNC_ERR, |c| {
        matches!(c, TierFlushError::Fsync { .. })
    });
}

/// Row: `tier_footer_torn` → demote-step-refuses-typed-then-recovers (the
/// first seal of the boot is a capacity seal inside a demote step).
#[test]
fn tier_footer_torn_in_a_boot_demote_step_refuses_typed_and_the_next_boot_recovers() {
    demote_step_refuses_typed_then_recovers(inf_log::fault::TIER_FOOTER_TORN, |c| {
        matches!(c, TierFlushError::Io { .. })
    });
}

/// Row: `tier_dir_open_fail` → demote-step-refuses-typed-then-recovers
/// (the first demote step's create is refused: no file is left).
#[test]
fn tier_dir_open_fail_in_a_boot_demote_step_refuses_typed_and_the_next_boot_recovers() {
    demote_step_refuses_typed_then_recovers(inf_log::fault::TIER_DIR_OPEN_FAIL, |c| {
        matches!(c, TierFlushError::Io { .. })
    });
}

/// Row: `tier_torn_frame` → demote-step-cut-then-recovers. The torn write
/// succeeds (the lying-disk physics of the point); the boot is cut right
/// after the record whose step made it, and the next boot removes the
/// boot's file and recovers the unit.
#[test]
fn tier_torn_frame_in_a_boot_demote_step_is_cut_and_the_next_boot_recovers() {
    let point = inf_log::fault::TIER_TORN_FRAME;
    let (fs, tail) = crashed_life();
    let (mut table, mut machine) = boot(&fs);
    fault::arm(point, FaultSpec::Nth(1));
    let mut rest: &[u8] = &tail;
    let mut markers = Vec::new();
    while fault::fired(point) == 0 {
        assert!(!rest.is_empty(), "VACUOUS — no demote step wrote a tier frame");
        let (record, consumed) = decode_record(rest).expect("tail decodes");
        rest = &rest[consumed..];
        apply_one(&mut table, &mut machine, record, &mut markers).expect("a torn write succeeds");
    }
    fault::disarm_all();
    assert!(machine.counters().demote_steps > 0, "the torn write was a demote step's");
    assert!(!unmanifested(&fs).is_empty(), "the cut leaves the boot's file behind");
    drop((table, machine));
    the_next_boot_recovers(&fs, &tail);
    receipt::verified(point, "demote-step-cut-then-recovers");
}
