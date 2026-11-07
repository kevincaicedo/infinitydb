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

    /// One frame holding a checkpoint's begin marker; the marker's LSN.
    fn begin_marker(&mut self, ckpt_id: u64) -> inf_log::Lsn {
        let at = self.ring.stage(&MutationEffect::CkptBegin { ckpt_id }).expect("stage");
        self.rotor.maintain(0).expect("maintain");
        let slot = self.rotor.begin_frame(self.ring.pending_frame_len(), 0).expect("reserve");
        let covered = slot.base().to_u64();
        let lease = self.ring.seal(slot.first_record_lsn(), covered, slot.layout());
        let frame = self.ring.leased_frame(&lease).to_vec();
        self.rotor.commit_frame(slot, &frame).expect("commit");
        let lsn = lease.lsn_of(at);
        self.ring.release(lease);
        lsn
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
///   record and its twins' reads, and the last one stays within its budget
///   and the same unit: the hand-over is a phase of its own;
/// - a hand-over step's boot I/O stays within its budget and one unit, a
///   rebuild's settle read or the drain's seal of the active file (one
///   barrier). The drain appends nothing: every demote step appends up to
///   its cut, and the end settle seals nothing (`ro` does not move), which
///   `TierReplay::hand_over` asserts;
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
/// whole settle is the last step, which only its bound of one unit judges).
#[test]
fn every_step_yields_at_the_first_boundary_where_its_reads_and_charge_reach_the_budget() {
    // The upper bounds are asserted as each step runs, the yields under the
    // budget once both budgets ran: a driver that leaves boot I/O out of its
    // yield test also yields early, and the bound it passes is its red.
    let mut early = Vec::new();
    for budget in [256 << 10, 8 << 20] {
        early.extend(step_under_budget(budget));
    }
    assert!(
        early.is_empty(),
        "{} replay steps yielded under their budget: {:?}",
        early.len(),
        early.first()
    );
}

/// One boot under `budget`, its upper bounds asserted; returns the replay
/// steps that yielded under the budget.
fn step_under_budget(budget: u64) -> Vec<String> {
    use inf_server::{RecoverPhase, UnitPhase};
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
    let hand_over_unit = barrier.max(read_price);

    let mut ks = tiered_keyspace();
    let mut recovery = Recovery::new(fs.clone(), CELL, &cfg, anchor(), now());
    let (mut largest, mut settle_steps, mut open_span) = (0u64, 0u64, None);
    let mut early = Vec::new();
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
        match recovery.unit_row().phase {
            UnitPhase::Replay => {
                assert!(
                    read + charge < budget + record_unit,
                    "{at}: {read} read + {charge} charged passes the budget by more than one \
                     record's unit ({record_unit})"
                );
                let work = recovery.step_work_bytes();
                if recovery.phase() == RecoverPhase::Replay
                    && recovery.segments_progress().0 == segments_before
                    && work < budget
                {
                    early.push(format!("{at}: yielded at a work of {work}"));
                }
            }
            // The lift decision: replay has ended, and the end settle's
            // cursor starts at `ro`.
            UnitPhase::Finish if open_span.is_none() => {
                let space = ks.tiered_store(TIER_NS).expect("tiered").space();
                open_span = Some(space.tail().to_raw() - space.ro_boundary().to_raw());
            }
            UnitPhase::Settle => {
                settle_steps += 1;
                let violation = format!("{at}: SETTLE BUDGET VIOLATION");
                if recovery.next_unit_row().phase == UnitPhase::Settle {
                    assert!(
                        charge >= budget,
                        "{violation}: a step yielded at a charge of {charge}"
                    );
                }
                let bound = budget + settle_unit;
                assert!(charge < bound, "{violation}: a step charged {charge}, over {bound}");
            }
            UnitPhase::HandOver => {
                let bound = budget + hand_over_unit;
                assert!(
                    charge <= bound,
                    "{at}: a hand-over step's boot I/O of {charge} passes its budget and one \
                     unit ({bound})"
                );
            }
            UnitPhase::Finish
            | UnitPhase::Start
            | UnitPhase::Ick
            | UnitPhase::CkptEnd
            | UnitPhase::Audit
            | UnitPhase::Probe
            | UnitPhase::Resume
            | UnitPhase::Complete => {
                assert_eq!(
                    charge, 0,
                    "{at}: no boot I/O outside replay, the settle and the hand-over"
                );
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
    early
}

/// Where a power cut lands in a demoting boot (ADR-0174 D5).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Cut {
    /// Mid-replay, right after the first demote step made a boot tier file.
    AfterFirstDemote,
    /// Inside the end settle with records unsettled: a settle step yielded.
    InsideSettle,
    /// After the last machine's hand-over, before the log resumes.
    BeforeResume,
    /// After the resume, before the cell serves: the boot completed.
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
    use inf_server::{RecoverPhase, UnitPhase};
    let mut ks = tiered_keyspace();
    let mut recovery = Recovery::new(disk.clone(), CELL, cfg, anchor(), now());
    let mut settle_steps = 0u64;
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
                let settled = recovery.unit_row().phase == UnitPhase::Settle;
                settle_steps += u64::from(settled);
                settle_steps >= 2 && recovery.next_unit_row().phase == UnitPhase::Settle
            }
            Cut::BeforeResume => recovery.next_unit_row().phase == UnitPhase::Resume,
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
/// inside its end settle, between its hand-over and its log's resume, or
/// after the resume before the cell serves — once, or twice in a row,
/// leaves unmanifested tier files the next boot removes before any flush
/// (ADR-0174 D4, D5); the boot that completes
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
    let rows: [&[Cut]; 8] = [
        &[Cut::AfterFirstDemote],
        &[Cut::InsideSettle],
        &[Cut::BeforeResume],
        &[Cut::AfterHandOver],
        &[Cut::AfterFirstDemote, Cut::InsideSettle],
        &[Cut::InsideSettle, Cut::AfterHandOver],
        &[Cut::BeforeResume, Cut::InsideSettle],
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
    /// The units with no I/O of their own a step ran (the engine's unit
    /// counters, apart from its charge), each at the RAM price.
    ram_units: u64,
    /// The probe groups the ticket rebuild scanned (its unit counter), each
    /// at its bytes.
    group_units: u64,
}

/// The RAM units and the rebuild's probe groups counted between two
/// readings of a cell's stats.
fn units_since(now: inf_server::RecoverUnits, before: inf_server::RecoverUnits) -> (u64, u64) {
    let ram = (now.ckpt_entries - before.ckpt_entries)
        + (now.ckpt_end_releases - before.ckpt_end_releases)
        + (now.sweep_stamped - before.sweep_stamped)
        + (now.sweep_seeded - before.sweep_seeded);
    (ram, now.rebuild_groups - before.rebuild_groups)
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
            ram_units: self.ram_units,
            group_units: self.group_units,
        }
    }

    /// The work at the `limits` prices: a reader byte and a tier byte one
    /// budget byte each, a settle read at the store's read price, a
    /// barrier — a file's or a directory's — at the server's, a rename at
    /// the server's file-operation price, a RAM unit at the store's probe
    /// price and a probe group at the store's group price.
    fn priced(self) -> u64 {
        let barrier = inf_server::limits::REPLAY_BARRIER_CHARGE_BYTES;
        self.reader_bytes
            + self.tier_bytes_written * inf_server::limits::REPLAY_TIER_BYTE_CHARGE
            + self.tier_reads * inf_store::limits::SETTLE_READ_CHARGE_BYTES
            + (self.tier_syncs + self.dir_syncs) * barrier
            + self.renames * inf_server::limits::BOOT_FILE_OP_CHARGE_BYTES
            + self.ram_units * inf_store::limits::RECOVER_PROBE_CHARGE_BYTES
            + self.group_units * inf_store::limits::REBUILD_GROUP_CHARGE_BYTES
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
/// reader's one read chunk on a frame row; and its work gauge reads within
/// what the step did ([`gauge_over`]). The end settle's last step is judged
/// as every settle step is, at one unit: the hand-over and the resume are
/// steps of their own. Returns the steps over the bound, the recovery's
/// stats and the recovered keyspace; a step that refuses is the caller's
/// `Err`.
fn judged_boot(
    fs: &CountingFs,
    cfg: &DurableConfig,
    ks: Keyspace,
    budget: u64,
    unit_of: &dyn Fn(inf_server::UnitPhase) -> u64,
) -> (Vec<String>, std::io::Result<inf_server::RecoverStats>, Keyspace) {
    let (over, stats, ks, _) = judged_boot_logged(fs, cfg, ks, budget, unit_of);
    (over, stats, ks)
}

/// One step's row of `RECOVERY_UNITS`, the RAM units and probe groups it
/// ran and the renames its filesystem saw.
#[derive(Copy, Clone, Debug)]
struct StepUnits {
    phase: inf_server::UnitPhase,
    entries: u64,
    releases: u64,
    stamped: u64,
    seeded: u64,
    groups: u64,
    renames: u64,
}

/// [`judged_boot`], with every step's row and RAM units.
fn judged_boot_logged(
    fs: &CountingFs,
    cfg: &DurableConfig,
    mut ks: Keyspace,
    budget: u64,
    unit_of: &dyn Fn(inf_server::UnitPhase) -> u64,
) -> (Vec<String>, std::io::Result<inf_server::RecoverStats>, Keyspace, Vec<StepUnits>) {
    use inf_server::UnitEntry;
    let mut recovery = Recovery::new(fs.clone(), CELL, cfg, anchor(), now());
    let (mut over, mut log) = (Vec::new(), Vec::new());
    // Bound: every step makes progress (the zero-budget row).
    for step in 0u64.. {
        let (before, units) = (fs.tally(), recovery.stats().units);
        let yields_before = yields_in_unit(recovery.stats());
        let progress = match recovery.step(&mut ks, budget) {
            Ok(progress) => progress,
            Err(err) => return (over, Err(err), ks, log),
        };
        let mut work = fs.tally().since(before);
        let after = recovery.stats().units;
        (work.ram_units, work.group_units) = units_since(after, units);
        let row = recovery.unit_row();
        log.push(StepUnits {
            phase: row.phase,
            entries: after.ckpt_entries - units.ckpt_entries,
            releases: after.ckpt_end_releases - units.ckpt_end_releases,
            stamped: after.sweep_stamped - units.sweep_stamped,
            seeded: after.sweep_seeded - units.sweep_seeded,
            groups: after.rebuild_groups - units.rebuild_groups,
            renames: work.renames,
        });
        if row.budgeted {
            let chunk =
                if row.entry == UnitEntry::Frame { inf_log::DEFAULT_READ_CHUNK as u64 } else { 0 };
            let unit = unit_of(row.phase);
            let bound = budget.saturating_add(unit).saturating_add(chunk);
            if work.priced() > bound {
                over.push(format!(
                    "step {step} ({:?}): {} budget bytes of work over the bound {bound} \
                     ({work:?})",
                    row.phase,
                    work.priced()
                ));
            }
            let gauge = Gauge {
                work: recovery.step_work_bytes(),
                boot_charge: recovery.step_charge_bytes(),
                reader_bytes: work.reader_bytes,
                ram_bytes: work.ram_units * inf_store::limits::RECOVER_PROBE_CHARGE_BYTES,
                parked: yields_in_unit(recovery.stats()) > yields_before,
            };
            if let Some(reading) = gauge_over(row.entry, budget, unit, gauge) {
                over.push(format!("step {step} ({:?}): {reading}", row.phase));
            }
        }
        if progress == RecoveryProgress::Complete {
            break;
        }
    }
    let (_rotor, stats, _seed) = recovery.finish();
    (over, Ok(stats), ks, log)
}

/// One step's work gauge (`Recovery::step_work_bytes`), its boot I/O charge
/// (`Recovery::step_charge_bytes`), the reader bytes its filesystem
/// returned, its RAM units at their price, and whether it yielded inside a
/// unit.
#[derive(Copy, Clone, Debug)]
struct Gauge {
    work: u64,
    boot_charge: u64,
    reader_bytes: u64,
    ram_bytes: u64,
    parked: bool,
}

/// The yields inside a unit a cell's steps made so far, every kind.
fn yields_in_unit(stats: &inf_server::RecoverStats) -> u64 {
    let step = stats.step;
    step.yields_in_frame + step.yields_in_section + step.yields_in_demote_step
}

/// The slack of a step with no boot I/O on a row with no frame, in bytes:
/// one item that crosses the budget (every unit judged here holds items
/// under a block: 80-byte records, entries of at most 22 bytes at the
/// 64-byte RAM price), the framing a finished section charges at its end
/// (its header, CRC and padding) and the padding of the section the step
/// parked in, which the walk reads when it hops past it.
const GAUGE_SLACK: u64 = 3 * inf_log::ckpt::ICK_BLOCK_ALIGN as u64;

/// The work gauge against what the step did (ADR-0174 A1.2): a step's work
/// is what it charged plus the bytes of the frame or section it entered and
/// parked in, never their price. On a frame row the gauge is within `B + U`
/// (B1), and on a row that enters no unit — nothing parks there — it is the
/// step's charge, within the same. On a section row a step parks only in a
/// section it entered, whose block it read at its entry, so the gauge is
/// within `B`, the bytes the step read and [`GAUGE_SLACK`] where the step
/// charged no boot I/O, or `U` in its place where it did. `Some` names the
/// over-read. Red where a walk parks an entry section's price
/// (`inf_canary_recover_parked_priced`).
///
/// The lower side: a step that read a frame or a section and yielded inside
/// it adds the bytes it left to its charge, so its gauge is at least what it
/// read and its RAM units at their price, less [`GAUGE_SLACK`] on a section
/// row (the parked section's padding, read when its walk hops past it) or
/// the reader's one read chunk on a frame row (a refill may read past the
/// frame). Tier I/O stays out of the floor: the engine prices a barrier per
/// seal, not each directory sync its filesystem saw. Red where a park adds
/// nothing of its unit (`inf_canary_recover_parked_dropped`).
fn gauge_over(
    entry: inf_server::UnitEntry,
    budget: u64,
    unit: u64,
    gauge: Gauge,
) -> Option<String> {
    if let Some(reading) = gauge_under(entry, gauge) {
        return Some(reading);
    }
    let bound = match entry {
        inf_server::UnitEntry::Frame | inf_server::UnitEntry::None => budget.saturating_add(unit),
        inf_server::UnitEntry::Section => {
            let slack = if gauge.boot_charge == 0 { GAUGE_SLACK } else { unit };
            budget.saturating_add(gauge.reader_bytes).saturating_add(slack)
        }
    };
    (gauge.work > bound)
        .then(|| format!("GAUGE: a work gauge of {} over {bound} ({gauge:?})", gauge.work))
}

/// [`gauge_over`]'s lower side: `Some` names a step that read a unit it
/// yielded inside and whose gauge reads under what it did.
fn gauge_under(entry: inf_server::UnitEntry, gauge: Gauge) -> Option<String> {
    let slack = match entry {
        inf_server::UnitEntry::Frame => inf_log::DEFAULT_READ_CHUNK as u64,
        inf_server::UnitEntry::Section => GAUGE_SLACK,
        inf_server::UnitEntry::None => return None,
    };
    let floor = (gauge.reader_bytes + gauge.ram_bytes).saturating_sub(slack);
    (gauge.parked && gauge.reader_bytes > 0 && gauge.work < floor).then(|| {
        format!("GAUGE: a work gauge of {} under the step's reads, {floor} ({gauge:?})", gauge.work)
    })
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

/// `U` of a hand-over step, from the `limits` and never from the engine:
/// the largest of a parked death stamped or an extent id seeded (the RAM
/// unit price), a revive rename (the file-operation price), a probe group
/// the rebuild scans (its bytes), a slot it returns (its settle read), and
/// one machine's hand-over — its drain's seal of the active file, a barrier,
/// with the footer block it writes.
fn hand_over_unit() -> u64 {
    let seal = inf_server::limits::REPLAY_BARRIER_CHARGE_BYTES + (64 << 10);
    [
        inf_store::limits::RECOVER_PROBE_CHARGE_BYTES,
        inf_server::limits::BOOT_FILE_OP_CHARGE_BYTES,
        inf_store::limits::REBUILD_GROUP_CHARGE_BYTES,
        inf_store::limits::SETTLE_READ_CHARGE_BYTES,
        seal,
    ]
    .into_iter()
    .max()
    .expect("five units")
}

/// `U` for the hostile units: a frame's entry plus one record's unit on the
/// replay row, a record settled against its twin on the end settle's, one
/// hand-over unit on the hand-over's.
fn hostile_unit(frame_entry: u64) -> impl Fn(inf_server::UnitPhase) -> u64 {
    let record = (TieredTable::RECORD_HEADER_LEN + key(0).len() + SMALL_VALUE_LEN) as u64;
    move |phase| match phase {
        inf_server::UnitPhase::Replay => frame_entry + replay_record_unit(record),
        inf_server::UnitPhase::Settle => record + inf_store::limits::SETTLE_READ_CHARGE_BYTES,
        inf_server::UnitPhase::HandOver => hand_over_unit(),
        other => panic!("the hostile units judge no {other:?} step"),
    }
}

/// One 4 MiB frame of 80-byte rewrites of keys the
/// boot has demoted, replayed at a 256 KiB budget under the step judge
/// (`U` the frame's entry plus one record's unit). Every judged
/// step's work is within `B + U + C`, read before the engagement clause,
/// and the boot recovers the model. Red where the frame's demote steps run
/// inside the step that enters it (`inf_canary_recover_frame_whole`),
/// where a demote step's seal walk ignores the budget left
/// (`inf_canary_recover_seal_unbudgeted`), and where the step that parks in
/// the frame adds nothing of it to its work gauge
/// (`inf_canary_recover_parked_dropped`: the gauge under the step's reads).
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
/// extent reference map, the reclaim queue in its hand-out order (the
/// order the boot's deaths reached it), the ticket rebuild's counters, the
/// D6 counters (the gauges and the step facts excepted), the record counts
/// and the hand-over's rebuild reads and revive renames.
#[derive(Debug, PartialEq, Eq)]
struct Recovered {
    table: inf_store::StateDigest,
    files: Vec<(String, Vec<u8>)>,
    served: Model,
    extents: Vec<(u64, u64, u64)>,
    reclaim_order: Vec<u64>,
    shadow: inf_store::ShadowCounters,
    counters: inf_store::ReplayCounters,
    dead_life_files_removed: u64,
    records_applied: u64,
    records_pre_begin: u64,
    shadow_settle_reads: u64,
    blob_revived: u64,
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
    // Bound: every step makes progress; a boot of this unit at a budget of
    // 0 takes well under a million steps, so a walk that re-applies records
    // without moving on is caught here, not as a hang.
    let mut steps = 0u64;
    let refused = |err| panic!("BUDGET INDEPENDENCE: the boot at {budget} refused: {err}");
    while recovery.step(&mut ks, budget).unwrap_or_else(refused) == RecoveryProgress::Working {
        steps += 1;
        assert!(steps < 5_000_000, "BUDGET INDEPENDENCE: no progress at {budget}");
    }
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
    let table = ks.tiered_store_mut(TIER_NS).expect("tiered");
    let digest = table.simulation_digest();
    let extents = table.extent_references().collect();
    let shadow = table.shadow_counters();
    // Read last: handing the queue out moves its candidates into reclaim.
    let queue = table.extent_reclaim_work(u64::MAX, usize::MAX);
    Recovered {
        table: digest,
        files,
        served,
        extents,
        reclaim_order: queue.iter().map(|candidate| candidate.extent_id).collect(),
        shadow,
        counters: stats.tier_replay.counters,
        dead_life_files_removed: stats.tier_replay.dead_life_files_removed,
        records_applied: stats.records_applied,
        records_pre_begin: stats.records_pre_begin,
        shadow_settle_reads: stats.shadow_settle_reads,
        blob_revived: stats.blob_revived,
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
    let differs = first_difference(&state.extents, &control.extents);
    assert!(differs.is_none(), "{at}: the extent references differ from entry {differs:?}");
    let differs = first_difference(&state.reclaim_order, &control.reclaim_order);
    assert!(differs.is_none(), "{at}: the reclaim queue's order differs from {differs:?}");
    assert_eq!(state.shadow, control.shadow, "{at}: the ticket rebuild's counters");
    assert_eq!(state.counters, control.counters, "{at}: the D6 counters");
    assert_eq!(state.shadow_settle_reads, control.shadow_settle_reads, "{at}: rebuild reads");
    assert_eq!(state.blob_revived, control.blob_revived, "{at}: revive renames");
    assert_eq!(state.dead_life_files_removed, control.dead_life_files_removed, "{at}");
    assert_eq!(state.records_applied, control.records_applied, "{at}: records applied");
    assert_eq!(state.records_pre_begin, control.records_pre_begin, "{at}: records below begin");
}

/// The first index at which two lists differ (the shorter one's length
/// where one is the other's prefix), `None` when they are equal.
fn first_difference<T: PartialEq>(state: &[T], control: &[T]) -> Option<usize> {
    let differs = state.iter().zip(control).position(|(a, b)| a != b);
    differs.or_else(|| (state.len() != control.len()).then(|| state.len().min(control.len())))
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
            IckStep::Parked => panic!("a walk that consumes every item never parks"),
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

/// A tiered checkpoint unit at size: a first life writes `keys` distinct
/// 80-byte records through the live table and its flush pipeline at the
/// smallest window, so all but about its last window are cold in
/// manifested tier files; then a checkpoint — one section a class — whose
/// ref section names every cold key, whose records section re-images every
/// key with a new value (what a walk writes when every cold key is
/// overwritten between its two passes, ADR-0174 R8's shape) and, with
/// `blob_refs`, whose blob-reference section names an extent at every
/// ref's address. Its images fill the window, so the boot demotes inside
/// the records section and each sealed image settles against its key's ref
/// (one settle read each), listing the ref for the end of the checkpoint's
/// releases (R9). The MANIFEST names the checkpoint and the tier section
/// over a log holding its begin marker.
struct CkptUnit {
    model: Model,
    /// The refs the checkpoint holds: the first life's cold keys.
    refs: u64,
    /// The checkpoint's largest section, in block bytes.
    section_bytes_max: u64,
}

fn write_ckpt_unit<F: SegmentFs + Clone>(
    fs: &F,
    cfg: &DurableConfig,
    keys: u64,
    blob_refs: bool,
) -> CkptUnit {
    let mut log = Log::new(fs, cfg);
    let begin = log.begin_marker(1);
    drop(log);
    make_durable(fs, cfg);
    let life = first_life(fs, cfg, keys);
    let refs = life.refs.len() as u64;
    let images: Vec<(Vec<u8>, Vec<u8>)> = (0..keys).map(|i| (key(i), small_value(i, 1))).collect();
    let model = images.iter().map(|(key, value)| (key.clone(), Some(value.clone()))).collect();
    let ick = publish_tiered_checkpoint(fs, cfg, life, begin, &images, blob_refs);
    CkptUnit { model, refs, section_bytes_max: largest_section(fs, &ick) }
}

/// A first life at the smallest window: `keys` distinct 80-byte records
/// written through the live table and its flush pipeline, so all but about
/// its last window are cold in manifested tier files, and a checkpoint walk
/// begun over it — its watermark and the refs it emitted, `(hash, address)`
/// of each cold slot.
struct FirstLife<F: SegmentFs> {
    table: TieredTable,
    flush: inf_log::TierFlush<F>,
    hasher: inf_store::KeyHasher,
    walk: u64,
    refs: Vec<(u64, u64)>,
}

fn first_life<F: SegmentFs + Clone>(fs: &F, cfg: &DurableConfig, keys: u64) -> FirstLife<F> {
    let shard = cfg.data_dir.join(format!("shard-{CELL}"));
    let spec = TierSpec::for_budget(MEM_BUDGET);
    let demote = spec.demotion_config();
    let space = inf_store::AddressSpaceConfig {
        reserve_bytes: demote.ring_reserve_bytes().expect("the ring"),
        page_bytes: inf_alloc::REGION_PAGE_BYTES,
        life_origin: inf_store::LogicalAddr::ZERO,
    };
    let hasher = inf_store::KeyHasher::default();
    let mut table = TieredTable::new(space, demote, 4096, hasher).expect("the table");
    let census = inf_foundation::bounded::CapCensus::new(inf_foundation::CellId(CELL)).0;
    let flush_cfg = inf_log::TierFlushConfig {
        shard_dir: shard.join(format!("ns-{}", TIER_NS.0)),
        cell: u32::from(CELL),
        ns: TIER_NS,
        mode: spec.tier_io_mode,
        file_capacity: inf_log::TIER_FILE_CAPACITY_DEFAULT,
        slice_bytes: spec.maintain_slice_bytes,
        record_len_max: spec.record_len_max(),
    };
    let mut flush = inf_log::TierFlush::new(fs.clone(), flush_cfg, 0, &census);
    let maintain = |table: &mut TieredTable, flush: &mut inf_log::TierFlush<F>| loop {
        let sealed = table.seal_slice();
        let flushed = table.flush_slice(flush).expect("flush slice");
        let released = table.release_slice();
        if sealed + released + flushed.appended_bytes + u64::from(flushed.gaps_crossed) == 0 {
            break;
        }
    };
    for i in 0..keys {
        let (key, value) = (key(i), small_value(i, 0));
        let hash = hasher.hash(&key);
        let mut rounds = 0;
        // Bound: the live writer parks until MAINTAIN frees the window.
        while table.insert(&key, &value, hash).is_err() {
            maintain(&mut table, &mut flush);
            rounds += 1;
            assert!(rounds < 64, "MAINTAIN frees the window");
        }
    }
    maintain(&mut table, &mut flush);
    let walk = table.begin_ckpt_walk(1).to_raw();
    let mut refs: Vec<(u64, u64)> = Vec::new();
    let mut cursor = 0u64;
    // Bound: the walk's slices over the index.
    loop {
        cursor =
            table.ckpt_walk_slice(cursor, 4096, |hash, at| refs.push((hash, at.to_raw())), |_| {});
        if cursor == 0 {
            break;
        }
    }
    FirstLife { table, flush, hasher, walk, refs }
}

/// Publishes the checkpoint of `life`'s walk at `begin` — one section a
/// class: its refs, then `images`, its live set and, with `blob_refs`, a
/// blob-reference entry at every ref's address — and the MANIFEST that names
/// it and the namespace's tier section; returns the checkpoint's path.
fn publish_tiered_checkpoint<F: SegmentFs + Clone>(
    fs: &F,
    cfg: &DurableConfig,
    mut life: FirstLife<F>,
    begin: inf_log::Lsn,
    images: &[(Vec<u8>, Vec<u8>)],
    blob_refs: bool,
) -> PathBuf {
    use inf_log::ckpt::SyncIckWriter;
    let shard = cfg.data_dir.join(format!("shard-{CELL}"));
    let ckpt = inf_log::CkptConfig { section_bytes: 32 << 20, ..cfg.ckpt };
    let mut writer = SyncIckWriter::create_v3(
        fs.clone(),
        &shard.join("ckpt"),
        &ckpt,
        CELL,
        1,
        begin,
        &[TIER_NS.0],
    )
    .expect("ick create");
    for &(hash, at) in &life.refs {
        writer.append_ref(TIER_NS.0, life.walk, hash, at).expect("ref");
    }
    for (key, value) in images {
        let image = inf_log::RecordView::StringPostImage { ns: TIER_NS, key, value };
        writer.append(&image).expect("image");
    }
    for file in life.table.live_set().files().to_vec() {
        let (id, len, dead, exact) = (file.id, file.data_len, file.dead_bytes, file.byte_exact);
        writer.append_live_set(TIER_NS.0, id, len, dead, exact).expect("live set");
    }
    if blob_refs {
        let mut addrs: Vec<u64> = life.refs.iter().map(|&(_, at)| at).collect();
        addrs.sort_unstable();
        for (extent_id, at) in (1u64..).zip(addrs) {
            writer.append_blob_ref(TIER_NS.0, at, extent_id, 4096).expect("blob ref");
        }
    }
    writer.finish().expect("ick publish");
    life.table.end_ckpt_walk();
    let section = life.table.tier_manifest(TIER_NS.0, &life.flush);
    let manifest = inf_log::Manifest {
        ckpt_id: 1,
        begin_lsn: begin,
        segments: vec![begin.segment],
        tiers: vec![section],
        key_hash_id: life.hasher.identity(),
    };
    inf_log::write_manifest(fs, &shard, &manifest).expect("manifest");
    shard.join("ckpt").join(inf_log::ckpt::ick_file_name(1))
}

/// The largest section of the checkpoint at `ick`, in block bytes, by its
/// reader.
fn largest_section<F: SegmentFs>(fs: &F, ick: &std::path::Path) -> u64 {
    use inf_log::ckpt::{IckReader, IckReaderConfig, IckStep};
    let mut reader = IckReader::open(fs, ick, IckReaderConfig::default()).expect("open the ick");
    let mut largest = 0u64;
    // Bound: the checkpoint's sections.
    loop {
        let step = reader
            .next_step_hybrid(|_| Ok::<(), ()>(()), |_| Ok(()), |_| Ok(()), |_| Ok(()), |_| Ok(()))
            .expect("a section");
        match step {
            IckStep::Section { bytes } => largest = largest.max(bytes),
            IckStep::Parked => panic!("a walk that consumes every item never parks"),
            IckStep::Done(_) => return largest,
        }
    }
}

/// `U` for the checkpoint units: on the checkpoint row its largest section
/// plus one record's unit; one listed address at the RAM price on the end
/// of the checkpoint's; a frame's entry (the begin marker's) and one
/// record's unit on the replay row; a record settled against its twin on
/// the end settle's; one hand-over unit on the hand-over's.
fn ckpt_unit(section_bytes_max: u64) -> impl Fn(inf_server::UnitPhase) -> u64 {
    let record = (TieredTable::RECORD_HEADER_LEN + key(0).len() + SMALL_VALUE_LEN) as u64;
    move |phase| match phase {
        inf_server::UnitPhase::Ick => section_bytes_max + replay_record_unit(record),
        inf_server::UnitPhase::CkptEnd => inf_store::limits::RECOVER_PROBE_CHARGE_BYTES,
        inf_server::UnitPhase::Replay => (64 << 10) + replay_record_unit(record),
        inf_server::UnitPhase::Settle => record + inf_store::limits::SETTLE_READ_CHARGE_BYTES,
        inf_server::UnitPhase::HandOver => hand_over_unit(),
        other => panic!("the checkpoint units judge no {other:?} step"),
    }
}

/// One records section of 8.4 MiB of 80-byte images, each of a key the
/// checkpoint also holds a ref for, loaded at a 256 KiB budget under the
/// step judge (`U` the section's entry plus one record's unit): the
/// section's demote steps settle every sealed image against its ref. Every
/// judged step's work is within `B + U`, read before the engagement clause;
/// the boot yields inside the section and inside its demote steps and
/// recovers the model. Red where a section's walk never parks and its
/// demote steps run to their end inside the step that enters it
/// (`inf_canary_recover_section_whole`).
#[test]
fn a_records_section_of_rewrites_of_cold_keys_is_loaded_under_the_step_judge() {
    const BUDGET: u64 = 256 << 10;
    let fs = CountingFs::new();
    let cfg = step_cfg();
    let unit = write_ckpt_unit(&fs, &cfg, 105_000, false);
    let (over, stats, mut ks) =
        judged_boot(&fs, &cfg, tiered_keyspace(), BUDGET, &ckpt_unit(unit.section_bytes_max));
    let stats = stats.expect("the checkpoint boots");
    let replay = stats.tier_replay.counters;
    eprintln!(
        "records section {} bytes at {BUDGET}: {} steps over the judge, {} refs, {} demote \
         steps, {} settle reads, {:?}, {:?}",
        unit.section_bytes_max,
        over.len(),
        unit.refs,
        replay.demote_steps,
        replay.settle_reads,
        stats.step,
        stats.units
    );
    assert!(over.is_empty(), "JUDGE: {} steps over B + U: {:?}", over.len(), over.first());
    assert!(stats.step.yields_in_section > 0, "VACUOUS: no yield in a section ({:?})", stats.step);
    assert!(stats.step.yields_in_demote_step > 0, "VACUOUS: no demote step yielded");
    assert!(replay.settle_reads > 10_000, "VACUOUS: {} settle reads", replay.settle_reads);
    let (served, _, _) = read_back(&fs, &cfg, &mut ks, &unit.model);
    assert_model(&served, &unit.model, "after the records section");
}

/// The entry sections and the end of the checkpoint at size, at a 256 KiB
/// budget: a ref section and a blob-reference section of one entry per
/// cold key, applied a batch a step at their price (an entry's bytes and
/// the RAM unit price), and the end of the checkpoint's list of the refs
/// the image load settled, released a batch a step at the RAM price, in
/// settle order. Every judged step's work — its reader bytes, its I/O and
/// its RAM units at their prices — is within `B + U`, and the work gauge of
/// a step that parks in an entry section adds the section's bytes it left,
/// never their price (the judge's `GAUGE`); the checkpoint's
/// entries are all applied, the boot yields inside a section, and the list
/// it walks is longer than one step buys; and no step applies or releases
/// more than its budget buys at their price, plus one. Red where an entry
/// section applies every entry in the step that enters it
/// (`inf_canary_recover_section_whole`: `ENTRIES`), where the end of the
/// checkpoint's releases charge nothing (`inf_canary_recover_probe_uncharged`:
/// `RELEASES`), where a parked walk adds the price of the entries it left
/// to the gauge (`inf_canary_recover_parked_priced`: `GAUGE`), and where it
/// adds nothing of the section (`inf_canary_recover_parked_dropped`: the
/// gauge under the step's reads).
#[test]
fn entry_sections_and_the_end_of_the_checkpoint_are_applied_under_the_step_judge() {
    const BUDGET: u64 = 256 << 10;
    let fs = CountingFs::new();
    let cfg = step_cfg();
    let unit = write_ckpt_unit(&fs, &cfg, 105_000, true);
    let (over, stats, mut ks, log) = judged_boot_logged(
        &fs,
        &cfg,
        tiered_keyspace(),
        BUDGET,
        &ckpt_unit(unit.section_bytes_max),
    );
    let stats = stats.expect("the checkpoint boots");
    let units = stats.units;
    // Entries and releases a step within the price bound: what its budget
    // buys at their price, plus one (a ref is 14 bytes, a blob-reference
    // entry 22, each with the RAM unit price).
    let price = inf_store::limits::RECOVER_PROBE_CHARGE_BYTES;
    for (step, row) in log.iter().enumerate() {
        assert!(
            row.entries * (14 + price) <= BUDGET + 22 + price,
            "ENTRIES: step {step} ({:?}) applied {} entries",
            row.phase,
            row.entries
        );
        assert!(
            row.releases * price <= BUDGET + price,
            "RELEASES: step {step} ({:?}) released {} listed addresses",
            row.phase,
            row.releases
        );
    }
    eprintln!(
        "entry sections at {BUDGET}: {} steps over the judge, {} refs, {units:?}, {:?}, {:?}",
        over.len(),
        unit.refs,
        stats.step,
        stats.tier_replay.counters
    );
    assert!(over.is_empty(), "JUDGE: {} steps over B + U: {:?}", over.len(), over.first());
    assert!(unit.refs > 4 * BUDGET / 14, "VACUOUS: {} refs, one step's worth", unit.refs);
    assert!(units.ckpt_entries >= 2 * unit.refs, "every ref and blob entry applied: {units:?}");
    assert!(
        units.ckpt_end_releases > BUDGET / inf_store::limits::RECOVER_PROBE_CHARGE_BYTES + 1,
        "VACUOUS: {} listed addresses, one step's worth",
        units.ckpt_end_releases
    );
    assert!(stats.step.yields_in_section > 0, "VACUOUS: no yield in a section ({:?})", stats.step);
    let released = stats.tier_replay.counters.blob_releases;
    assert!(released >= units.ckpt_end_releases, "each listed entry released: {released}");
    let (served, _, _) = read_back(&fs, &cfg, &mut ks, &unit.model);
    assert_model(&served, &unit.model, "after the entry sections");
}

/// Budget independence over the checkpoint: the same unit — a records
/// section over the window of rewrites of cold keys, its refs, and a
/// blob-reference entry at each ref — recovered at `u64::MAX` (the
/// control, which never parks) and at 8 MiB, 256 KiB, 4 KiB, 1 and 0
/// recovers the same table, tier files, served values, extent reference
/// map, reclaim queue order — the end of the checkpoint's deaths in their
/// settle order, which the 256 KiB and 4 KiB legs release in batches that
/// resume inside the list — D6 counters (the blob references released among
/// them) and record counts. Red where a resumed section walk passes a
/// record that needs room (`inf_canary_recover_skip_on_resume`'s section
/// arm) and where a release call that resumes inside the list releases its
/// batch in reverse (`inf_canary_replay_release_reordered`: the reclaim
/// queue's order).
#[test]
fn every_budget_recovers_the_checkpoint_of_the_unparked_boot() {
    let cfg = step_cfg();
    // Each boot recovers its own copy of the unit, written the same way: a
    // boot's tier files are the next boot's dead-life files (D4).
    let boot = |budget: u64| {
        let fs = CountingFs::new();
        let unit = write_ckpt_unit(&fs, &cfg, 60_000, true);
        let state = recovered(&fs, &cfg, budget, &unit.model);
        (unit.model, state)
    };
    let (model, control) = boot(u64::MAX);
    assert_model(&control.served, &model, "the control boot");
    assert!(control.counters.demote_steps > 0, "VACUOUS: {:?}", control.counters);
    assert!(control.counters.blob_releases > 0, "VACUOUS: {:?}", control.counters);
    // The order is observable where budgets differ: more deaths queued than
    // one 4 KiB step releases, so that leg's batches resume inside the list.
    let batch = (4 << 10) / inf_store::limits::RECOVER_PROBE_CHARGE_BYTES;
    let deaths = control.reclaim_order.len() as u64;
    eprintln!(
        "checkpoint control: {deaths} deaths queued, {} extent references, {:?}",
        control.extents.len(),
        control.counters
    );
    assert!(deaths > 2 * batch, "VACUOUS: {deaths} deaths in the reclaim queue");
    for budget in [8 << 20, 256 << 10, 4 << 10, 1, 0] {
        let (model, state) = boot(budget);
        assert_model(&state.served, &model, &format!("BUDGET INDEPENDENCE at {budget}"));
        assert_same(&state, &control, budget);
    }
}

/// An index sidecar of more sections than one step's budget, at the tail
/// of a checkpoint (the writer streams derived data last), loaded at a
/// 1 MiB budget under the step judge: each sidecar section is applied whole
/// and the step asks its budget after it, so every step's work is within
/// its budget and one section, and the run takes about its bytes over the
/// budget in steps. Red where a sidecar section is not asked after
/// (`inf_canary_recover_section_whole`: the whole run in one step).
#[cfg(feature = "doc")]
#[test]
fn a_run_of_index_sidecar_sections_yields_after_each_section() {
    const BUDGET: u64 = 1 << 20;
    let fs = CountingFs::new();
    let cfg = cfg();
    let mut log = LogBuilder::new(&fs.inner, &cfg);
    let (_, lsns) = log.frame(&[MutationEffect::CkptBegin { ckpt_id: 1 }]);
    drop(log);
    publish_checkpoint(&fs.inner, &cfg, lsns[0], &[], 400_000);
    let ick = cfg.data_dir.join(format!("shard-{CELL}/ckpt")).join(inf_log::ckpt::ick_file_name(1));
    let (sections, sidecar_sections) = ick_sections(&fs.inner, &ick);
    assert!(sidecar_sections >= 16, "VACUOUS: {sidecar_sections} sidecar sections");
    let section_max = largest_section(&fs.inner, &ick);
    // A memory namespace's boot settles and hands over nothing.
    let unit = move |phase| match phase {
        inf_server::UnitPhase::Ick => section_max,
        inf_server::UnitPhase::Replay => 64 << 10,
        inf_server::UnitPhase::Settle | inf_server::UnitPhase::HandOver => 0,
        other => panic!("the sidecar unit judges no {other:?} step"),
    };
    let (over, stats, _ks) = judged_boot(&fs, &cfg, fresh_keyspace(), BUDGET, &unit);
    let stats = stats.expect("the sidecar boots");
    let steps = stats.step.steps;
    eprintln!("sidecar run: {sidecar_sections} of {sections} sections, {steps} steps");
    assert!(over.is_empty(), "JUDGE: {} steps over B + U: {:?}", over.len(), over.first());
    let run = sidecar_sections * section_max;
    assert!(steps > run / (BUDGET + section_max), "VACUOUS: {steps} steps for a {run}-byte run");
}

// ---- the end of a boot: the hand-over's sweep seed, renames and rebuild ----

/// Keys of the end unit's first life; about two-thirds of them go cold.
const END_KEYS: u64 = 60_000;
/// Cold keys the end unit's tail rewrites: pairs the ticket rebuild forms,
/// the ones past its cap returned to settle.
const END_REWRITES: usize = 6_000;
/// Extent references the tail writes and deletes: replay deaths parked
/// until the sweep seed stamps them.
const END_PARKED: u64 = 12_000;
/// Listed extent files no record names: orphans the seed queues.
const END_ORPHANS: u64 = 12_000;
/// Live extent references whose files are quarantined (renamed back), and
/// as many quarantined files no record names (second verdicts).
const END_REVIVED: u64 = 64;

/// The end phases' recovery unit: a first life of [`END_KEYS`] records and a
/// checkpoint of its refs and live set, with no image; then a tail that
/// rewrites [`END_REWRITES`] of the cold keys — each a RAM record beside its
/// cold twin, a pair the ticket rebuild forms, and past its cap one it hands
/// back to read and settle — writes [`END_PARKED`] extent references and
/// deletes them (replay deaths the sweep seed stamps), and writes
/// [`END_REVIVED`] live ones; and in the namespace's directory a blob file for
/// every dead extent and [`END_ORPHANS`] more no record names (listed ids the
/// seed queues), a quarantine twin for every live extent (renamed back) and
/// [`END_REVIVED`] more no record names. The tail fits the window, so the boot
/// demotes nothing and its end settle walks nothing: every unit of the end of
/// the boot is the hand-over's.
struct EndUnit {
    /// The cold keys' values; the first life's RAM keys have no image.
    model: Model,
    /// The extent ids the boot lists, plain and quarantined.
    listed: u64,
    quarantined: u64,
}

fn write_end_unit<F: SegmentFs + Clone>(fs: &F, cfg: &DurableConfig) -> EndUnit {
    let mut log = Log::new(fs, cfg);
    let begin = log.begin_marker(1);
    let life = first_life(fs, cfg, END_KEYS);
    let cold: Vec<u64> = (0..END_KEYS)
        .filter(|&i| {
            let key = key(i);
            matches!(life.table.lookup(&key, life.hasher.hash(&key), &[]), TieredLookup::Cold(_))
        })
        .collect();
    assert!(cold.len() > 2 * END_REWRITES, "VACUOUS: {} cold keys", cold.len());
    let mut model: Model = cold.iter().map(|&i| (key(i), Some(small_value(i, 0)))).collect();
    let rewrites: Vec<(Vec<u8>, Vec<u8>)> =
        cold[..END_REWRITES].iter().map(|&i| (key(i), small_value(i, 1))).collect();
    for chunk in rewrites.chunks(512) {
        let effects: Vec<MutationEffect<'_>> = chunk
            .iter()
            .map(|(key, value)| MutationEffect::StringSet { ns: TIER_NS, key, value })
            .collect();
        log.frame(&effects);
    }
    model.extend(rewrites.into_iter().map(|(key, value)| (key, Some(value))));
    let dead: Vec<(Vec<u8>, u64)> =
        (1..=END_PARKED).map(|id| (format!("x:{id:06}").into_bytes(), id)).collect();
    let live: Vec<(Vec<u8>, u64)> =
        (1..=END_REVIVED).map(|j| (format!("q:{j:04}").into_bytes(), END_PARKED + j)).collect();
    for chunk in dead.chunks(512).chain(live.chunks(512)) {
        let effects: Vec<MutationEffect<'_>> = chunk
            .iter()
            .map(|(key, id)| MutationEffect::StringSetExtent {
                ns: TIER_NS,
                key,
                extent_id: *id,
                offset: 0,
                len: 4096,
            })
            .collect();
        log.frame(&effects);
    }
    for chunk in dead.chunks(512) {
        let effects: Vec<MutationEffect<'_>> =
            chunk.iter().map(|(key, _)| MutationEffect::Delete { ns: TIER_NS, key }).collect();
        log.frame(&effects);
    }
    drop(log);
    make_durable(fs, cfg);
    publish_tiered_checkpoint(fs, cfg, life, begin, &[], false);
    let cold_dir = cold_dir(cfg);
    let file = |name: String| {
        let mut file = fs.create_meta(&cold_dir.join(name)).expect("an extent file");
        file.write_at(0, b"extent").expect("its bytes");
        file.sync_data().expect("its barrier");
    };
    let orphans = END_PARKED + END_REVIVED;
    let (plain, quarantined) = (inf_log::extent_file_name, inf_log::quarantined_file_name);
    for id in (1..=END_PARKED).chain(orphans + 1..=orphans + END_ORPHANS) {
        file(plain(inf_log::ExtentId(id)));
    }
    let unreferenced = orphans + END_ORPHANS;
    for id in (END_PARKED + 1..=orphans).chain(unreferenced + 1..=unreferenced + END_REVIVED) {
        file(quarantined(inf_log::ExtentId(id)));
    }
    fs.sync_dir(&cold_dir).expect("the directory's barrier");
    EndUnit { model, listed: END_PARKED + END_ORPHANS, quarantined: 2 * END_REVIVED }
}

/// `U` for the end unit: a frame's entry and one record's unit on the
/// replay row, the checkpoint's largest section and one record's unit on its
/// row, a record settled against its twin on the end settle's, one hand-over
/// unit on the hand-over's.
fn end_unit_of(section_bytes_max: u64) -> impl Fn(inf_server::UnitPhase) -> u64 {
    let record = (TieredTable::RECORD_HEADER_LEN + key(0).len() + SMALL_VALUE_LEN) as u64;
    move |phase| match phase {
        inf_server::UnitPhase::Replay => FRAME_BYTES_MAX + replay_record_unit(record),
        inf_server::UnitPhase::Ick => section_bytes_max + replay_record_unit(record),
        inf_server::UnitPhase::Settle => record + inf_store::limits::SETTLE_READ_CHARGE_BYTES,
        inf_server::UnitPhase::HandOver => hand_over_unit(),
        other => panic!("the end unit judges no {other:?} step"),
    }
}

/// The units a hand-over step ran past what its budget buys at their
/// prices, plus one: `None` when within. `kind` names the oracle that sees
/// it, the witness of its planted red.
fn over_price(kind: &str, step: usize, units: u64, price: u64, budget: u64) -> Option<String> {
    (units * price > budget + price).then(|| format!("{kind}: step {step} ran {units} at {price}"))
}

/// The end of a boot at size (the end-phase row, at a size the suite runs),
/// at a 256 KiB budget under the step judge (`U` one hand-over unit): the
/// hand-over's sweep seed stamps 12 000 parked replay deaths and seeds
/// 12 000 listed ids, 12 000 orphans among them, and 128 quarantined ones; 64
/// quarantined extents are renamed back; the ticket rebuild scans every probe
/// group of the index and hands back the 1 904 pairs past its cap, each read
/// and settled. Every judged step's work is within `B + U`, read before the
/// engagement clause; no hand-over step stamps or seeds more ids than its
/// budget buys at the RAM price, renames more extents than it buys at the
/// file-operation price, or scans more probe groups than it buys at their
/// bytes, plus one (each reported, the oracle's name first); and the boot
/// recovers the model and the live extents. Red where the hand-over's units
/// charge nothing (`inf_canary_recover_probe_uncharged`: `STAMPS`, `SEEDS`,
/// `RENAMES`, `GROUPS`).
#[test]
fn the_end_of_a_boot_at_size_is_handed_over_under_the_step_judge() {
    const BUDGET: u64 = 256 << 10;
    let fs = CountingFs::new();
    let cfg = step_cfg();
    let unit = write_end_unit(&fs, &cfg);
    let ick = cfg.data_dir.join(format!("shard-{CELL}/ckpt")).join(inf_log::ckpt::ick_file_name(1));
    let section_bytes_max = largest_section(&fs, &ick);
    let (mut over, stats, mut ks, log) =
        judged_boot_logged(&fs, &cfg, tiered_keyspace(), BUDGET, &end_unit_of(section_bytes_max));
    let stats = stats.expect("the end unit boots");
    let (ram, rename, group) = (
        inf_store::limits::RECOVER_PROBE_CHARGE_BYTES,
        inf_server::limits::BOOT_FILE_OP_CHARGE_BYTES,
        inf_store::limits::REBUILD_GROUP_CHARGE_BYTES,
    );
    let mut hand_over_steps = 0u64;
    for (step, row) in log.iter().enumerate() {
        if row.phase != inf_server::UnitPhase::HandOver {
            continue;
        }
        hand_over_steps += 1;
        over.extend(over_price("STAMPS", step, row.stamped, ram, BUDGET));
        over.extend(over_price("SEEDS", step, row.seeded, ram, BUDGET));
        over.extend(over_price("RENAMES", step, row.renames, rename, BUDGET));
        over.extend(over_price("GROUPS", step, row.groups, group, BUDGET));
    }
    let units = stats.units;
    eprintln!(
        "end unit at {BUDGET}: {} over, {hand_over_steps} hand-over steps, {units:?}, {} rebuild \
         reads, {} revived, {:?}",
        over.len(),
        stats.shadow_settle_reads,
        stats.blob_revived,
        stats.step
    );
    assert!(over.is_empty(), "JUDGE: {} steps over: {over:?}", over.len());
    assert_eq!(units.sweep_stamped, END_PARKED, "every parked death stamped once");
    assert_eq!(units.sweep_seeded, unit.listed + unit.quarantined, "every listed id seeded once");
    assert_eq!(stats.blob_revived, END_REVIVED, "every referenced quarantined extent revived");
    let over_cap = (END_REWRITES - inf_store::SHADOW_TICKETS_CAP) as u64;
    assert_eq!(stats.shadow_settle_reads, over_cap, "every pair past the cap read and settled");
    // Each stage outgrows a step, so its cursor resumes inside it.
    assert!(units.sweep_stamped * ram > 2 * BUDGET, "VACUOUS: the stamps fit a step");
    assert!(units.sweep_seeded * ram > 2 * BUDGET, "VACUOUS: the seeds fit a step");
    assert!(stats.blob_revived * rename > 2 * BUDGET, "VACUOUS: the renames fit a step");
    assert!(units.rebuild_groups * group > 2 * BUDGET, "VACUOUS: the rebuild fits a step");
    let table = ks.tiered_store_mut(TIER_NS).expect("tiered");
    let live = table.extent_stats().live;
    assert_eq!(live, END_REVIVED, "the live extents' references stand");
    let (served, _, _) = read_back(&fs, &cfg, &mut ks, &unit.model);
    assert_model(&served, &unit.model, "after the end unit");
}

/// Budget independence over the end of a boot: the end unit recovered at
/// `u64::MAX` (the control, which runs each stage whole) and at 8 MiB,
/// 256 KiB, 4 KiB, 1 and 0 recovers the same table, tier and extent files
/// byte for byte (the revived names among them), served values, extent
/// reference map, reclaim queue in hand-out order (the stamped deaths, then
/// the orphans and the second verdicts, in the listings' order, which the
/// small budgets queue a batch a step), ticket rebuild counters, D6 counters
/// and record counts.
#[test]
fn every_budget_recovers_the_end_of_the_unparked_boot() {
    let cfg = step_cfg();
    // Each boot recovers its own copy of the unit, written the same way: a
    // boot's tier files are the next boot's dead-life files (D4).
    let boot = |budget: u64| {
        let fs = CountingFs::new();
        let unit = write_end_unit(&fs, &cfg);
        let state = recovered(&fs, &cfg, budget, &unit.model);
        (unit.model, state)
    };
    let (model, control) = boot(u64::MAX);
    assert_model(&control.served, &model, "the control boot");
    let queued = control.reclaim_order.len() as u64;
    assert_eq!(queued, END_PARKED + END_ORPHANS + END_REVIVED, "VACUOUS: {queued} queued");
    assert_eq!(control.blob_revived, END_REVIVED, "VACUOUS: no revive rename");
    assert!(control.shadow_settle_reads > 0, "VACUOUS: no slot past the ticket cap");
    for budget in [8 << 20, 256 << 10, 4 << 10, 1, 0] {
        let (model, state) = boot(budget);
        assert_model(&state.served, &model, &format!("BUDGET INDEPENDENCE at {budget}"));
        assert_same(&state, &control, budget);
    }
}

/// A fresh cell — no log, no MANIFEST — hands its tiered machines over under
/// the step budget before it creates its first segment: a namespace whose
/// directory lists 64 orphan extents seeds them at a 256-byte budget, four
/// a step at the RAM unit price, in steps of the hand-over row, then the
/// resume; every orphan is queued, in the listing's order. A fresh cell with
/// no tiered namespace has nothing to hand over and completes in its first
/// step.
#[test]
fn a_fresh_cell_hands_its_tiered_machines_over_under_the_step_budget() {
    const BUDGET: u64 = 256;
    let cfg = step_cfg();
    let fs = MemFs::new();
    let cold = cold_dir(&cfg);
    fs.create_dir_all(&cold).expect("the namespace's directory");
    for id in 1..=64u64 {
        let name = inf_log::extent_file_name(inf_log::ExtentId(id));
        let mut file = fs.create_meta(&cold.join(name)).expect("an orphan");
        file.write_at(0, b"extent").expect("its bytes");
    }
    let mut ks = tiered_keyspace();
    let mut recovery = Recovery::new(fs.clone(), CELL, &cfg, anchor(), now());
    let mut hand_over_steps = 0u64;
    // Bound: a step a unit batch, 64 seeds at four a step.
    for _ in 0..1_000 {
        let progress = recovery.step(&mut ks, BUDGET).expect("a fresh cell's step");
        if recovery.unit_row().phase == inf_server::UnitPhase::HandOver {
            hand_over_steps += 1;
        }
        if progress == RecoveryProgress::Complete {
            break;
        }
    }
    let (_rotor, stats, _seed) = recovery.finish();
    let seeded = stats.units.sweep_seeded;
    eprintln!("fresh cell at {BUDGET}: {hand_over_steps} hand-over steps, {:?}", stats.units);
    assert_eq!(seeded, 64, "every listed orphan seeded once");
    let price = inf_store::limits::RECOVER_PROBE_CHARGE_BYTES;
    assert!(hand_over_steps >= 64 / (BUDGET / price), "{hand_over_steps} hand-over steps");
    let table = ks.tiered_store_mut(TIER_NS).expect("tiered");
    let queue = table.extent_reclaim_work(u64::MAX, usize::MAX);
    let order: Vec<u64> = queue.iter().map(|candidate| candidate.extent_id).collect();
    assert_eq!(order, (1..=64).collect::<Vec<u64>>(), "the orphans, in the listing's order");

    let mut ks = fresh_keyspace();
    let mut recovery = Recovery::new(MemFs::new(), CELL, &cfg, anchor(), now());
    let first = recovery.step(&mut ks, BUDGET).expect("a fresh memory cell's step");
    assert_eq!(first, RecoveryProgress::Complete, "nothing to hand over: one step");
}
