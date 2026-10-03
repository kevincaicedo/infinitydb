//! The recovery driver stepping a demoting boot, at the server tier
//! (ADR-0174 D1, D2 rule 4, D6): `Recovery::step` under a byte budget
//! over a tiered tail of several RAM windows, whose last window rewrites
//! and deletes keys the boot has demoted by then — so the boot demotes,
//! settles at the seal and at the end of replay (E10, E12) and verifies
//! its deletes against demoted copies (E5). Every row reads every key of
//! the model back after the boot: RAM through the record, cold through
//! the tier file's CRC-verified frames.

use std::collections::BTreeMap;
use std::path::PathBuf;

use inf_log::fs::mem::MemFs;
use inf_log::fs::{SegmentFile, SegmentFs};
use inf_log::tier::{parse_tier_file_name, probe_tier_file};
use inf_log::{
    MutationEffect, NsId, SegmentConfig, SegmentRotor, StagingRing, TIER_FRAME_BYTES,
    create_cell_dirs, tier_extract, tier_frame_offset, tier_frame_span,
};
use inf_server::{DurableConfig, Recovery, RecoveryProgress};
use inf_store::{FsyncClass, Keyspace, NsMode, NsSpec, TierSpec, TieredLookup, TieredTable};

mod support;
use support::*;

const TIER_NS: NsId = NsId(17);
/// The smallest legal window: `MEM-BUDGET 3mb` + `MAINTAIN-SLICE 1mb`.
const MEM_BUDGET: u64 = 3 << 20;
const WINDOW: u64 = MEM_BUDGET + (1 << 20);
const VALUE_LEN: usize = 4 << 10;
const RECORDS_PER_FRAME: usize = 12;
/// One segment holds the whole unit, so a replay step ends at its budget,
/// never at a segment's end.
const SEGMENT_BYTES: u32 = 32 << 20;
/// The largest log frame the unit writes (asserted as each is staged):
/// twelve records of a 4 KiB value and their framing.
const FRAME_BYTES_MAX: u64 = 64 << 10;

fn step_cfg() -> DurableConfig {
    cfg_with(SegmentConfig { segment_bytes: SEGMENT_BYTES, ..Default::default() })
}

/// `fresh_keyspace` plus a materialized tiered namespace at the smallest
/// window (no MANIFEST — the first-crash shape).
fn tiered_keyspace() -> Keyspace {
    let mut ks = fresh_keyspace();
    ks.ns_create(NsSpec {
        id: TIER_NS,
        name: b"hot".to_vec(),
        mode: NsMode::Durable,
        fsync: Some(FsyncClass::Always),
        policy: None,
        maxmemory: None,
        tier: Some(TierSpec::for_budget(MEM_BUDGET)),
    })
    .expect("tiered ns");
    ks
}

fn key(i: u64) -> Vec<u8> {
    format!("u:{i:06}").into_bytes()
}

fn value(i: u64, generation: u8) -> Vec<u8> {
    let mut v = vec![(i % 251) as u8 ^ generation; VALUE_LEN];
    v[..8].copy_from_slice(&i.to_le_bytes());
    v[8] = generation;
    v
}

/// The acknowledged state: every key the unit wrote, `None` once deleted.
type Model = BTreeMap<Vec<u8>, Option<Vec<u8>>>;

/// The recovery unit as written: the model, and the records of its
/// distinct-key prefix and of its last window's rewrites and deletes.
struct Unit {
    model: Model,
    distinct_records: u64,
    late_records: u64,
}

/// A cell log written through the real rotor and staging ring on any
/// filesystem, one frame per call (the shape of `support::LogBuilder`).
struct Log<F: SegmentFs> {
    rotor: SegmentRotor<F>,
    ring: StagingRing,
}

impl<F: SegmentFs + Clone> Log<F> {
    fn new(fs: &F, cfg: &DurableConfig) -> Log<F> {
        let dirs = create_cell_dirs(fs, &cfg.data_dir.join(format!("shard-{CELL}"))).expect("dirs");
        let rotor = SegmentRotor::create_fresh(fs.clone(), dirs.log, cfg.segment).expect("rotor");
        Log { rotor, ring: StagingRing::new(cfg.staging) }
    }

    fn frame(&mut self, records: &[MutationEffect<'_>]) {
        for effect in records {
            self.ring.stage(effect).expect("stage");
        }
        self.rotor.maintain(0).expect("maintain");
        let frame_len = self.ring.pending_frame_len();
        assert!(u64::from(frame_len) <= FRAME_BYTES_MAX, "a {frame_len}-byte frame");
        let slot = self.rotor.begin_frame(frame_len, 0).expect("reserve");
        let covered = slot.base().to_u64();
        let lease = self.ring.seal(slot.first_record_lsn(), covered, slot.layout());
        let frame = self.ring.leased_frame(&lease).to_vec();
        self.rotor.commit_frame(slot, &frame).expect("commit");
        self.ring.release(lease);
    }
}

/// Writes the recovery unit and makes it durable: distinct keys until
/// the records fill `windows` RAM windows, then — inside the last window —
/// a rewrite of every 7th of the first 420 keys and a delete of every
/// 11th of the rest of them. Their first copies are demoted by then, so a
/// rewrite is a record with a cold twin (settled at its seal, E10, or at
/// the end of replay, E12) and a delete verifies a demoted copy (E5).
fn write_unit<F: SegmentFs + Clone>(fs: &F, cfg: &DurableConfig, windows: u64) -> Unit {
    let mut log = Log::new(fs, cfg);
    let mut model = Model::new();
    let mut bytes = 0u64;
    let mut next = 0u64;
    while bytes < windows * WINDOW {
        let batch: Vec<(Vec<u8>, Vec<u8>)> =
            (next..next + RECORDS_PER_FRAME as u64).map(|i| (key(i), value(i, 0))).collect();
        let effects: Vec<MutationEffect<'_>> = batch
            .iter()
            .map(|(key, value)| MutationEffect::StringSet { ns: TIER_NS, key, value })
            .collect();
        log.frame(&effects);
        for (key, value) in batch {
            bytes += (TieredTable::RECORD_HEADER_LEN + key.len() + value.len()) as u64;
            model.insert(key, Some(value));
        }
        next += RECORDS_PER_FRAME as u64;
    }
    let late: Vec<(Vec<u8>, Option<Vec<u8>>)> = (0..420u64)
        .filter_map(|i| match (i % 7, i % 11) {
            (0, _) => Some((key(i), Some(value(i, 1)))),
            (_, 0) => Some((key(i), None)),
            _ => None,
        })
        .collect();
    for chunk in late.chunks(RECORDS_PER_FRAME) {
        let effects: Vec<MutationEffect<'_>> = chunk
            .iter()
            .map(|(key, value)| match value {
                Some(value) => MutationEffect::StringSet { ns: TIER_NS, key, value },
                None => MutationEffect::Delete { ns: TIER_NS, key },
            })
            .collect();
        log.frame(&effects);
    }
    let late_records = late.len() as u64;
    for (key, value) in late {
        model.insert(key, value);
    }
    drop(log);
    make_durable(fs, cfg);
    Unit { model, distinct_records: next, late_records }
}

/// Every log file's bytes and name under a barrier (the sim disk tears
/// whatever is not).
fn make_durable<F: SegmentFs>(fs: &F, cfg: &DurableConfig) {
    let log_dir = cfg.data_dir.join(format!("shard-{CELL}")).join("log");
    for name in fs.list_dir(&log_dir).expect("log dir") {
        fs.open_write(&log_dir.join(name)).expect("open").sync_data().expect("sync");
    }
    fs.sync_dir(&log_dir).expect("sync log dir");
}

fn cold_dir(cfg: &DurableConfig) -> PathBuf {
    cfg.data_dir.join(format!("shard-{CELL}")).join(format!("ns-{}", TIER_NS.0)).join("cold")
}

/// The namespace's tier files: (base, readable bytes, path), each probed
/// by its header identity and footer — every boot file is sealed by the
/// hand-over.
fn tier_files<F: SegmentFs>(fs: &F, cfg: &DurableConfig) -> Vec<(u64, u64, PathBuf)> {
    let cold = cold_dir(cfg);
    let mut files = Vec::new();
    for name in fs.list_dir(&cold).unwrap_or_default() {
        if parse_tier_file_name(&name).is_none() {
            continue;
        }
        let path = cold.join(&name);
        let (header, footer) = probe_tier_file(fs, &path).expect("probe");
        let footer = footer.expect("the hand-over sealed every boot file");
        files.push((header.identity.base.to_raw(), footer.data_len, path));
    }
    files
}

/// The cold-read path's decode of one record: the frames covering the
/// address, CRC-verified by `tier_extract`, the header sizing the record,
/// then the record itself; the key must be the key asked for.
fn read_cold_value<F: SegmentFs>(
    fs: &F,
    files: &[(u64, u64, PathBuf)],
    addr: u64,
    key: &[u8],
) -> Vec<u8> {
    let (base, len, path) = files
        .iter()
        .find(|(base, len, _)| addr >= *base && addr < base + len)
        .expect("a catalogued file holds the cold address");
    let read = |at: u64, want: usize| -> Vec<u8> {
        assert!(at + want as u64 <= base + len, "the record lies whole in its file");
        let (first, count, skip) = tier_frame_span(at - base, want);
        let mut window = vec![0u8; count as usize * TIER_FRAME_BYTES];
        let file = fs.open_read(path).expect("open");
        let mut done = 0usize;
        while done < window.len() {
            let n = file.read_at(tier_frame_offset(first) + done as u64, &mut window[done..]);
            let n = n.expect("read");
            assert!(n > 0, "short tier file");
            done += n;
        }
        let mut out = Vec::new();
        tier_extract(&window, skip, want, &mut out).expect("frame CRCs");
        out
    };
    let head = read(addr, TieredTable::RECORD_HEADER_LEN);
    let record = read(addr, TieredTable::record_len_from_header(&head));
    let parts = TieredTable::decode_record(&record);
    assert_eq!(parts.key, key, "the cold record carries the key asked for");
    parts.value.to_vec()
}

/// What the recovered keyspace serves for every key of the model, and
/// how many answered from RAM and from a tier file.
fn read_back<F: SegmentFs>(
    fs: &F,
    cfg: &DurableConfig,
    ks: &mut Keyspace,
    model: &Model,
) -> (Model, u64, u64) {
    let files = tier_files(fs, cfg);
    let (mut ram, mut cold) = (0u64, 0u64);
    let mut served = Model::new();
    for key in model.keys() {
        let hash = ks.hasher().hash(key);
        let table = ks.tiered_store_mut(TIER_NS).expect("tiered");
        let got = match table.lookup(key, hash, &[]) {
            TieredLookup::Ram(addr) => {
                ram += 1;
                Some(table.record(addr).value.to_vec())
            }
            TieredLookup::Cold(addr) => {
                cold += 1;
                Some(read_cold_value(fs, &files, addr.to_raw(), key))
            }
            TieredLookup::Miss => None,
        };
        served.insert(key.clone(), got);
    }
    (served, ram, cold)
}

/// Asserts the served state equals the model, naming the first key that
/// differs.
fn assert_model(served: &Model, model: &Model, when: &str) {
    for (key, want) in model {
        let got = served.get(key).cloned().flatten();
        assert!(
            got.as_ref() == want.as_ref(),
            "{when}: key {:?} serves {} where the model holds {}",
            String::from_utf8_lossy(key),
            got.as_ref().map_or("nothing".to_owned(), |v| format!("{} bytes", v.len())),
            want.as_ref().map_or("nothing".to_owned(), |v| format!("{} bytes", v.len())),
        );
    }
}

/// A zero step budget still completes the boot: every step makes
/// progress — a frame, a section, or a settled record — so the end
/// settle never yields before its first record (L6: no step requeues
/// unchanged work). Red before the settle phase's progress rule: the
/// `Settle` phase yielded at a budget it had not spent, every step.
#[test]
fn a_zero_step_budget_completes_a_demoting_boot() {
    let fs = MemFs::new();
    let cfg = step_cfg();
    let model = write_unit(&fs, &cfg, 3).model;
    let mut ks = tiered_keyspace();
    let mut recovery = Recovery::new(fs.clone(), CELL, &cfg, anchor(), now());
    // Bound: each step applies at least one frame or settles at least one
    // record — fewer than four steps per model key — plus the start,
    // audit and finish steps.
    let cap = model.len() as u64 * 4 + 64;
    let mut steps = 0u64;
    while recovery.step(&mut ks, 0).expect("a step") == RecoveryProgress::Working {
        steps += 1;
        assert!(
            steps <= cap,
            "no progress: {steps} steps at a zero budget, the next in phase {:?}",
            recovery.phase()
        );
    }
    let (_rotor, stats, _seed) = recovery.finish();
    let replay = stats.tier_replay.counters;
    assert!(replay.demote_steps > 0, "VACUOUS: the boot did not demote ({replay:?})");
    let (served, ram, cold) = read_back(&fs, &cfg, &mut ks, &model);
    assert!(ram > 0 && cold > 0, "VACUOUS: {ram} RAM and {cold} cold keys");
    assert_model(&served, &model, "after a zero-budget boot");
}

/// Bytes the namespace's tier files hold on disk — the boot's tier
/// writes, seen from the filesystem rather than from its counters.
fn tier_bytes_on_disk<F: SegmentFs>(fs: &F, cfg: &DurableConfig) -> u64 {
    let cold = cold_dir(cfg);
    fs.list_dir(&cold)
        .unwrap_or_default()
        .iter()
        .filter(|name| parse_tier_file_name(name).is_some())
        .map(|name| fs.open_read(&cold.join(name)).expect("open").file_size().expect("size"))
        .sum()
}

/// The step budget's charge for boot I/O (ADR-0174 D2 rule 4), judged
/// step by step at two budgets over a unit of four windows.
/// The prices are the server's and the store's `limits` consts; the
/// charge each step took is `Recovery::step_charge_bytes`, and the tier
/// writes are read from the filesystem:
///
/// - a step during which the tier files grew charged at least the one
///   barrier every flush that appends makes;
/// - a replay step still inside its segment yielded at its budget: its
///   bytes read plus its charge reach it;
/// - and passed it by at most one frame's non-yielding unit — the frame's
///   bytes, one demote step's tier bytes (a lead and a page past the
///   need, plus a record), two barriers (the flush, one seal), and the
///   settle reads the unit's rewrites and deletes can make;
/// - an end-settle step that yields charged its budget, by at most one
///   record and its twins' reads; the last adds the hand-over's drain;
/// - the D6 gauge equals the largest step's charge.
///
/// Red on a driver that leaves the charge out of the yield test (at the
/// 8 MiB budget a step's bytes span several demote steps), and on one that
/// charges nothing (a step that wrote tier bytes charged no barrier).
#[test]
fn every_step_yields_at_the_first_boundary_where_its_reads_and_charge_reach_the_budget() {
    for budget in [256 << 10, 8 << 20] {
        step_under_budget(budget);
    }
}

fn step_under_budget(budget: u64) {
    use inf_server::RecoverPhase;
    let barrier = inf_server::limits::REPLAY_BARRIER_CHARGE_BYTES;
    let read_price = inf_store::limits::SETTLE_READ_CHARGE_BYTES;
    let page = inf_alloc::REGION_PAGE_BYTES as u64;
    let lead = 1u64 << 20; // `MAINTAIN-SLICE` at this budget, a whole page
    let record = (TieredTable::RECORD_HEADER_LEN + key(0).len() + VALUE_LEN) as u64;
    let fs = MemFs::new();
    let cfg = step_cfg();
    let unit = write_unit(&fs, &cfg, 4);
    // One frame's non-yielding unit before the last window's records
    // apply (no record has a cold twin yet), and after.
    let frame_unit = FRAME_BYTES_MAX + lead + page + record + 2 * barrier;
    let late_unit = frame_unit + unit.late_records * read_price;
    let settle_unit = record + 4 * read_price;
    let drain_unit = WINDOW + lead + 3 * barrier;

    let mut ks = tiered_keyspace();
    let mut recovery = Recovery::new(fs.clone(), CELL, &cfg, anchor(), now());
    let (mut largest, mut settle_steps, mut finished) = (0u64, 0u64, false);
    loop {
        let phase = recovery.phase();
        let segments_before = recovery.segments_progress().0;
        let consumed_before = recovery.bytes_consumed();
        let tier_before = tier_bytes_on_disk(&fs, &cfg);
        let progress = recovery.step(&mut ks, budget).expect("a step");
        if progress == RecoveryProgress::Complete {
            break;
        }
        let read = recovery.bytes_consumed() - consumed_before;
        let charge = recovery.step_charge_bytes();
        let applied = recovery.stats().records_applied;
        largest = largest.max(charge);
        let at = format!("budget {budget}, a {phase:?} step, {applied} records applied");
        if tier_bytes_on_disk(&fs, &cfg) > tier_before {
            assert!(charge >= barrier, "{at}: tier files grew under a charge of {charge}");
        }
        match phase {
            RecoverPhase::Replay => {
                let unit_bytes =
                    if applied <= unit.distinct_records { frame_unit } else { late_unit };
                if recovery.phase() == RecoverPhase::Replay
                    && recovery.segments_progress().0 == segments_before
                {
                    assert!(read + charge >= budget, "{at}: yielded at {read} read + {charge}");
                }
                assert!(
                    read + charge < budget + unit_bytes,
                    "{at}: {read} read + {charge} charged passes the budget by more than one \
                     frame's unit ({unit_bytes})"
                );
            }
            RecoverPhase::Finish if !finished => finished = true, // the lift decision
            RecoverPhase::Finish => {
                settle_steps += 1;
                if recovery.phase() == RecoverPhase::Finish {
                    assert!(charge >= budget, "{at}: the settle yielded at a charge of {charge}");
                    assert!(charge < budget + settle_unit, "{at}: a settle step charged {charge}");
                } else {
                    let bound = budget + settle_unit + drain_unit;
                    assert!(charge < bound, "{at}: the last settle step charged {charge}");
                }
            }
            RecoverPhase::Start
            | RecoverPhase::Ckpt
            | RecoverPhase::Audit
            | RecoverPhase::Complete => {
                assert_eq!(charge, 0, "{at}: no boot I/O outside replay and the settle");
            }
        }
    }
    let (_rotor, stats, _seed) = recovery.finish();
    assert_eq!(stats.tier_replay.step_charge_bytes_max, largest, "the gauge is the largest step");
    let replay = stats.tier_replay.counters;
    eprintln!(
        "budget {budget}: gauge {largest} bytes, {settle_steps} settle steps, {} demote steps, \
         {} settle reads, {} deletes verified",
        replay.demote_steps, replay.settle_reads, replay.deletes_verified
    );
    assert!(replay.demote_steps > 1, "VACUOUS: {replay:?}");
    assert!(replay.settle_reads > 0, "VACUOUS: no settle read ({replay:?})");
    assert!(replay.deletes_verified > 0, "VACUOUS: no delete verified ({replay:?})");
    if budget < WINDOW {
        assert!(settle_steps > 1, "VACUOUS: the end settle took {settle_steps} step(s)");
    }
    let (served, ram, cold) = read_back(&fs, &cfg, &mut ks, &unit.model);
    assert!(ram > 0 && cold > 0, "VACUOUS: {ram} RAM and {cold} cold keys");
    assert_model(&served, &unit.model, &format!("after a boot at a {budget}-byte budget"));
}

/// Where a power cut lands in a demoting boot (ADR-0174 D5).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Cut {
    /// Mid-replay, right after the first demote step made a boot tier file.
    AfterFirstDemote,
    /// Inside the end settle with records unsettled: a settle step yielded.
    InsideSettle,
    /// After E13's hand-over, before the cell serves: the boot completed.
    AfterHandOver,
}

/// What a boot left that a later boot must reproduce exactly: the tiered
/// table's digest (every slot's address and hash, every RAM record's
/// bytes), the namespace's tier files by name and bytes, and what every
/// key of the model serves.
#[derive(Debug, PartialEq, Eq)]
struct BootState {
    table: inf_store::StateDigest,
    files: Vec<(String, Vec<u8>)>,
    served: Model,
}

/// Boots the cell on `disk` under `budget` until `cut` lands, asserting
/// it landed there, and drops every handle — the process dies; the
/// caller cuts the power.
fn boot_cut(disk: &inf_server::SimDisk, cfg: &DurableConfig, budget: u64, cut: Cut) {
    use inf_server::RecoverPhase;
    let mut ks = tiered_keyspace();
    let mut recovery = Recovery::new(disk.clone(), CELL, cfg, anchor(), now());
    let mut finish_steps = 0u64;
    let cold = cold_dir(cfg);
    // Bound: the boot's own steps — every step progresses (above).
    for _ in 0..1_000_000u64 {
        let phase = recovery.phase();
        let progress = recovery.step(&mut ks, budget).expect("a cut boot's step");
        let landed = match cut {
            Cut::AfterFirstDemote => {
                let files = disk.list_dir(&cold).unwrap_or_default();
                phase == RecoverPhase::Replay
                    && files.iter().any(|name| parse_tier_file_name(name).is_some())
                    && recovery.phase() == RecoverPhase::Replay
            }
            Cut::InsideSettle => {
                finish_steps += u64::from(phase == RecoverPhase::Finish);
                finish_steps >= 3 && recovery.phase() == RecoverPhase::Finish
            }
            Cut::AfterHandOver => progress == RecoveryProgress::Complete,
        };
        if landed {
            return;
        }
        assert!(
            progress == RecoveryProgress::Working,
            "VACUOUS: the boot completed before the cut {cut:?} landed"
        );
    }
    panic!("the boot never reached the cut {cut:?}");
}

/// Boots the cell on `disk` to the end under `budget` and reads its state.
fn boot_whole(
    disk: &inf_server::SimDisk,
    cfg: &DurableConfig,
    budget: u64,
    model: &Model,
) -> std::io::Result<BootState> {
    let mut ks = tiered_keyspace();
    let mut recovery = Recovery::new(disk.clone(), CELL, cfg, anchor(), now());
    while recovery.step(&mut ks, budget)? == RecoveryProgress::Working {}
    let (_rotor, stats, _seed) = recovery.finish();
    assert!(stats.tier_replay.counters.demote_steps > 0, "VACUOUS: the boot did not demote");
    let table = ks.tiered_store(TIER_NS).expect("tiered").simulation_digest();
    let cold = cold_dir(cfg);
    let mut names = disk.list_dir(&cold).expect("cold dir");
    names.sort();
    let files = names
        .into_iter()
        .map(|name| {
            let bytes = disk.contents(&cold.join(&name)).expect("a listed file");
            (name, bytes)
        })
        .collect();
    let (served, _, _) = read_back(disk, cfg, &mut ks, model);
    Ok(BootState { table, files, served })
}

/// A demoting boot cut by a power loss — after its first demote step,
/// inside its end settle, or after its hand-over before the cell serves —
/// once, or twice in a row, leaves unmanifested tier files the next boot
/// removes before any flush (ADR-0174 D4, D5); the boot that completes
/// reproduces the uncut boot of the same unit exactly: the table's digest,
/// the tier files byte for byte, and every key of the model. Each cut
/// asserts it landed where it says (`VACUOUS` otherwise), on the
/// simulated disk, which tears what no barrier covered.
#[test]
fn a_demoting_boot_cut_at_each_point_once_or_twice_recovers_the_uncut_boot() {
    const BUDGET: u64 = 256 << 10;
    let cfg = step_cfg();
    let disk = inf_server::SimDisk::new();
    let unit = write_unit(&disk, &cfg, 3);
    let uncut = boot_whole(&disk, &cfg, BUDGET, &unit.model).expect("the uncut boot");
    assert_model(&uncut.served, &unit.model, "the uncut boot");
    assert!(!uncut.files.is_empty(), "VACUOUS: the uncut boot sealed no tier file");
    let rows: [&[Cut]; 6] = [
        &[Cut::AfterFirstDemote],
        &[Cut::InsideSettle],
        &[Cut::AfterHandOver],
        &[Cut::AfterFirstDemote, Cut::InsideSettle],
        &[Cut::InsideSettle, Cut::AfterHandOver],
        &[Cut::AfterHandOver, Cut::AfterFirstDemote],
    ];
    for (row, cuts) in rows.iter().enumerate() {
        let disk = inf_server::SimDisk::new();
        let unit = write_unit(&disk, &cfg, 3);
        for (i, &cut) in cuts.iter().enumerate() {
            boot_cut(&disk, &cfg, BUDGET, cut);
            disk.power_cut(0xC07_5EED ^ ((row as u64) << 8) ^ i as u64);
        }
        let state = match boot_whole(&disk, &cfg, BUDGET, &unit.model) {
            Ok(state) => state,
            Err(err) => panic!("the cuts {cuts:?}: the next boot refused: {err}"),
        };
        assert_model(&state.served, &unit.model, &format!("after the cuts {cuts:?}"));
        assert!(
            state.table == uncut.table,
            "the cuts {cuts:?}: the table differs from the uncut boot's"
        );
        assert!(
            state.files == uncut.files,
            "the cuts {cuts:?}: the tier files differ from the uncut boot's"
        );
    }
}
