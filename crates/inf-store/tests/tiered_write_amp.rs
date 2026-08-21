//! M4-S16 — write-amplification reporting (ADR-0060) at the seam tier:
//! the ratio's definition, the arithmetic that must never flatter, and
//! the **deliberate-regression canary** the story's second AC names.
//!
//! Two properties are load-bearing here and both are measured, not
//! argued:
//!
//! 1. **`compaction_bytes` is not a numerator term** (ADR-0060 D2).
//!    Copy-forward re-appends into the RAM tail, and the ordinary flush
//!    leg carries those bytes to the device — where `flush_bytes` already
//!    counts them. `relocated_bytes_reach_the_device_through_the_flush_leg`
//!    is the counter-test: it measures the overlap and states, in numbers,
//!    how far the rejected numerator would drift from device truth.
//! 2. **The gate has teeth.** `mistuned_dead_ratio_trips_the_write_amp_gate`
//!    runs one workload twice against the same build — the ADR-0059 D1
//!    default 50% dead-ratio trigger, then the same trigger mis-tuned to
//!    10% — and asserts the mis-tuned leg reports write amplification well
//!    above the §7 gate. Compaction that fires at one-tenth dead copies
//!    nine live bytes to reclaim one, and those nine are flushed again:
//!    the regression is physical, not simulated. That test also carries
//!    the story's finding about how little headroom the *tuned* default
//!    has under sustained churn — read its doc comment before touching the
//!    dead-ratio default.
//!
//! Substrate `MemFs` with a real `StagingRing`/`SegmentRotor` WAL and the
//! real `TierFlush` pipeline: write amplification is a ratio of counters,
//! so it is device-independent by construction — the block-layer
//! reconciliation on real NVMe is `benches/write_accounting.rs`.

use inf_log::flush::unlink_tier_file;
use inf_log::fs::mem::MemFs;
use inf_log::{
    MutationEffect, NsId, SegmentConfig, SegmentRotor, StagingConfig, StagingRing,
    TIER_FRAME_BYTES, TierFlush, TierFlushConfig, TierIoMode, create_cell_dirs, tier_extract,
    tier_frame_offset, tier_frame_span,
};
use inf_store::{
    AddressSpaceConfig, CompactionConfig, CompactionWork, DemotionConfig, LogicalAddr,
    TieredLookup, TieredTable, WriteAccounting, WriteAmplification,
};

const NS: NsId = NsId(61);
const PAGE: u64 = 4 << 10;
const BUDGET: u64 = 1 << 20;
/// Small files keep many compaction candidates in play at test scale.
const FILE_CAPACITY: u64 = 64 << 10;
/// Flush slice: 16 tier frames per barrier. The S13 finding says the
/// partial-tail-frame rewrite is paid once per barrier, so a slice this
/// size costs ≤ ~6% of `flush_bytes` in rewrites — near the 4 KiB
/// pathological end would swamp the compaction signal this file is
/// measuring (the ADR-0052 D4 production default is 1 MiB).
const FLUSH_SLICE: u64 = 64 << 10;
/// The §7 exit gate, in the milli-units the reported figure uses. The
/// authority is `docs/milestones/m4-gates.toml` (`write_amplification`,
/// `< 3.0`); duplicated here as a constant so the canary asserts against
/// the same number the report evaluates.
const GATE_MILLI: u64 = 3_000;
/// A live set of ~1.6 MB — large enough (25× a tier file) that a file's
/// dead ratio creeps up over many rounds instead of jumping past every
/// threshold inside one. The trigger can only be the variable if the
/// workload gives it room to be.
const KEYS: u64 = 8_000;
const VALUE_BYTES: usize = 192;

fn seeded(x: &mut u64) -> u64 {
    *x ^= *x << 13;
    *x ^= *x >> 7;
    *x ^= *x << 17;
    *x
}

/// One tiered namespace with its WAL and tier pipeline, driven the way
/// the MAINTAIN round drives them.
struct Rig {
    table: TieredTable,
    fs: MemFs,
    flush: TierFlush<MemFs>,
    rotor: SegmentRotor<MemFs>,
    ring: StagingRing,
    /// Bytes copy-forward relocated (the `compaction_bytes` twin, tracked
    /// independently so the counter can be checked against it).
    relocated_bytes: u64,
    relocated_records: u64,
    /// WAL frame bytes the rotor actually wrote — the envelope term the
    /// per-namespace counters deliberately do not pro-rate (S13).
    frame_bytes: u64,
    ckpt_id: u64,
    unlinked: u32,
}

impl Rig {
    fn new(dead_ratio_pct: u8) -> Rig {
        // Seal in flush-slice quanta, so one barrier writes a full run of
        // frames instead of rewriting the same partial tail frame every
        // time (the S13 slice-budget finding — at a 4 KiB quantum the
        // rewrite term alone is worth ~0.9× of user bytes and would
        // dominate everything this file measures).
        let demote =
            DemotionConfig { slice_bytes: FLUSH_SLICE, ..DemotionConfig::for_budget(BUDGET, PAGE) };
        let fs = MemFs::new();
        let dirs = create_cell_dirs(&fs, std::path::Path::new("data/shard-0")).expect("cell dirs");
        let mut table = TieredTable::new(
            AddressSpaceConfig {
                reserve_bytes: demote.ring_reserve_bytes().expect("valid budget"),
                page_bytes: PAGE as usize,
                life_origin: LogicalAddr::ZERO,
            },
            demote,
            KEYS as usize * 2,
        )
        .expect("ring");
        table.set_compaction_config(CompactionConfig { dead_ratio_pct, slice_bytes: 1 << 20 });
        let flush = TierFlush::new(
            fs.clone(),
            TierFlushConfig {
                shard_dir: std::path::Path::new("data/shard-0").to_path_buf(),
                cell: 0,
                ns: NS,
                mode: TierIoMode::Buffered,
                file_capacity: FILE_CAPACITY,
                slice_bytes: FLUSH_SLICE,
            },
            0,
        );
        let rotor = SegmentRotor::create_fresh(
            fs.clone(),
            dirs.log,
            SegmentConfig { segment_bytes: 8 << 20, ..Default::default() },
        )
        .expect("fresh log");
        Rig {
            table,
            fs,
            flush,
            rotor,
            ring: StagingRing::new(StagingConfig::default()),
            relocated_bytes: 0,
            relocated_records: 0,
            frame_bytes: 0,
            ckpt_id: 0,
            unlinked: 0,
        }
    }

    fn accounting(&self) -> WriteAccounting {
        self.table.write_accounting()
    }

    /// Stages one effect, sealing the frame first when the ring is full —
    /// the LOG step's shape, so `wal_bytes` is charged by the same path
    /// production charges it through.
    fn stage(&mut self, effect: &MutationEffect<'_>) {
        if self.table.stage_wal(&mut self.ring, effect).is_err() {
            self.write_frame();
            self.table.stage_wal(&mut self.ring, effect).expect("a drained ring has room");
        }
    }

    fn write_frame(&mut self) {
        let Some(lease) = self.ring.flush_into(&mut self.rotor, 0).expect("log append") else {
            return;
        };
        self.frame_bytes += u64::from(lease.frame_len());
        self.ring.release(lease);
    }

    /// SET through the WAL then the record store, exactly as a tiered
    /// namespace's mutation path does (displacement included — an
    /// overwrite of a cold record is what creates the dead bytes
    /// compaction later reclaims).
    fn set(&mut self, key: &[u8], value: &[u8]) {
        self.stage(&MutationEffect::StringSet { ns: NS, key, value });
        let hash = TieredTable::hash_key(key);
        let found = match self.table.lookup(key, hash, &[]) {
            TieredLookup::Ram(addr) | TieredLookup::Cold(addr) => Some(addr),
            TieredLookup::Miss => None,
        };
        match found {
            Some(addr) => {
                let (len, version) = match self.table.lookup(key, hash, &[]) {
                    TieredLookup::Ram(a) => {
                        let parts = self.table.record(a);
                        (parts.encoded_len, parts.version)
                    }
                    _ => {
                        let bytes = self.read_cold(addr).expect("cold record readable");
                        let parts = TieredTable::decode_record(&bytes);
                        assert_eq!(parts.key, key, "no hash collisions at test scale");
                        (parts.encoded_len, parts.version)
                    }
                };
                let _ = self.table.take_displacement_origins(hash, addr);
                self.table.update(key, value, hash, addr, len, version).expect("fits");
            }
            None => {
                self.table.insert(key, value, hash).expect("fits");
            }
        }
    }

    /// A cold record's bytes, via the tier files the flush pipeline wrote
    /// — sealed files first, then the file still being appended to (a
    /// record can be flushed and released while its file is open).
    fn read_cold(&self, addr: LogicalAddr) -> Option<Vec<u8>> {
        let raw = addr.to_raw();
        let holds = |base: u64, data_len: u64| raw >= base && raw < base + data_len;
        let (base, path, len) = self
            .flush
            .sealed()
            .iter()
            .find(|m| holds(m.base.to_raw(), m.data_len))
            .map(|m| (m.base.to_raw(), m.path.clone(), m.data_len))
            .or_else(|| {
                let (_, base, _, durable_len, path) = self.flush.active()?;
                holds(base.to_raw(), durable_len)
                    .then(|| (base.to_raw(), path.to_path_buf(), durable_len))
            })?;
        let image = self.fs.contents(&path)?;
        // The header is enough to learn the record's length; read a
        // bounded window and extract exactly it.
        let window = usize::try_from((base + len - raw).min(1 << 12)).expect("fits");
        let (first, count, skip) = tier_frame_span(raw - base, window);
        let from = tier_frame_offset(first) as usize;
        let to = from + count as usize * TIER_FRAME_BYTES;
        let mut out = Vec::new();
        tier_extract(image.get(from..to)?, skip, window, &mut out).ok()?;
        let encoded = TieredTable::decode_record(&out).encoded_len;
        out.truncate(encoded);
        Some(out)
    }

    /// The initial fill: every key once, draining often enough that the
    /// tail window never runs out of address space (the MAINTAIN cadence
    /// a reactor runs continuously).
    fn fill(&mut self, value: &[u8]) {
        for i in 0..KEYS {
            self.set(format!("k:{i:06}").as_bytes(), value);
            if i.is_multiple_of(64) {
                self.drain();
            }
        }
        self.drain();
    }

    /// Seal → flush → release, driven to quiescence.
    fn drain(&mut self) {
        loop {
            let sealed = self.table.seal_slice();
            let flushed = self.table.flush_slice(&mut self.flush).expect("flush slice");
            let released = self.table.release_slice();
            if sealed + released + flushed.appended_bytes + u64::from(flushed.gaps_crossed) == 0 {
                break;
            }
        }
    }

    /// One compaction slice at the full ADR-0059 D6 budget. Returns the
    /// bytes relocated by this slice.
    fn compact_slice(&mut self, budget: u64) -> u64 {
        let before = self.relocated_bytes;
        let mut spent = 0u64;
        while spent < budget {
            let work = self.table.compaction_work(&self.flush, false, budget - spent);
            let CompactionWork::Read { file_id, addr, len } = work else { break };
            let Some(bytes) = self.read_chunk(file_id, addr, len) else { break };
            let applied = self.table.compaction_apply(file_id, addr, &bytes);
            self.relocated_bytes += applied.relocated_bytes;
            self.relocated_records += u64::from(applied.relocated);
            // `need` when the next record exceeds the chunk (the
            // minimum-one-record rule), `1` so a no-progress answer can
            // never spin: the budget is the loop's bound, not a hope.
            spent += applied.consumed.max(applied.need).max(1);
            if applied.stalled {
                break;
            }
        }
        self.relocated_bytes - before
    }

    /// A compaction scan chunk (the S08 cold read, modeled synchronously).
    fn read_chunk(&self, file_id: u32, addr: LogicalAddr, len: u64) -> Option<Vec<u8>> {
        let meta = self.flush.sealed().iter().find(|m| m.id == file_id)?.clone();
        let image = self.fs.contents(&meta.path)?;
        let len = usize::try_from(len).expect("fits");
        let (first, count, skip) = tier_frame_span(addr.to_raw() - meta.base.to_raw(), len);
        let from = tier_frame_offset(first) as usize;
        let to = from + count as usize * TIER_FRAME_BYTES;
        let mut out = Vec::new();
        tier_extract(image.get(from..to)?, skip, len, &mut out).ok()?;
        Some(out)
    }

    /// Retirement: walk → stamp → manifest exclusion → commit → unlink,
    /// so emptied files leave the candidate set the way they do in the
    /// reactor (ADR-0059 D3).
    fn publish(&mut self) {
        self.ckpt_id += 1;
        self.table.begin_ckpt_walk(self.ckpt_id);
        self.table.end_ckpt_walk();
        self.table.retire_scan(self.ckpt_id, &self.flush);
        let _section = self.table.tier_manifest(NS.0, &self.flush);
        for id in self.table.commit_retirement() {
            let meta = self.flush.detach_sealed(id).expect("retired files are sealed");
            unlink_tier_file(&self.fs, &meta).expect("unlink");
            self.unlinked += 1;
        }
    }
}

/// What one workload run reported.
struct Outcome {
    accounting: WriteAccounting,
    amp: WriteAmplification,
    relocated_bytes: u64,
    relocated_records: u64,
    frame_bytes: u64,
    unlinked: u32,
}

impl Outcome {
    fn milli(&self) -> u64 {
        self.amp.milli().expect("the workload admitted user bytes")
    }

    /// The rejected numerator (ADR-0060 D2): device bytes **plus** the
    /// relocation volume, i.e. every relocated byte counted twice.
    fn rejected_milli(&self) -> u64 {
        let acct = self.accounting;
        (acct.written_bytes() + acct.compaction_bytes) * 1_000 / acct.user_bytes
    }

    fn print(&self, label: &str) {
        let acct = self.accounting;
        println!(
            "{label:<22} user {:>9} | wal {:>9} | flush {:>9} | relocated {:>9} B / {:>6} recs \
             | unlinked {:>3} | WA {}.{:03}× (rejected numerator {}.{:03}×)",
            acct.user_bytes,
            acct.wal_bytes,
            acct.flush_bytes,
            acct.compaction_bytes,
            self.relocated_records,
            self.unlinked,
            self.milli() / 1_000,
            self.milli() % 1_000,
            self.rejected_milli() / 1_000,
            self.rejected_milli() % 1_000,
        );
        assert_eq!(
            acct.compaction_bytes, self.relocated_bytes,
            "the counter tracks the relocations"
        );
        assert!(self.frame_bytes >= acct.wal_bytes, "frames carry at least their records");
    }
}

/// The shared workload: a skewed overwrite storm big enough that records
/// go cold, files fill, and the dead-ratio trigger arms — run identically
/// for every compaction tuning so the tuning is the only variable.
fn run_workload(dead_ratio_pct: u8) -> Outcome {
    /// Churn per round is ~2% of the live set: a file's dead ratio walks
    /// past 10% many rounds before it reaches 50%, which is what lets the
    /// threshold decide when copy-forward fires. Total churn is 2× the
    /// live set, long enough for the compactor to reach its steady state.
    const ROUNDS: u64 = 100;
    const WRITES_PER_ROUND: u64 = 160;

    let mut rig = Rig::new(dead_ratio_pct);
    let mut seed = 0x5165_0060u64;
    let value = vec![0x41u8; VALUE_BYTES];
    rig.fill(&value);
    for round in 0..ROUNDS {
        for _ in 0..WRITES_PER_ROUND {
            // 80% of writes land on the hottest fifth of the keyspace, so
            // cold files keep long-lived survivors: the dead-ratio arm
            // then forces real copy-forward instead of retiring
            // fully-churned files for free.
            let idx = if seeded(&mut seed) % 100 < 80 {
                seeded(&mut seed) % (KEYS / 5)
            } else {
                seeded(&mut seed) % KEYS
            };
            let len = VALUE_BYTES - (seeded(&mut seed) % 64) as usize;
            rig.set(format!("k:{idx:06}").as_bytes(), &value[..len]);
        }
        rig.drain();
        rig.compact_slice(1 << 20);
        rig.drain();
        if round.is_multiple_of(4) {
            rig.publish();
        }
    }
    rig.write_frame();
    rig.drain();
    rig.publish();

    let accounting = rig.accounting();
    Outcome {
        accounting,
        amp: accounting.write_amplification(),
        relocated_bytes: rig.relocated_bytes,
        relocated_records: rig.relocated_records,
        frame_bytes: rig.frame_bytes,
        unlinked: rig.unlinked,
    }
}

/// ADR-0060 D2, measured: copy-forward's bytes reach the device through
/// the **flush** leg, so `flush_bytes` already contains them and the
/// numerator must not add `compaction_bytes` on top.
///
/// Method: drive the workload until compaction has relocated bytes, then
/// drain the flush pipeline and compare the `flush_bytes` delta against
/// the relocation volume. The rejected numerator's drift is reported in
/// the same run — it is exactly the double-counted volume, which is why
/// keeping it would have broken S13's ±10% block-layer window the moment
/// compaction started running.
#[test]
fn relocated_bytes_reach_the_device_through_the_flush_leg() {
    let mut rig = Rig::new(50);
    let value = vec![0x42u8; VALUE_BYTES];
    rig.fill(&value);
    // Overwrite three keys in every five: each cold file lands ~60% dead
    // — past the 50% arm but with survivors left, which is the state that
    // makes copy-forward *move* bytes instead of retiring a fully-dead
    // file for free.
    for i in (0..KEYS).filter(|i| i % 5 < 3) {
        rig.set(format!("k:{i:06}").as_bytes(), &value[..128]);
        if i.is_multiple_of(64) {
            rig.drain();
        }
    }
    rig.drain();

    let before = rig.accounting();
    let relocated = rig.compact_slice(1 << 20);
    assert!(relocated > 0, "the trigger armed and copy-forward moved live records");
    let mid = rig.accounting();
    assert_eq!(
        mid.compaction_bytes - before.compaction_bytes,
        relocated,
        "the volume counter charged exactly the relocations"
    );
    assert_eq!(
        mid.flush_bytes, before.flush_bytes,
        "relocation itself writes nothing: it re-appends into the RAM tail"
    );

    rig.drain();
    let after = rig.accounting();
    let flush_delta = after.flush_bytes - mid.flush_bytes;
    assert!(
        flush_delta >= relocated,
        "the relocated bytes reached the device through the flush leg \
         (flush +{flush_delta} B for {relocated} B relocated)"
    );
    println!(
        "relocated {relocated} B → flush +{flush_delta} B; numerator {} B, \
         rejected numerator {} B (+{:.1}% double-counted)",
        after.written_bytes(),
        after.written_bytes() + after.compaction_bytes,
        after.compaction_bytes as f64 / after.written_bytes() as f64 * 100.0,
    );
    assert!(
        after.compaction_bytes * 20 > after.written_bytes(),
        "the double-count would be material (>5% of the numerator), which is why it is rejected"
    );
}

/// The M4-S16 canary (AC 2): the same build, the same workload, one knob
/// mis-tuned — compaction triggering at 10% dead instead of the ADR-0059
/// D1 default 50% — and the reported write amplification crosses the §7
/// gate. Nine live bytes copied to reclaim one, each re-flushed, is what
/// a mis-tuned trigger costs; the gate is what notices.
///
/// The printed figures are the canary artifact's numbers
/// (`.artifacts/m4/s16/`), and the mis-tuned figure is what
/// `inf-bench gate-run m4 --write-amp-milli` turns into a FAIL verdict.
///
/// **Finding this test carries (owners S19/S22).** Under sustained
/// overwrite churn where every dead byte lands in a tier file, the
/// steady-state ratio is
/// `WA ≈ wal/user + 1 + (1 − t)/t` for a dead-ratio trigger `t`:
/// compaction must reclaim one dead byte per user byte written, and at
/// threshold `t` it relocates `(1 − t)/t` live bytes to do it. That model
/// puts the shipped `t = 0.5` at **≈ 3.0×** — the §7 gate itself, with no
/// headroom — and this workload measures 3.04×, within 1.3% of it. So the
/// assertion below is deliberately *not* "the default passes the gate":
/// on this near-worst-case shape it does not, and tuning the workload
/// until it did would be exactly the silent narrowing L10 forbids. What is
/// asserted is the tripwire (the mis-tuned leg is far over the gate), the
/// causation (relocation volume), and a band on the tuned leg so a real
/// regression in the default path still fails here. The gate-grade number
/// comes from S22's zipfian rows, where a large hot set kills most
/// overwrites in RAM before they ever reach a file (`D/U < 1`) and the
/// same model gives real headroom; S19 owns exposing `t`.
#[test]
fn mistuned_dead_ratio_trips_the_write_amp_gate() {
    /// The tuned leg's band: the steady-state model's 3.0× plus room for
    /// the frame/header terms this substrate pays. Not a gate verdict —
    /// a regression bound (see the finding above).
    const TUNED_BAND_MILLI: u64 = 3_500;

    let tuned = run_workload(50);
    let canary = run_workload(10);
    println!("--- M4-S16 write-amplification canary (gate {GATE_MILLI} milli) ---");
    tuned.print("tuned (50% dead)");
    canary.print("canary (10% dead)");

    assert_eq!(
        tuned.accounting.user_bytes, canary.accounting.user_bytes,
        "identical workloads: only the trigger differs"
    );
    assert_eq!(tuned.accounting.wal_bytes, canary.accounting.wal_bytes, "the WAL leg is identical");
    assert!(
        canary.milli() >= GATE_MILLI,
        "the mis-tuned trigger must trip the gate ({}.{:03}× < {GATE_MILLI} milli) — \
         the tripwire does not fire and the story's second AC is unproven",
        canary.milli() / 1_000,
        canary.milli() % 1_000
    );
    assert!(
        canary.milli() > tuned.milli() * 2,
        "mis-tuning must dominate every other term: {}.{:03}× vs {}.{:03}×",
        canary.milli() / 1_000,
        canary.milli() % 1_000,
        tuned.milli() / 1_000,
        tuned.milli() % 1_000
    );
    assert!(
        canary.relocated_bytes > tuned.relocated_bytes * 2,
        "the mis-tuning is what moved the ratio: relocation volume {} B vs {} B",
        canary.relocated_bytes,
        tuned.relocated_bytes
    );
    assert!(
        tuned.milli() <= TUNED_BAND_MILLI,
        "the default path regressed past its band ({}.{:03}× > {TUNED_BAND_MILLI} milli)",
        tuned.milli() / 1_000,
        tuned.milli() % 1_000
    );
}

/// The undefined arm, end to end on a real table: a namespace that only
/// deletes writes WAL bytes and admits none, so its amplification is
/// unbounded — reported as `undefined`, never as a flattering zero.
#[test]
fn delete_only_namespace_reports_unbounded_amplification() {
    let mut rig = Rig::new(50);
    assert_eq!(
        rig.accounting().write_amplification(),
        WriteAmplification::Undefined { written_bytes: 0 },
        "an untouched namespace has no ratio and is not a fault"
    );
    assert!(!rig.accounting().write_amplification().is_unbounded());

    for i in 0..16u32 {
        let key = format!("gone:{i}");
        rig.stage(&MutationEffect::Delete { ns: NS, key: key.as_bytes() });
    }
    let acct = rig.accounting();
    assert_eq!(acct.user_bytes, 0, "a tombstone stores no user byte");
    assert!(acct.wal_bytes > 0, "it does cost a WAL record");
    let amp = acct.write_amplification();
    assert!(amp.is_unbounded(), "wrote bytes, admitted none");
    assert_eq!(amp.milli(), None);
    assert_eq!(amp.to_string(), "undefined");
}
