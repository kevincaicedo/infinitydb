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
        let slot = self.rotor.begin_frame(self.ring.pending_frame_len(), 0).expect("reserve");
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
fn write_unit<F: SegmentFs + Clone>(fs: &F, cfg: &DurableConfig, windows: u64) -> Model {
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
    model
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
    let model = write_unit(&fs, &cfg, 3);
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
