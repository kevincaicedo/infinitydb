//! FCR-STTIER-01 boot-replay fault row (ADR-0174 D3; DRR FCR-STTIER-01
//! §2) — carried at the node tier per `m4.toml`:
//!
//! - `boot-refuses-typed-then-recovers` — a boot settle read that fails
//!   (`replay_settle_read_fail`) is the typed refusal: a `DEL` whose read
//!   failed has changed nothing (the key still resolves, no slot moved,
//!   the boundary stayed), the boot stops typed, and a later boot with
//!   the fault cleared recovers the same unit and applies the `DEL`.

#[path = "../receipt.rs"]
mod receipt;

use std::path::Path;

use inf_foundation::fault::{self, FaultSpec};
use inf_log::fs::SegmentFs;
use inf_log::fs::mem::MemFs;
use inf_log::{
    CkptConfig, Lsn, Manifest, NsId, RecordView, SegmentId, SyncIckWriter, TierFlush,
    TierFlushConfig, TierIoMode, decode_record, read_ick_hybrid, read_manifest, write_manifest,
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
