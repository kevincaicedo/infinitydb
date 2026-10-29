//! M4.5-S31 reactor-drive equivalence storm (ADR-0084): the **same
//! seeded workload** drives two pipelines — the ADR-0056 D3 seam drive
//! on `MemFs` and the reactor drive on `SimDisk` (rounds planned and
//! staged by `stage_flush_round`, executed through the sim driver's
//! write/fsync entry points exactly as the plane would, effects applied by
//! `complete_flush_round`) — and they must flush the same bytes: every
//! logical address both drives flushed holds the same byte in both
//! drives' tier files, every sealed file verifies, and every key resolves
//! to the same value. The drives choose their spans by different rules —
//! the reactor's round is the records' own whole-record prefix within the
//! slice (ADR-0155 D1), the seam's chunks end at recorded seal cuts — so
//! their file boundaries and watermarks between rounds may differ; the
//! bytes may not. Neither may the pace: both drives flush the stream to
//! within the RAM window of its end, the reactor to within a commit page
//! of the seam, so a short stream cannot narrow what is compared. The byte
//! clauses carry their canary in the test itself: one byte flipped in one
//! reactor file must turn both red on every run.
//!
//! Round-scoped invariants asserted every MAINTAIN: no watermark moves
//! between stage and completion (the §3.1 chain — durability facts
//! apply only at the barrier CQE), the claim rule holds after every
//! completion, and writes strictly precede barriers in the op list.

use std::collections::BTreeMap;
use std::path::Path;

use inf_log::fs::mem::MemFs;
use inf_log::fs::sim::SimDisk;
use inf_log::fs::{SegmentFile, SegmentFs};
use inf_log::{NsId, TierDrive, TierFlush, TierFlushConfig, TierIoMode, inspect_tier_bytes};
use inf_store::KeyHasher;
use inf_store::{
    AddressSpaceConfig, DemotionConfig, Keyspace, LogicalAddr, StoreConfig, TieredLookup,
    TieredTable,
};

const NS: NsId = NsId(41);
const PAGE: u64 = 4 << 10;
const BUDGET: u64 = 1 << 20;
/// Small file capacity so the storm rotates files constantly.
const FILE_CAPACITY: u64 = 96 << 10;
const OPS: u64 = 60_000;

fn seeded(x: &mut u64) -> u64 {
    *x ^= *x << 13;
    *x ^= *x >> 7;
    *x ^= *x << 17;
    *x
}

fn keyspace() -> Keyspace {
    let demote = DemotionConfig::for_budget(BUDGET, PAGE);
    let ring = demote.ring_reserve_bytes().expect("valid budget");
    let mut ks = Keyspace::new(StoreConfig::default());
    assert!(
        ks.materialize_tiered(
            NS,
            AddressSpaceConfig {
                reserve_bytes: ring,
                page_bytes: PAGE as usize,
                life_origin: LogicalAddr::ZERO,
            },
            DemotionConfig::for_budget(BUDGET, PAGE),
            2048,
        )
        .is_ok()
    );
    ks
}

fn pipeline<F: SegmentFs>(fs: F) -> TierFlush<F> {
    TierFlush::new(
        fs,
        TierFlushConfig {
            shard_dir: Path::new("shard-0").to_path_buf(),
            cell: 0,
            ns: NS,
            mode: TierIoMode::Buffered,
            file_capacity: FILE_CAPACITY,
            slice_bytes: PAGE,
            record_len_max: 16 << 20,
        },
        0,
        &inf_foundation::bounded::CapCensus::new(inf_foundation::CellId(0)).0,
    )
}

/// Executes the staged round the plane's way: every write, then every
/// barrier (fdatasync covers only completed writes), then the effects.
fn run_round(disk: &SimDisk, table: &mut TieredTable, flush: &mut TierFlush<SimDisk>) {
    let flushed_at_stage = table.space().flushed();
    let writes = flush.round_write_count();
    for index in 0..flush.round_op_count() {
        let op = flush.round_op(index);
        assert_eq!(op.is_barrier, index >= writes, "writes strictly precede barriers");
        if op.is_barrier {
            disk.driver_fdatasync(op.fd).expect("driver barrier");
        } else {
            disk.driver_write_at(op.fd, op.offset, op.bytes).expect("driver write");
        }
    }
    // Nothing moved while the round was in flight (ADR-0084 D2).
    assert_eq!(table.space().flushed(), flushed_at_stage, "no watermark before completion");
    let _ = table.complete_flush_round(flush);
    // Claim rule after every completion (the seam storm's invariant).
    if flush.active().is_some() {
        let limit = flush.confirmable_end().expect("active file has a bound");
        assert!(table.space().flushed().to_raw() <= limit, "flushed within the claim bound");
    }
}

/// One MAINTAIN round on the reactor drive: seal → stage round → run
/// round → release, looping until this round's backlog drains.
fn maintain_reactor(ks: &mut Keyspace, disk: &SimDisk, flush: &mut TierFlush<SimDisk>) {
    loop {
        let d = ks.demote_tick();
        let table = ks.tiered_store_mut(NS).expect("materialized");
        let staged = table.stage_flush_round(flush).expect("stage round");
        let mut round_work = 0u64;
        if flush.round_active() {
            round_work = 1 + staged;
            run_round(disk, table, flush);
        }
        if d.sealed_bytes + d.released_bytes + round_work == 0 {
            break;
        }
    }
}

/// One MAINTAIN round on the seam drive (the storm oracle's shape).
fn maintain_seam(ks: &mut Keyspace, flush: &mut TierFlush<MemFs>) {
    loop {
        let d = ks.demote_tick();
        let table = ks.tiered_store_mut(NS).expect("materialized");
        let f = table.flush_slice(flush).expect("flush slice");
        if d.sealed_bytes + d.released_bytes + f.appended_bytes + u64::from(f.gaps_crossed) == 0 {
            break;
        }
    }
}

/// The logical bytes `[0, upto)` the sealed files `(base, data_len, image)`
/// hold, frame by frame (a data byte `d` of a file lives at frame
/// `d / TIER_FRAME_DATA`, ADR-0056's mapping); a byte no file covers is 0.
fn logical_bytes(files: &[(u64, u64, Vec<u8>)], upto: u64) -> Vec<u8> {
    const D: u64 = inf_log::TIER_FRAME_DATA as u64;
    const V: u64 = inf_log::TIER_FRAME_BYTES as u64;
    const HEADER: u64 = 4096;
    let mut out = vec![0u8; usize::try_from(upto).expect("fits")];
    for (base, data_len, image) in files {
        for d in 0..*data_len {
            let addr = base + d;
            if addr >= upto {
                break;
            }
            let at = HEADER + (d / D) * V + d % D;
            out[usize::try_from(addr).expect("fits")] = image[usize::try_from(at).expect("fits")];
        }
    }
    out
}

fn sim_image(disk: &SimDisk, path: &Path) -> Vec<u8> {
    let file = disk.open_read(path).expect("file exists");
    let size = file.file_size().expect("size") as usize;
    let mut bytes = vec![0u8; size];
    let mut read = 0;
    while read < size {
        let n = file.read_at(read as u64, &mut bytes[read..]).expect("read");
        assert!(n > 0, "no EOF inside the image");
        read += n;
    }
    bytes
}

/// The byte clauses' canary, run on every storm: one data byte flipped in
/// one reactor file below `upto` must turn both clauses red — the logical
/// stream differs at that byte's address, and the file's frame check fails
/// at its frame. An oracle that read the wrong file, the wrong range or
/// no bytes at all would pass the planted image.
fn planted_flip_turns_both_byte_clauses_red(
    seam_stream: &[u8],
    reactor_files: &[(u64, u64, Vec<u8>)],
    upto: u64,
) {
    const D: u64 = inf_log::TIER_FRAME_DATA as u64;
    const V: u64 = inf_log::TIER_FRAME_BYTES as u64;
    const HEADER: u64 = 4096;
    let at_file = reactor_files
        .iter()
        .position(|(base, len, _)| *len > 0 && base + len <= upto)
        .expect("a sealed reactor file below the compared window");
    let mut planted = reactor_files.to_vec();
    let (base, data_len, image) = &mut planted[at_file];
    let d = *data_len / 2;
    let at = usize::try_from(HEADER + (d / D) * V + d % D).expect("fits");
    image[at] ^= 0xFF;
    let summary = inspect_tier_bytes(image).expect("the planted image parses");
    assert_eq!(summary.first_bad_frame, Some(d / D), "the frame check catches the plant");
    let addr = *base + d;
    let stream = logical_bytes(&planted, upto);
    let first = seam_stream.iter().zip(&stream).position(|(a, b)| a != b).map(|i| i as u64);
    assert_eq!(first, Some(addr), "the byte comparison catches the plant at its address");
}

/// The equivalence storm: both drives, one op sequence, one outcome.
#[test]
fn reactor_drive_matches_the_seam_drive_byte_for_byte() {
    let mem = MemFs::new();
    let disk = SimDisk::new();
    let mut seam_ks = keyspace();
    let mut reactor_ks = keyspace();
    let mut seam_flush = pipeline(mem.clone());
    let mut reactor_flush = pipeline(disk.clone());
    reactor_flush.set_drive(TierDrive::Reactor);

    let mut seed = 0x54EE_DF1Cu64;
    let keys = 900u64;
    let mut model: BTreeMap<Vec<u8>, (Vec<u8>, u32, usize)> = BTreeMap::new();
    let mut ops = 0u64;
    while ops < OPS {
        for _ in 0..64 {
            ops += 1;
            let idx = seeded(&mut seed) % keys;
            let key = format!("flush:{idx:05}").into_bytes();
            let value =
                vec![(seeded(&mut seed) % 251) as u8; 40 + (seeded(&mut seed) % 200) as usize];
            let hash = KeyHasher::default().hash(&key);
            let mut placed_version = 0u32;
            let mut placed_len = 0usize;
            for ks in [&mut seam_ks, &mut reactor_ks] {
                let table = ks.tiered_store_mut(NS).expect("materialized");
                let placed = match table.lookup(&key, hash, &[]) {
                    TieredLookup::Ram(old) | TieredLookup::Cold(old) => {
                        let (_, old_version, old_len) = model.get(&key).expect("model has it");
                        table
                            .update(&key, &value, hash, old, *old_len, *old_version)
                            .expect("paced storm fits the window")
                    }
                    TieredLookup::Miss => {
                        table.insert(&key, &value, hash).expect("paced storm fits the window")
                    }
                };
                let parts = table.record(placed);
                placed_version = parts.version;
                placed_len = parts.encoded_len;
            }
            model.insert(key, (value, placed_version, placed_len));
        }
        maintain_seam(&mut seam_ks, &mut seam_flush);
        maintain_reactor(&mut reactor_ks, &disk, &mut reactor_flush);
    }

    // Both catalogs rotated, and every sealed file of either drive verifies.
    assert!(seam_flush.sealed().len() > 3, "capacity rotation ({})", seam_flush.sealed().len());
    assert!(
        reactor_flush.sealed().len() > 3,
        "capacity rotation ({})",
        reactor_flush.sealed().len()
    );
    let mut seam_files = Vec::new();
    for meta in seam_flush.sealed() {
        seam_files.push((
            meta.base.to_raw(),
            meta.data_len,
            mem.contents(&meta.path).expect("seam"),
        ));
    }
    let mut reactor_files = Vec::new();
    for meta in reactor_flush.sealed() {
        let image = sim_image(&disk, &meta.path);
        let summary = inspect_tier_bytes(&image).expect("sealed image parses");
        assert_eq!(summary.sealed.expect("sealed").data_len, meta.data_len);
        assert_eq!(summary.first_bad_frame, None, "every frame verifies");
        reactor_files.push((meta.base.to_raw(), meta.data_len, image));
    }
    // The bytes: below both drives' watermarks, one logical stream.
    let seam_flushed = seam_ks.tiered_store_mut(NS).expect("t").space().flushed().to_raw();
    let reactor_table = reactor_ks.tiered_store_mut(NS).expect("t");
    let reactor_flushed = reactor_table.space().flushed().to_raw();
    // One workload, one allocation order: both tables share this tail.
    let tail = reactor_table.space().tail().to_raw();
    let covered =
        |files: &[(u64, u64, Vec<u8>)]| files.iter().map(|f| f.0 + f.1).max().unwrap_or(0);
    // The comparison's liveness: a short stream cannot narrow the window the
    // bytes are compared over. Each drive drains every MAINTAIN, so each
    // flushed all of the stream but the RAM window, and sealed all of what
    // it flushed but its active file; and the reactor reaches the seam's
    // watermark within a commit page (the seam's chunks end at seal cuts,
    // one a page, the reactor's rounds at the ro-boundary itself). A drive
    // that stalled or lost spans fails here instead of shrinking `upto`.
    assert!(
        reactor_flushed + PAGE >= seam_flushed,
        "the reactor flushed {reactor_flushed} B against the seam's {seam_flushed} B"
    );
    for (drive, flushed, files) in
        [("seam", seam_flushed, &seam_files), ("reactor", reactor_flushed, &reactor_files)]
    {
        assert!(flushed + BUDGET + PAGE >= tail, "{drive} flushed {flushed} B of {tail} B");
        let sealed = covered(files);
        assert!(
            sealed + FILE_CAPACITY + PAGE >= flushed,
            "{drive} sealed {sealed} B below its {flushed} B watermark"
        );
    }
    let upto =
        seam_flushed.min(reactor_flushed).min(covered(&seam_files)).min(covered(&reactor_files));
    assert!(upto > 1 << 20, "the drives flushed a common prefix ({upto} B)");
    let seam_stream = logical_bytes(&seam_files, upto);
    assert!(
        seam_stream == logical_bytes(&reactor_files, upto),
        "the drives' tier files hold the same bytes below {upto}"
    );
    planted_flip_turns_both_byte_clauses_red(&seam_stream, &reactor_files, upto);

    // Both tables place every key at the same address (one workload, one
    // allocation order); a RAM record holds the model's value; a record one
    // drive has released and the other has not is the same address on
    // either side of its watermark.
    let mut cold = 0u64;
    for (key, (want_value, _, _)) in &model {
        let hash = KeyHasher::default().hash(key);
        let seam_table = seam_ks.tiered_store_mut(NS).expect("t");
        let seam_hit = seam_table.lookup(key, hash, &[]);
        let reactor_table = reactor_ks.tiered_store_mut(NS).expect("t");
        let address = |hit: TieredLookup| match hit {
            TieredLookup::Ram(a) | TieredLookup::Cold(a) => a,
            TieredLookup::Miss => panic!("the model's key {key:?} is missing"),
        };
        let reactor_hit = reactor_table.lookup(key, hash, &[]);
        assert_eq!(address(seam_hit), address(reactor_hit), "same address for {key:?}");
        match reactor_hit {
            TieredLookup::Ram(b) => assert_eq!(reactor_table.record(b).value, &want_value[..]),
            TieredLookup::Cold(_) => cold += 1,
            TieredLookup::Miss => {}
        }
    }
    assert!(cold > 50, "the storm demoted a real cold set ({cold})");
}

/// ADR-0178 D3: the tier round's work gate is the stage's own test.
/// `flush_pending` before a stage is true exactly when that stage opens a
/// round, and false exactly when it stages nothing — so the cell's plan
/// stages a round only when a chunk (records or a ring-top gap) is there
/// to take. A seeded storm drives both answers many times,
/// across file rotations and ring wraps.
#[test]
fn flush_pending_is_true_exactly_when_the_stage_opens_a_round() {
    let disk = SimDisk::new();
    let mut ks = keyspace();
    let mut flush = pipeline(disk.clone());
    flush.set_drive(TierDrive::Reactor);
    let mut seed = 0x6A7E_F10Du64;
    let (mut idle_stages, mut work_stages) = (0u32, 0u32);
    let mut placed: BTreeMap<Vec<u8>, (u32, usize)> = BTreeMap::new();
    for _ in 0..400 {
        for _ in 0..64 {
            let idx = seeded(&mut seed) % 900;
            let key = format!("gate:{idx:05}").into_bytes();
            let value = vec![7u8; 40 + (seeded(&mut seed) % 200) as usize];
            let hash = KeyHasher::default().hash(&key);
            let table = ks.tiered_store_mut(NS).expect("materialized");
            let at = match table.lookup(&key, hash, &[]) {
                TieredLookup::Ram(old) | TieredLookup::Cold(old) => {
                    let (old_version, old_len) = placed[&key];
                    table
                        .update(&key, &value, hash, old, old_len, old_version)
                        .expect("paced storm fits the window")
                }
                TieredLookup::Miss => {
                    table.insert(&key, &value, hash).expect("paced storm fits the window")
                }
            };
            let parts = table.record(at);
            placed.insert(key, (parts.version, parts.encoded_len));
        }
        loop {
            let d = ks.demote_tick();
            let table = ks.tiered_store_mut(NS).expect("materialized");
            let pending = table.flush_pending(&flush);
            let staged = table.stage_flush_round(&mut flush).expect("stage round");
            assert_eq!(pending, flush.round_active(), "the gate is the stage's own work test");
            if pending {
                work_stages += 1;
                run_round(&disk, table, &mut flush);
            } else {
                idle_stages += 1;
                assert_eq!(staged, 0, "an idle stage stages nothing");
            }
            if d.sealed_bytes + d.released_bytes + u64::from(pending) == 0 {
                break;
            }
        }
    }
    assert!(work_stages > 100, "the storm staged rounds ({work_stages})");
    assert!(idle_stages > 100, "the storm reached idle stages ({idle_stages})");
    assert!(flush.sealed().len() > 3, "file rotation happened ({})", flush.sealed().len());
}
