//! The recovery driver stepping a demoting boot, at the server tier
//! (ADR-0174 D1, R10, D6): `Recovery::step` under a byte budget
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

/// The recovery unit as written: the model.
struct Unit {
    model: Model,
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
        let rotor = SegmentRotor::create_fresh(fs.clone(), dirs.log, cfg.geometry).expect("rotor");
        Log { rotor, ring: StagingRing::new(cfg.geometry) }
    }

    fn frame(&mut self, records: &[MutationEffect<'_>]) {
        for effect in records {
            self.ring.stage(effect).expect("stage");
        }
        let frame_len = self.ring.pending_frame_len();
        assert!(u64::from(frame_len) <= FRAME_BYTES_MAX, "a {frame_len}-byte frame");
        self.seal_frame();
    }

    /// Seals and commits the records staged so far as one frame.
    fn seal_frame(&mut self) {
        self.rotor.maintain(0).expect("maintain");
        let frame_len = self.ring.pending_frame_len();
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
    for (key, value) in late {
        model.insert(key, value);
    }
    drop(log);
    make_durable(fs, cfg);
    Unit { model }
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
/// progress — a record, a sealed or settled record, a section — so no
/// step yields before its first unit (L6: no step requeues unchanged work).
/// Red before the settle phase's progress rule: the `Settle` phase yielded
/// at a budget it had not spent, every step.
#[test]
fn a_zero_step_budget_completes_a_demoting_boot() {
    let fs = MemFs::new();
    let cfg = step_cfg();
    let model = write_unit(&fs, &cfg, 3).model;
    let mut ks = tiered_keyspace();
    let mut recovery = Recovery::new(fs.clone(), CELL, &cfg, anchor(), now());
    // Bound: at a zero budget each step consumes one record, seals one
    // record in a demote step's walk, completes a demote step, or settles
    // one record at the end of replay — under four steps per model key,
    // every key written about once — plus the start, audit and finish
    // steps.
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

/// The recovery step budget's charge for boot I/O (ADR-0174 R10, D6's
/// gauge), judged step by step at two budgets over a unit of four windows.
/// The prices are the server's and the store's `limits` consts; the
/// charge each step took is `Recovery::step_charge_bytes`, and the tier
/// writes are read from the filesystem:
///
/// - a step during which the tier files grew charged at least the one
///   barrier every flush that appends makes;
/// - a replay step passed its budget by at most one record's unit
///   (ADR-0174 A1): its bytes read — a frame counted whole at its entry —
///   plus its charge stay under the budget, one frame's entry and the
///   largest of a record settled against its cold twin and a demote step's
///   flush (a lead and a page past the need and two records of tier bytes,
///   two barriers: the flush and one seal);
/// - a replay step still inside its segment yielded at its budget: its
///   work (`Recovery::step_work_bytes`) reaches it;
/// - an end-settle step that yields charged its budget, by at most one
///   record and its twins' reads;
/// - the last one adds only the hand-over's drain (E13): one barrier, the
///   active file's seal, and the settle reads of ADR-0093's rebuild, read
///   from the stats. The drain appends nothing: every demote step appends
///   up to its cut, and the end settle seals nothing (`ro` does not move),
///   which `TierReplay::hand_over` asserts;
/// - the end settle walks its open span `[ro, tail)` at one budget byte a
///   byte and a step charges at most its budget and one record's unit, so
///   it takes at least ⌈span ÷ (budget + unit)⌉ steps;
/// - the D6 gauge equals the largest step's charge.
///
/// A settle step past its budget is a `SETTLE BUDGET VIOLATION`. Red on a
/// driver that leaves the charge out of a frame's yield test (at the
/// 8 MiB budget a step's bytes span several demote steps,
/// `inf_canary_replay_yield_uncharged`), on one that charges nothing (a
/// step that wrote tier bytes charged no barrier,
/// `inf_canary_replay_charge_dropped`), on one that settles without a
/// budget (`inf_canary_replay_settle_unbudgeted`), and on one that settles
/// at twice its budget (`inf_canary_replay_settle_doubled`: at 8 MiB the
/// whole settle is the last step, which only the drain's bound judges).
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
    // One record's unit, with the frame entry a step may park in.
    let flush_unit = lead + page + 2 * record + 2 * barrier;
    let record_unit = FRAME_BYTES_MAX + flush_unit.max(record + read_price);
    let settle_unit = record + 4 * read_price;
    let drain_unit = |rebuild_reads: u64| barrier + rebuild_reads * read_price;

    let mut ks = tiered_keyspace();
    let mut recovery = Recovery::new(fs.clone(), CELL, &cfg, anchor(), now());
    let (mut largest, mut settle_steps, mut open_span) = (0u64, 0u64, None);
    loop {
        let phase = recovery.phase();
        let segments_before = recovery.segments_progress().0;
        let consumed_before = recovery.bytes_consumed();
        let rebuild_reads_before = recovery.stats().shadow_settle_reads;
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
                assert!(
                    read + charge < budget + record_unit,
                    "{at}: {read} read + {charge} charged passes the budget by more than one \
                     record's unit ({record_unit})"
                );
                let work = recovery.step_work_bytes();
                if recovery.phase() == RecoverPhase::Replay
                    && recovery.segments_progress().0 == segments_before
                {
                    assert!(work >= budget, "{at}: yielded at a work of {work}");
                }
            }
            // The lift decision: replay has ended, and the end settle's
            // cursor starts at `ro`.
            RecoverPhase::Finish if open_span.is_none() => {
                let space = ks.tiered_store(TIER_NS).expect("tiered").space();
                open_span = Some(space.tail().to_raw() - space.ro_boundary().to_raw());
            }
            RecoverPhase::Finish => {
                settle_steps += 1;
                let violation = format!("{at}: SETTLE BUDGET VIOLATION");
                if recovery.phase() == RecoverPhase::Finish {
                    assert!(
                        charge >= budget,
                        "{violation}: a step yielded at a charge of {charge}"
                    );
                    let bound = budget + settle_unit;
                    assert!(charge < bound, "{violation}: a step charged {charge}, over {bound}");
                } else {
                    let rebuild_reads = recovery.stats().shadow_settle_reads - rebuild_reads_before;
                    let drain = drain_unit(rebuild_reads);
                    let bound = budget + settle_unit + drain;
                    assert!(
                        charge < bound,
                        "{violation}: the last step charged {charge}, over its budget, one \
                         record's unit ({settle_unit}) and the hand-over's drain ({drain})"
                    );
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
    let open_span = open_span.expect("the boot reached the lift decision");
    // The span holds at most one ring top (it is under a window, and the
    // window under the ring), whose hole is shorter than the record that
    // did not fit there; the walk passes a hole without charging it.
    let walked_min = open_span.saturating_sub(record) * inf_store::limits::SETTLE_WALK_BYTE_CHARGE;
    let steps_min = walked_min.div_ceil(budget + settle_unit);
    eprintln!(
        "budget {budget}: gauge {largest} bytes, open span {open_span} bytes, {settle_steps} \
         settle steps (at least {steps_min}), {} demote steps, {} settle reads, {} deletes \
         verified",
        replay.demote_steps, replay.settle_reads, replay.deletes_verified
    );
    assert!(
        settle_steps >= steps_min,
        "SETTLE BUDGET VIOLATION: a {open_span}-byte open span settled in {settle_steps} \
         step(s) at a {budget}-byte budget, under the {steps_min} its walk needs"
    );
    assert!(replay.demote_steps > 1, "VACUOUS: {replay:?}");
    assert!(replay.settle_reads > 0, "VACUOUS: no settle read ({replay:?})");
    assert!(replay.deletes_verified > 0, "VACUOUS: no delete verified ({replay:?})");
    if budget < WINDOW {
        // The unit's shape, not the driver: the span must outgrow a step.
        assert!(steps_min > 1, "VACUOUS: a {open_span}-byte open span fits one settle step");
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

// ---- the step judge: the work a step did, priced from what the
// filesystem saw, against its budget and one unit ----

/// Which file an operation touched, by its path under the shard.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum FileKind {
    Log,
    Ckpt,
    Tier,
    Other,
}

fn file_kind(path: &std::path::Path) -> FileKind {
    let text = path.to_string_lossy();
    if text.contains("/cold/") {
        FileKind::Tier
    } else if text.contains("/log/") {
        FileKind::Log
    } else if text.contains("/ckpt/") {
        FileKind::Ckpt
    } else {
        FileKind::Other
    }
}

/// What the filesystem saw: the bytes reads returned on the log and the
/// checkpoint, and the tier's reads, bytes written and barriers, with every
/// metadata operation.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
struct Tally {
    reader_bytes: u64,
    tier_reads: u64,
    tier_bytes_written: u64,
    tier_syncs: u64,
    dir_syncs: u64,
    opens: u64,
    removes: u64,
    renames: u64,
    lists: u64,
}

impl Tally {
    fn since(self, before: Tally) -> Tally {
        Tally {
            reader_bytes: self.reader_bytes - before.reader_bytes,
            tier_reads: self.tier_reads - before.tier_reads,
            tier_bytes_written: self.tier_bytes_written - before.tier_bytes_written,
            tier_syncs: self.tier_syncs - before.tier_syncs,
            dir_syncs: self.dir_syncs - before.dir_syncs,
            opens: self.opens - before.opens,
            removes: self.removes - before.removes,
            renames: self.renames - before.renames,
            lists: self.lists - before.lists,
        }
    }

    /// The work at the `limits` prices: a reader byte and a tier byte one
    /// budget byte each, a settle read at the store's read price, a
    /// barrier — a file's or a directory's — at the server's.
    fn priced(self) -> u64 {
        let barrier = inf_server::limits::REPLAY_BARRIER_CHARGE_BYTES;
        self.reader_bytes
            + self.tier_bytes_written * inf_server::limits::REPLAY_TIER_BYTE_CHARGE
            + self.tier_reads * inf_store::limits::SETTLE_READ_CHARGE_BYTES
            + (self.tier_syncs + self.dir_syncs) * barrier
    }
}

/// A `MemFs` whose every operation the judge counts.
#[derive(Clone)]
struct CountingFs {
    inner: MemFs,
    tally: std::rc::Rc<std::cell::Cell<Tally>>,
}

impl CountingFs {
    fn new() -> CountingFs {
        CountingFs { inner: MemFs::new(), tally: std::rc::Rc::default() }
    }

    fn tally(&self) -> Tally {
        self.tally.get()
    }

    fn count(&self, op: impl FnOnce(&mut Tally)) {
        let mut tally = self.tally.get();
        op(&mut tally);
        self.tally.set(tally);
    }

    fn wrap(&self, path: &std::path::Path, file: inf_log::fs::mem::MemFile) -> CountingFile {
        self.count(|t| t.opens += 1);
        CountingFile { inner: file, kind: file_kind(path), tally: self.tally.clone() }
    }
}

struct CountingFile {
    inner: inf_log::fs::mem::MemFile,
    kind: FileKind,
    tally: std::rc::Rc<std::cell::Cell<Tally>>,
}

impl CountingFile {
    fn count(&self, op: impl FnOnce(&mut Tally)) {
        let mut tally = self.tally.get();
        op(&mut tally);
        self.tally.set(tally);
    }
}

impl SegmentFile for CountingFile {
    fn write_at(&mut self, offset: u64, data: &[u8]) -> std::io::Result<()> {
        if self.kind == FileKind::Tier {
            self.count(|t| t.tier_bytes_written += data.len() as u64);
        }
        self.inner.write_at(offset, data)
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> std::io::Result<usize> {
        let read = self.inner.read_at(offset, buf)?;
        match self.kind {
            FileKind::Log | FileKind::Ckpt => self.count(|t| t.reader_bytes += read as u64),
            FileKind::Tier => self.count(|t| t.tier_reads += 1),
            FileKind::Other => {}
        }
        Ok(read)
    }

    fn file_size(&self) -> std::io::Result<u64> {
        self.inner.file_size()
    }

    fn truncate(&mut self, len: u64) -> std::io::Result<()> {
        self.inner.truncate(len)
    }

    fn sync_data(&mut self) -> std::io::Result<()> {
        if self.kind == FileKind::Tier {
            self.count(|t| t.tier_syncs += 1);
        }
        self.inner.sync_data()
    }

    fn raw_fd(&self) -> Option<std::os::fd::RawFd> {
        self.inner.raw_fd()
    }

    fn fully_allocated(&self) -> std::io::Result<bool> {
        self.inner.fully_allocated()
    }
}

impl SegmentFs for CountingFs {
    type File = CountingFile;

    fn create_dir_all(&self, dir: &std::path::Path) -> std::io::Result<()> {
        self.inner.create_dir_all(dir)
    }

    fn sync_dir(&self, dir: &std::path::Path) -> std::io::Result<()> {
        self.count(|t| t.dir_syncs += 1);
        self.inner.sync_dir(dir)
    }

    fn list_dir(&self, dir: &std::path::Path) -> std::io::Result<Vec<String>> {
        self.count(|t| t.lists += 1);
        self.inner.list_dir(dir)
    }

    fn create_segment(
        &self,
        path: &std::path::Path,
        prealloc: u64,
    ) -> std::io::Result<CountingFile> {
        let file = self.inner.create_segment(path, prealloc)?;
        Ok(self.wrap(path, file))
    }

    fn create_meta(&self, path: &std::path::Path) -> std::io::Result<CountingFile> {
        let file = self.inner.create_meta(path)?;
        Ok(self.wrap(path, file))
    }

    fn create_meta_direct(&self, path: &std::path::Path) -> std::io::Result<CountingFile> {
        let file = self.inner.create_meta_direct(path)?;
        Ok(self.wrap(path, file))
    }

    fn open_dir(&self, dir: &std::path::Path) -> std::io::Result<CountingFile> {
        let file = self.inner.open_dir(dir)?;
        Ok(self.wrap(dir, file))
    }

    fn open_write(&self, path: &std::path::Path) -> std::io::Result<CountingFile> {
        let file = self.inner.open_write(path)?;
        Ok(self.wrap(path, file))
    }

    fn open_read(&self, path: &std::path::Path) -> std::io::Result<CountingFile> {
        let file = self.inner.open_read(path)?;
        Ok(self.wrap(path, file))
    }

    fn rename(&self, from: &std::path::Path, to: &std::path::Path) -> std::io::Result<()> {
        self.count(|t| t.renames += 1);
        self.inner.rename(from, to)
    }

    fn remove_file(&self, path: &std::path::Path) -> std::io::Result<()> {
        self.count(|t| t.removes += 1);
        self.inner.remove_file(path)
    }
}

/// Bytes of one value of the hostile unit: a tiered record of 80 bytes
/// with its 8-byte key and header.
const SMALL_VALUE_LEN: usize = 64;

fn small_value(i: u64, generation: u8) -> Vec<u8> {
    let mut v = vec![generation; SMALL_VALUE_LEN];
    v[..8].copy_from_slice(&i.to_le_bytes());
    v
}

/// The hostile frame's recovery unit: distinct keys of
/// 80-byte records until they fill `windows` RAM windows, in frames of at
/// most 64 KiB, then ONE frame of up to `frame_bytes` that rewrites the
/// oldest of them — keys the boot has demoted by the time it replays the
/// frame — so the frame's records need room and their seal settles read
/// each one's cold twin. Returns the model and the frame's length.
fn write_hostile_unit<F: SegmentFs + Clone>(
    fs: &F,
    cfg: &DurableConfig,
    windows: u64,
    frame_bytes: u32,
) -> (Model, u64) {
    write_late_frame(fs, cfg, windows, frame_bytes, Late::Rewrite)
}

/// What the hostile frame does to the oldest keys.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Late {
    /// Rewrites each: a record that needs room, sealed against its twin.
    Rewrite,
    /// Deletes each: a `DEL` that reads its demoted copy (a priced answer).
    Delete,
}

/// The distinct-key prefix of [`write_hostile_unit`], then one frame of
/// `late` records of the oldest keys up to `frame_bytes`.
fn write_late_frame<F: SegmentFs + Clone>(
    fs: &F,
    cfg: &DurableConfig,
    windows: u64,
    frame_bytes: u32,
    late: Late,
) -> (Model, u64) {
    let mut log = Log::new(fs, cfg);
    let mut model = Model::new();
    let record = (TieredTable::RECORD_HEADER_LEN + key(0).len() + SMALL_VALUE_LEN) as u64;
    let keys = windows * WINDOW / record;
    let mut next = 0u64;
    while next < keys {
        let batch: Vec<(Vec<u8>, Vec<u8>)> =
            (next..(next + 512).min(keys)).map(|i| (key(i), small_value(i, 0))).collect();
        let effects: Vec<MutationEffect<'_>> = batch
            .iter()
            .map(|(key, value)| MutationEffect::StringSet { ns: TIER_NS, key, value })
            .collect();
        log.frame(&effects);
        next += batch.len() as u64;
        model.extend(batch.into_iter().map(|(key, value)| (key, Some(value))));
    }
    // The hostile frame: from key 0 on until the ring is full or the
    // frame reaches `frame_bytes`.
    for effect_key in 0..keys {
        let (key, value) = (key(effect_key), small_value(effect_key, 1));
        let effect = match late {
            Late::Rewrite => MutationEffect::StringSet { ns: TIER_NS, key: &key, value: &value },
            Late::Delete => MutationEffect::Delete { ns: TIER_NS, key: &key },
        };
        if log.ring.pending_frame_len() + 64 > frame_bytes || log.ring.stage(&effect).is_err() {
            break;
        }
        model.insert(key, (late == Late::Rewrite).then_some(value));
    }
    let frame_len = u64::from(log.ring.pending_frame_len());
    log.seal_frame();
    drop(log);
    make_durable(fs, cfg);
    (model, frame_len)
}

/// One unit of a replay step, less its frame entry, from the
/// `limits` and the unit the test wrote, never from the engine: the
/// largest of a record, a record settled against its one cold twin (no
/// forced collisions: a key has one cold slot), and a demote step's flush —
/// a lead, a page and two records of tier bytes, with its barrier, one seal
/// it may cross and the directory sync of the file that seal opens.
fn replay_record_unit(record: u64) -> u64 {
    let barrier = inf_server::limits::REPLAY_BARRIER_CHARGE_BYTES;
    let lead = 1u64 << 20; // `MAINTAIN-SLICE` at the smallest window, a whole page
    let page = inf_alloc::REGION_PAGE_BYTES as u64;
    let settled = record + inf_store::limits::SETTLE_READ_CHARGE_BYTES;
    let flush = lead + page + 2 * record + 3 * barrier;
    record.max(settled).max(flush)
}

/// A boot judged step by step: for every step of a budgeted row
/// of `RECOVERY_UNITS` (`Recovery::unit_row`), its observed work — priced
/// from the counting filesystem — is at most the budget, the row's unit `U`
/// from `unit_of` (computed by the test, never by the engine) and the
/// reader's one read chunk on a frame row. The end settle's last step runs
/// the hand-over and the log resume, which the hand-over phase's own steps
/// will cut, and is not judged here. Returns the steps over the bound, the
/// recovery's stats and the recovered keyspace; a step that refuses is the
/// caller's `Err`.
fn judged_boot(
    fs: &CountingFs,
    cfg: &DurableConfig,
    mut ks: Keyspace,
    budget: u64,
    unit_of: &dyn Fn(inf_server::UnitPhase) -> u64,
) -> (Vec<String>, std::io::Result<inf_server::RecoverStats>, Keyspace) {
    use inf_server::{RecoverPhase, UnitEntry};
    let mut recovery = Recovery::new(fs.clone(), CELL, cfg, anchor(), now());
    let mut over = Vec::new();
    // Bound: every step makes progress (the zero-budget row).
    for step in 0u64.. {
        let before = fs.tally();
        let progress = match recovery.step(&mut ks, budget) {
            Ok(progress) => progress,
            Err(err) => return (over, Err(err), ks),
        };
        let work = fs.tally().since(before);
        let row = recovery.unit_row();
        let last_settle = row.phase == inf_server::UnitPhase::Settle
            && recovery.phase() == RecoverPhase::Complete;
        if row.budgeted && !last_settle {
            let chunk =
                if row.entry == UnitEntry::Frame { inf_log::DEFAULT_READ_CHUNK as u64 } else { 0 };
            let bound = budget.saturating_add(unit_of(row.phase)).saturating_add(chunk);
            if work.priced() > bound {
                over.push(format!(
                    "step {step} ({:?}): {} budget bytes of work over the bound {bound} \
                     ({work:?})",
                    row.phase,
                    work.priced()
                ));
            }
        }
        if progress == RecoveryProgress::Complete {
            break;
        }
    }
    let (_rotor, stats, _seed) = recovery.finish();
    (over, Ok(stats), ks)
}

/// The yields inside a unit a boot made (ADR-0174 A1.2's liveness
/// counter): `Err` naming `VACUOUS` when none, so a reading of the gauge on
/// a boot that never parked is no evidence.
fn engagement(stats: &inf_server::RecoverStats) -> Result<u64, String> {
    let step = stats.step;
    let yields = step.yields_in_frame + step.yields_in_section + step.yields_in_demote_step;
    if yields == 0 {
        return Err(format!("VACUOUS: no step yielded inside a unit ({step:?})"));
    }
    Ok(yields)
}

/// `U` for the hostile units: a frame's entry plus one record's unit on the
/// replay row, a record settled against its twin on the end settle's.
fn hostile_unit(frame_entry: u64) -> impl Fn(inf_server::UnitPhase) -> u64 {
    let record = (TieredTable::RECORD_HEADER_LEN + key(0).len() + SMALL_VALUE_LEN) as u64;
    move |phase| match phase {
        inf_server::UnitPhase::Replay => frame_entry + replay_record_unit(record),
        inf_server::UnitPhase::Settle => record + inf_store::limits::SETTLE_READ_CHARGE_BYTES,
        other => panic!("the hostile units judge no {other:?} step"),
    }
}

/// One 4 MiB frame of 80-byte rewrites of keys the
/// boot has demoted, replayed at a 256 KiB budget under the step judge
/// (`U` the frame's entry plus one record's unit). Every judged
/// step's work is within `B + U + C`, read before the engagement clause,
/// and the boot recovers the model. Red where the frame's demote steps run
/// inside the step that enters it (`inf_canary_recover_frame_whole`) and
/// where a demote step's seal walk ignores the budget left
/// (`inf_canary_recover_seal_unbudgeted`).
#[test]
fn a_frame_of_rewrites_of_demoted_keys_is_replayed_under_the_step_judge() {
    const BUDGET: u64 = 256 << 10;
    let fs = CountingFs::new();
    let cfg = step_cfg();
    let (model, frame_len) = write_hostile_unit(&fs, &cfg, 3, 4 << 20);
    assert!(frame_len > (4 << 20) - (64 << 10), "VACUOUS: a {frame_len}-byte hostile frame");
    let (over, stats, mut ks) =
        judged_boot(&fs, &cfg, tiered_keyspace(), BUDGET, &hostile_unit(frame_len));
    let stats = stats.expect("the hostile frame boots");
    let replay = stats.tier_replay.counters;
    eprintln!(
        "hostile frame {frame_len} bytes at {BUDGET}: {} steps over the judge, {} demote steps, \
         {} settle reads, {:?}",
        over.len(),
        replay.demote_steps,
        replay.settle_reads,
        stats.step
    );
    assert!(over.is_empty(), "JUDGE: {} steps over B + U + C: {:?}", over.len(), over.first());
    engagement(&stats).expect("yields inside the frame");
    assert!(replay.demote_steps > 1, "VACUOUS: {replay:?}");
    assert!(replay.settle_reads > 0, "VACUOUS: no settle read ({replay:?})");
    let (served, _, _) = read_back(&fs, &cfg, &mut ks, &model);
    assert_model(&served, &model, "after the hostile frame");
}

/// The liveness counter's own planted red (ADR-0174 A1.2): the `u64::MAX`
/// control leg never parks, so it reads no yield inside a unit and the
/// engagement check names it `VACUOUS`; the same unit at 256 KiB yields
/// inside its frame. A gauge read on a boot that never parked is no reading.
#[test]
fn the_control_leg_never_parks_and_its_engagement_reads_vacuous() {
    let fs = CountingFs::new();
    let cfg = step_cfg();
    let (_model, frame_len) = write_hostile_unit(&fs, &cfg, 2, 1 << 20);
    for (budget, parks) in [(u64::MAX, false), (256 << 10, true)] {
        let (_over, stats, _ks) =
            judged_boot(&fs, &cfg, tiered_keyspace(), budget, &hostile_unit(frame_len));
        let stats = stats.expect("boots");
        let verdict = engagement(&stats);
        if parks {
            assert!(verdict.is_ok(), "budget {budget}: {verdict:?}");
        } else {
            let err = verdict.expect_err("the control leg never parks");
            assert!(err.contains("VACUOUS"), "{err}");
            assert_eq!(stats.step.yields_in_frame, 0, "the control leg parks in no frame");
        }
    }
}

/// A 1 MiB frame of deletes of keys the boot demoted, at
/// the default budget (8 MiB, which the frame fits) and at `u64::MAX`. Each
/// delete reads its demoted copy, a priced answer, so the walk charges it
/// and asks after it: every judged step stays within `B + U + C`, and the
/// default leg yields inside the frame. Red where a `DEL` that read answers
/// as one that did not (`inf_canary_recover_del_unasked`): the whole walk
/// never asks and one step runs every read.
#[test]
fn a_frame_of_deletes_of_demoted_keys_asks_after_each_priced_delete() {
    let default_budget = inf_server::RecoverConfig::default().step_bytes;
    let fs = CountingFs::new();
    let cfg = step_cfg();
    let (model, frame_len) = write_late_frame(&fs, &cfg, 3, 1 << 20, Late::Delete);
    for budget in [default_budget, u64::MAX] {
        let (over, stats, mut ks) =
            judged_boot(&fs, &cfg, tiered_keyspace(), budget, &hostile_unit(frame_len));
        let stats = stats.expect("the delete frame boots");
        let replay = stats.tier_replay.counters;
        eprintln!("delete frame at {budget}: {} over, {replay:?}, {:?}", over.len(), stats.step);
        assert!(over.is_empty(), "JUDGE: {} steps over B + U + C: {:?}", over.len(), over.first());
        assert!(replay.deletes_verified > 1000, "VACUOUS: {replay:?}");
        if budget == default_budget {
            engagement(&stats).expect("yields inside the delete frame");
        }
        let (served, _, _) = read_back(&fs, &cfg, &mut ks, &model);
        assert_model(&served, &model, &format!("after the delete frame at {budget}"));
    }
}

/// One boot's recovered state for budget independence: the
/// table digest, every tier file byte for byte, what every key serves, the
/// D6 counters (the gauges and the step facts excepted) and the record
/// counts.
#[derive(Debug, PartialEq, Eq)]
struct Recovered {
    table: inf_store::StateDigest,
    files: Vec<(String, Vec<u8>)>,
    served: Model,
    counters: inf_store::ReplayCounters,
    dead_life_files_removed: u64,
    records_applied: u64,
    records_pre_begin: u64,
}

fn file_bytes(fs: &MemFs, path: &std::path::Path) -> Vec<u8> {
    let file = fs.open_read(path).expect("open");
    let mut bytes = vec![0u8; file.file_size().expect("size") as usize];
    let mut done = 0usize;
    while done < bytes.len() {
        let n = file.read_at(done as u64, &mut bytes[done..]).expect("read");
        assert!(n > 0, "short file");
        done += n;
    }
    bytes
}

fn recovered(fs: &CountingFs, cfg: &DurableConfig, budget: u64, model: &Model) -> Recovered {
    let mut ks = tiered_keyspace();
    let mut recovery = Recovery::new(fs.clone(), CELL, cfg, anchor(), now());
    while recovery.step(&mut ks, budget).expect("a step") == RecoveryProgress::Working {}
    let (_rotor, stats, _seed) = recovery.finish();
    let cold = cold_dir(cfg);
    let mut names = fs.list_dir(&cold).unwrap_or_default();
    names.sort();
    let files = names
        .into_iter()
        .map(|name| (name.clone(), file_bytes(&fs.inner, &cold.join(name))))
        .collect();
    let (served, _, _) = read_back(fs, cfg, &mut ks, model);
    if budget != u64::MAX && budget <= 256 << 10 {
        engagement(&stats).expect("a small budget yields inside a unit");
    }
    Recovered {
        table: ks.tiered_store(TIER_NS).expect("tiered").simulation_digest(),
        files,
        served,
        counters: stats.tier_replay.counters,
        dead_life_files_removed: stats.tier_replay.dead_life_files_removed,
        records_applied: stats.records_applied,
        records_pre_begin: stats.records_pre_begin,
    }
}

/// Budget independence: the same recovery unit — a 1 MiB frame
/// of rewrites of demoted keys over two windows — recovered at `u64::MAX`
/// (the control, which never parks) and at 8 MiB, 256 KiB, 4 KiB, 1 and 0
/// recovers the same table, tier files, served values, D6 counters and
/// record counts. Red where a resumed walk passes a record that needs room
/// (`inf_canary_recover_skip_on_resume`: a key absent) or applies its last
/// consumed record again (`inf_canary_recover_twice_on_resume`: counters
/// and tier files differ).
#[test]
fn every_budget_recovers_the_state_of_the_unparked_boot() {
    let cfg = step_cfg();
    // Each boot recovers its own copy of the unit, written the same way:
    // a boot's tier files are the next boot's dead-life files (D4).
    let boot = |budget: u64| {
        let fs = CountingFs::new();
        let (model, _frame_len) = write_hostile_unit(&fs, &cfg, 2, 1 << 20);
        let state = recovered(&fs, &cfg, budget, &model);
        (model, state)
    };
    let (model, control) = boot(u64::MAX);
    assert_model(&control.served, &model, "the control boot");
    assert!(control.counters.demote_steps > 1, "VACUOUS: {:?}", control.counters);
    for budget in [8 << 20, 256 << 10, 4 << 10, 1, 0] {
        let (model, state) = boot(budget);
        assert_model(&state.served, &model, &format!("BUDGET INDEPENDENCE at {budget}"));
        assert_same(&state, &control, budget);
    }
}

/// Budget independence's comparison, naming the first field that differs.
fn assert_same(state: &Recovered, control: &Recovered, budget: u64) {
    let at = format!("BUDGET INDEPENDENCE: budget {budget}");
    assert_eq!(state.table, control.table, "{at}: the table digest");
    let names =
        |r: &Recovered| r.files.iter().map(|(n, b)| (n.clone(), b.len())).collect::<Vec<_>>();
    assert_eq!(names(state), names(control), "{at}: the tier files' names and sizes");
    for ((name, bytes), (_, want)) in state.files.iter().zip(&control.files) {
        let differs = bytes.iter().zip(want).position(|(a, b)| a != b);
        assert!(differs.is_none(), "{at}: {name} differs from byte {differs:?}");
    }
    assert!(state.served == control.served, "{at}: the served values");
    assert_eq!(state.counters, control.counters, "{at}: the D6 counters");
    assert_eq!(state.dead_life_files_removed, control.dead_life_files_removed, "{at}");
    assert_eq!(state.records_applied, control.records_applied, "{at}: records applied");
    assert_eq!(state.records_pre_begin, control.records_pre_begin, "{at}: records below begin");
}

/// A boot of the memory namespace's unit on `fs` at `budget`: its stats and
/// the keyspace's state digest; `Err` is the boot's refusal.
fn memory_boot(
    fs: &MemFs,
    cfg: &DurableConfig,
    budget: u64,
) -> (std::io::Result<inf_server::RecoverStats>, inf_store::StateDigest) {
    let mut ks = fresh_keyspace();
    let mut recovery = Recovery::new(fs.clone(), CELL, cfg, anchor(), now());
    // Bound: every step makes progress (the zero-budget row).
    loop {
        match recovery.step(&mut ks, budget) {
            Ok(RecoveryProgress::Working) => {}
            Ok(RecoveryProgress::Complete) => break,
            Err(err) => return (Err(err), ks.state_digest(now())),
        }
    }
    let (_rotor, stats, _seed) = recovery.finish();
    (Ok(stats), ks.state_digest(now()))
}

/// A checkpoint of `images` published at `begin`, named by the MANIFEST,
/// with `sidecar` entries of one index sidecar after them (the writer
/// streams derived data last).
fn publish_checkpoint(
    fs: &MemFs,
    cfg: &DurableConfig,
    begin: inf_log::Lsn,
    images: &[(&[u8], &[u8])],
    sidecar: u64,
) {
    use inf_log::ckpt::SyncIckWriter;
    let shard = cfg.data_dir.join(format!("shard-{CELL}"));
    let mut writer = SyncIckWriter::create_v3(
        fs.clone(),
        &shard.join("ckpt"),
        &cfg.ckpt,
        CELL,
        1,
        begin,
        &[NS.0],
    )
    .expect("ick create");
    for (key, value) in images {
        writer
            .append(&inf_log::RecordView::StringPostImage { ns: NS, key, value })
            .expect("ick record");
    }
    if sidecar > 0 {
        let meta = inf_log::ckpt::IdxSidecarMeta {
            ns: NS.0,
            index_id: 1,
            generation: 1,
            key_encoding_version: 1,
            fixed8: true,
            maint_rules: inf_log::ckpt::IdxSidecarRules::PRE_A2,
        };
        for ordinal in 0..sidecar {
            writer
                .append_idx_entry(&meta, ordinal, &ordinal.to_be_bytes(), ordinal)
                .expect("entry");
        }
        writer.append_idx_final(&meta, sidecar).expect("final");
    }
    writer.finish().expect("ick publish");
    inf_log::write_manifest(
        fs,
        &shard,
        &inf_log::Manifest {
            ckpt_id: 1,
            begin_lsn: begin,
            segments: vec![begin.segment],
            tiers: Vec::new(),
            key_hash_id: inf_foundation::KeyHasher::default().identity(),
        },
    )
    .expect("manifest");
}

/// The floor segment's frame holds records below and
/// above the checkpoint's begin LSN, and at a budget of 1 the walk parks at
/// every record, among those below `begin` too. Those below are skipped
/// and counted, those above applied once: `records_pre_begin`,
/// `records_applied` and the keyspace equal the `u64::MAX` control's
/// (budget independence). Red where the begin test runs only on a walk that starts at
/// a frame's entry (`inf_canary_recover_begin_skip_entry_only`): a resumed
/// walk applies the records below `begin`, older than the checkpoint.
#[test]
fn a_parked_frame_across_the_begin_lsn_skips_below_and_applies_above_once() {
    let fs = MemFs::new();
    let cfg = cfg();
    let mut log = LogBuilder::new(&fs, &cfg);
    let keys: Vec<Vec<u8>> = (0..12).map(|i| format!("b:{i:02}").into_bytes()).collect();
    let mut effects: Vec<MutationEffect<'_>> = keys[..6]
        .iter()
        .map(|key| MutationEffect::StringSet { ns: NS, key, value: b"older" })
        .collect();
    effects.push(MutationEffect::CkptBegin { ckpt_id: 1 });
    effects.extend(keys[6..].iter().map(|key| MutationEffect::StringSet {
        ns: NS,
        key,
        value: b"tail",
    }));
    let (_, lsns) = log.frame(&effects);
    let begin = lsns[6];
    let images: Vec<(&[u8], &[u8])> =
        keys[..6].iter().map(|key| (&key[..], &b"checkpointed"[..])).collect();
    publish_checkpoint(&fs, &cfg, begin, &images, 0);
    drop(log);
    let (control, control_digest) = memory_boot(&fs, &cfg, u64::MAX);
    let control = control.expect("the control boots");
    assert_eq!(control.records_pre_begin, 6, "the six records below begin are skipped");
    let (stats, digest) = memory_boot(&fs, &cfg, 1);
    let stats = stats.expect("the parked boot");
    // A yield after every record: five of them among the records below
    // `begin`, the frame's first five.
    assert!(stats.step.yields_in_frame >= 6, "VACUOUS: {:?}", stats.step);
    assert_eq!(stats.records_pre_begin, control.records_pre_begin, "BEGIN: records below begin");
    assert_eq!(stats.records_applied, control.records_applied, "BEGIN: records applied");
    assert_eq!(digest, control_digest, "BEGIN: the recovered keyspace");
}

/// A CRC-valid frame whose header counts one record
/// fewer than its body holds. The boot refuses at `u64::MAX` and at a
/// budget of 1 with the walk parked at the frame's last counted record, on
/// the record iterator's end check: the error names `TrailingBytes`. Red
/// where the move that leaves no record ends the frame on its count alone
/// (`inf_canary_recover_frame_end_counted`): the boot completes.
#[test]
fn a_frame_whose_count_is_short_refuses_the_boot_at_every_budget() {
    let fs = MemFs::new();
    let cfg = cfg();
    let mut log = LogBuilder::new(&fs, &cfg);
    let (base, _) = log.frame(&[
        MutationEffect::StringSet { ns: NS, key: b"a", value: b"1" },
        MutationEffect::StringSet { ns: NS, key: b"b", value: b"2" },
        MutationEffect::StringSet { ns: NS, key: b"c", value: b"3" },
    ]);
    // The header's record count, one fewer, and the trailer recomputed.
    let path = log.seg_path(base.segment);
    let segment = file_bytes(&fs, &path);
    let at = base.offset as usize;
    let frame_len = u32::from_le_bytes(segment[at + 4..at + 8].try_into().expect("4")) as usize;
    let mut frame = segment[at..at + frame_len].to_vec();
    frame[8..12].copy_from_slice(&2u32.to_le_bytes());
    let crc = inf_simd::crc32c(&frame[..frame_len - 4]);
    frame[frame_len - 4..].copy_from_slice(&crc.to_le_bytes());
    log.poke(base.segment, base.offset, &frame);
    drop(log);
    let (decoded, _) =
        inf_log::decode_frame(&frame, inf_log::DEFAULT_MAX_FRAME_LEN).expect("valid");
    let walk: Vec<_> = decoded.records().collect();
    assert_eq!(walk.len(), 3, "two counted records, then the end check");
    assert!(walk[..2].iter().all(Result::is_ok), "the counted records decode");
    assert!(
        matches!(walk[2], Err(inf_log::FrameRecordError::TrailingBytes { .. })),
        "{:?}",
        walk[2]
    );
    for budget in [u64::MAX, 1] {
        let mut ks = fresh_keyspace();
        let mut recovery = Recovery::new(fs.clone(), CELL, &cfg, anchor(), now());
        // Bound: every step makes progress; the boot refuses or completes.
        let err = loop {
            match recovery.step(&mut ks, budget) {
                Ok(RecoveryProgress::Working) => {}
                Ok(RecoveryProgress::Complete) => {
                    panic!("SHORT COUNT: the short-count frame booted at {budget}")
                }
                Err(err) => break err,
            }
        };
        let text = err.to_string();
        assert!(text.contains("TrailingBytes { at:"), "SHORT COUNT at {budget}: {text}");
        if budget == 1 {
            let step = recovery.stats().step;
            assert!(step.yields_in_frame > 0, "VACUOUS: no yield in the frame ({step:?})");
        }
    }
}

/// The budget-ask count: at `u64::MAX`, on a unit that demotes
/// nothing, the replay and checkpoint phases ask their budget once per
/// frame and once per section they enter — a walk that fits pays one ask,
/// as one compare per frame did. A unit of 70 frames of three records and
/// a checkpoint of at least 16 sections, an index sidecar's among them
/// where the build carries documents. Red where a whole walk asks after
/// every record (`inf_canary_recover_ask_per_record`).
#[test]
fn a_unit_that_fits_asks_once_per_frame_and_per_section() {
    let fs = MemFs::new();
    let mut cfg = cfg_with(SegmentConfig { segment_bytes: 1 << 20, ..Default::default() });
    cfg.ckpt.section_bytes = 4096;
    let mut log = LogBuilder::new(&fs, &cfg);
    let (_, lsns) = log.frame(&[MutationEffect::CkptBegin { ckpt_id: 1 }]);
    let begin = lsns[0];
    let keys: Vec<Vec<u8>> = (0..210).map(|i| format!("f:{i:04}").into_bytes()).collect();
    for chunk in keys.chunks(3) {
        let effects: Vec<MutationEffect<'_>> = chunk
            .iter()
            .map(|key| MutationEffect::StringSet { ns: NS, key, value: b"v" })
            .collect();
        log.frame(&effects);
    }
    let values: Vec<(Vec<u8>, Vec<u8>)> =
        (0..400).map(|i| (format!("c:{i:04}").into_bytes(), vec![b'x'; 200])).collect();
    let images: Vec<(&[u8], &[u8])> = values.iter().map(|(k, v)| (&k[..], &v[..])).collect();
    let sidecar = if cfg!(feature = "doc") { 2000 } else { 0 };
    publish_checkpoint(&fs, &cfg, begin, &images, sidecar);
    drop(log);
    let ick = cfg.data_dir.join(format!("shard-{CELL}/ckpt")).join(inf_log::ckpt::ick_file_name(1));
    let (sections, sidecar_sections) = ick_sections(&fs, &ick);
    assert_eq!(sidecar_sections > 0, sidecar > 0, "VACUOUS: {sidecar_sections} sidecar sections");
    let (stats, _) = memory_boot(&fs, &cfg, u64::MAX);
    let stats = stats.expect("the unit boots");
    assert!(stats.frames >= 64, "VACUOUS: {} frames", stats.frames);
    assert!(sections >= 16, "VACUOUS: {sections} sections");
    assert_eq!(
        stats.budget_asks,
        stats.frames + sections,
        "ASK COUNT: {} asks for {} frames and {sections} sections",
        stats.budget_asks,
        stats.frames
    );
}

/// The sections of the checkpoint at `ick`, counted by its reader, and of
/// them the index sidecar's.
fn ick_sections(fs: &MemFs, ick: &std::path::Path) -> (u64, u64) {
    use inf_log::ckpt::{IckReader, IckReaderConfig, IckStep};
    let mut reader = IckReader::open(fs, ick, IckReaderConfig::default()).expect("open the ick");
    let (mut sections, sidecar) = (0u64, std::cell::Cell::new(0u64));
    loop {
        let step = reader
            .next_step_hybrid(
                |_| Ok::<(), ()>(()),
                |_| Ok(()),
                |_| Ok(()),
                |_| Ok(()),
                |_| {
                    sidecar.set(sidecar.get() + 1);
                    Ok(())
                },
            )
            .expect("a section");
        match step {
            IckStep::Section { .. } => sections += 1,
            IckStep::Done(_) => return (sections, sidecar.get()),
        }
    }
}

/// Four room asks across suspensions, at the server tier: the hostile spec of ADR-0174 D2
/// rule 6 (`MEM-BUDGET 4mb BLOB-THRESHOLD 3mb`: a 5 MiB window, an 8 MiB
/// ring, 2.9 MiB records) between runs of small records, replayed at a
/// budget of 1, where a demote step suspends after every record its seal
/// walk passes and the record waiting for room is presented again at each
/// resume. Its room asks count across suspensions, the resumed demote step's
/// question being the same ask: the boot pads, demotes and completes with no
/// `RoomAsks`. Red where a resumed demote's room ask counts as a new one
/// (`inf_canary_recover_room_reask_counted`).
#[test]
fn room_asks_count_across_suspensions_and_the_hostile_spec_boots() {
    const BIG: usize = 2_900 << 10;
    let fs = MemFs::new();
    let cfg = cfg_with(SegmentConfig { segment_bytes: 64 << 20, ..Default::default() });
    let mut spec = TierSpec::for_budget(4 << 20);
    spec.blob_threshold_bytes = 3 << 20;
    let keyspace = || {
        let mut ks = fresh_keyspace();
        ks.ns_create(NsSpec {
            id: TIER_NS,
            name: b"hostile".to_vec(),
            mode: NsMode::Durable,
            fsync: Some(FsyncClass::Always),
            policy: None,
            maxmemory: None,
            tier: Some(spec),
        })
        .expect("tiered ns");
        ks
    };
    let mut log = Log::new(&fs, &cfg);
    let mut model = Model::new();
    for round in 0..12u64 {
        let small: Vec<(Vec<u8>, Vec<u8>)> =
            (0..64u64).map(|i| (key(round * 1000 + i), small_value(i, 0))).collect();
        let effects: Vec<MutationEffect<'_>> = small
            .iter()
            .map(|(key, value)| MutationEffect::StringSet { ns: TIER_NS, key, value })
            .collect();
        log.frame(&effects);
        model.extend(small.into_iter().map(|(k, v)| (k, Some(v))));
        let (big_key, big_value) = (format!("big:{round:02}").into_bytes(), vec![round as u8; BIG]);
        let effect = MutationEffect::StringSet { ns: TIER_NS, key: &big_key, value: &big_value };
        log.ring.stage(&effect).expect("stage");
        log.seal_frame();
        model.insert(big_key, Some(big_value));
    }
    drop(log);
    make_durable(&fs, &cfg);
    let mut ks = keyspace();
    let mut recovery = Recovery::new(fs.clone(), CELL, &cfg, anchor(), now());
    // Bound: every step makes progress (the zero-budget row).
    while recovery.step(&mut ks, 1).expect("ROOM ASKS: the hostile spec boots")
        == RecoveryProgress::Working
    {}
    let (_rotor, stats, _seed) = recovery.finish();
    let replay = stats.tier_replay.counters;
    eprintln!("hostile spec at a budget of 1: {replay:?}, {:?}", stats.step);
    assert!(replay.pads_placed > 0, "VACUOUS: no pad ({replay:?})");
    assert!(stats.step.yields_in_demote_step > 0, "VACUOUS: no demote step suspended");
    let (served, _, _) = read_back(&fs, &cfg, &mut ks, &model);
    assert_model(&served, &model, "after the hostile spec");
}
