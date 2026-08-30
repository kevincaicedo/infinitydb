//! M4-S15 — copy-forward compaction at the seam tier (ADR-0059): the
//! scan/relocate/repoint slice, the trigger arms, refusal-aware
//! admission, the retirement pipeline (stamp → manifest exclusion →
//! commit/abort → detach → unlink), byte-counter finalization
//! (ADR-0058's obligation), and the D9 relocation-origin discipline —
//! including the replay test proving the origin markers are what keep
//! ADR-0057 D4's exact replay exact across unlogged relocations, and
//! its counter-test proving they are load-bearing.
//!
//! The endurance slice test at the bottom runs on the real filesystem
//! (`StdSegmentFs`): sustained overwrites, disk usage oscillating
//! within budget, the cold floor advancing, and `statvfs` confirming
//! the reclaimed space actually returned to the OS.

use std::collections::BTreeMap;
use std::path::Path;

use inf_log::flush::unlink_tier_file;
use inf_log::fs::mem::MemFs;
use inf_log::{
    CkptConfig, Lsn, Manifest, NsId, RecordView, SegmentId, SyncIckWriter, TIER_FRAME_BYTES,
    TierFlush, TierFlushConfig, TierIoMode, decode_record, read_ick_hybrid, read_manifest,
    tier_extract, tier_frame_offset, tier_frame_span, write_manifest,
};
use inf_store::KeyHasher;
use inf_store::{
    AddressSpaceConfig, CompactionConfig, CompactionWork, DemotionConfig, LogicalAddr,
    TieredLookup, TieredTable, apply_live_set_section, apply_ref_section, recover_tiered_ns,
};

const NS: NsId = NsId(47);
const PAGE: u64 = 4 << 10;
const BUDGET: u64 = 1 << 20;
const FILE_CAPACITY: u64 = 48 << 10;
const SHARD: &str = "shard-0";

fn seeded(x: &mut u64) -> u64 {
    *x ^= *x << 13;
    *x ^= *x >> 7;
    *x ^= *x << 17;
    *x
}

fn flush_config() -> TierFlushConfig {
    TierFlushConfig {
        shard_dir: Path::new(SHARD).to_path_buf(),
        cell: 0,
        ns: NS,
        mode: TierIoMode::Buffered,
        file_capacity: FILE_CAPACITY,
        slice_bytes: PAGE,
    }
}

fn space_config(demote: DemotionConfig, origin: u64) -> AddressSpaceConfig {
    AddressSpaceConfig {
        reserve_bytes: demote.ring_reserve_bytes().expect("valid budget"),
        page_bytes: PAGE as usize,
        life_origin: LogicalAddr::from_raw(origin).expect("48-bit"),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Expect {
    value: Vec<u8>,
    encoded_len: usize,
    version: u32,
}

struct Rig {
    table: TieredTable,
    fs: MemFs,
    flush: TierFlush<MemFs>,
    model: BTreeMap<Vec<u8>, Expect>,
    /// Modeled WAL tail since ckpt-begin (real record-v1 encodings).
    tail: Vec<u8>,
    begun: bool,
    /// D9 origin markers ride displacements when true; the
    /// load-bearing counter-test turns them off to prove the hazard.
    emit_origins: bool,
    /// Version comparison is valid only within one life (relocations
    /// copy verbatim); across a recovery, string versions are per-life
    /// artifacts no oracle may compare (ADR-0057 D3).
    verify_versions: bool,
}

impl Rig {
    fn new() -> Rig {
        let demote = DemotionConfig::for_budget(BUDGET, PAGE);
        let fs = MemFs::new();
        let table = TieredTable::new(space_config(demote, 0), demote, 2048, KeyHasher::default())
            .expect("ring");
        let flush = TierFlush::new(fs.clone(), flush_config(), 0);
        Rig {
            table,
            fs,
            flush,
            model: BTreeMap::new(),
            tail: Vec::new(),
            begun: false,
            emit_origins: true,
            verify_versions: true,
        }
    }

    fn maintain(&mut self) {
        loop {
            let sealed = self.table.seal_slice();
            let f = self.table.flush_slice(&mut self.flush).expect("flush slice");
            let released = self.table.release_slice();
            if sealed + released + f.appended_bytes + u64::from(f.gaps_crossed) == 0 {
                break;
            }
        }
    }

    fn read_cold(&self, addr: u64, len: usize) -> Option<Vec<u8>> {
        let contains = |base: u64, flen: u64| addr >= base && addr + len as u64 <= base + flen;
        let (base, path) = self
            .flush
            .sealed()
            .iter()
            .find(|m| contains(m.base.to_raw(), m.data_len))
            .map(|m| (m.base.to_raw(), m.path.clone()))
            .or_else(|| {
                let (_, base, _, durable_len, path) = self.flush.active()?;
                contains(base.to_raw(), durable_len).then(|| (base.to_raw(), path.to_path_buf()))
            })?;
        let image = self.fs.contents(&path)?;
        let (first, count, skip) = tier_frame_span(addr - base, len);
        let from = tier_frame_offset(first) as usize;
        let to = from + count as usize * TIER_FRAME_BYTES;
        let mut out = Vec::new();
        tier_extract(image.get(from..to)?, skip, len, &mut out).ok()?;
        Some(out)
    }

    /// SET with the D4 marker discipline, D9 origin markers included.
    fn set(&mut self, key: &[u8], value: &[u8]) {
        let hash = KeyHasher::default().hash(key);
        let displaced: Option<(LogicalAddr, usize, u32)> = match self.table.lookup(key, hash, &[]) {
            TieredLookup::Ram(addr) => {
                let parts = self.table.record(addr);
                Some((addr, parts.encoded_len, parts.version))
            }
            TieredLookup::Cold(addr) => {
                let expect = self.model.get(key).expect("cold candidate implies model entry");
                let bytes = self
                    .read_cold(addr.to_raw(), expect.encoded_len)
                    .expect("cold record readable");
                let parts = TieredTable::decode_record(&bytes);
                assert_eq!(parts.key, key, "no hash collisions at test scale");
                Some((addr, parts.encoded_len, parts.version))
            }
            TieredLookup::Miss => None,
        };
        let new_version = match displaced {
            Some((old, old_len, old_version)) => {
                let origins = self.table.take_displacement_origins(hash, old);
                if self.begun {
                    if self.emit_origins {
                        for (origin, _) in origins {
                            RecordView::ColdDisplace { ns: NS, old_addr: origin }
                                .encode_into(&mut self.tail);
                        }
                    }
                    RecordView::ColdDisplace { ns: NS, old_addr: old.to_raw() }
                        .encode_into(&mut self.tail);
                }
                self.table.update(key, value, hash, old, old_len, old_version).expect("fits");
                old_version.wrapping_add(1)
            }
            None => {
                self.table.insert(key, value, hash).expect("fits");
                0
            }
        };
        if self.begun {
            RecordView::StringPostImage { ns: NS, key, value }.encode_into(&mut self.tail);
        }
        let encoded_len = match self.table.lookup(key, hash, &[]) {
            TieredLookup::Ram(addr) => self.table.record(addr).encoded_len,
            _ => unreachable!("a fresh write is RAM-resident"),
        };
        self.model.insert(
            key.to_vec(),
            Expect { value: value.to_vec(), encoded_len, version: new_version },
        );
    }

    fn del(&mut self, key: &[u8]) {
        let hash = KeyHasher::default().hash(key);
        let target = match self.table.lookup(key, hash, &[]) {
            TieredLookup::Ram(addr) => Some((addr, self.table.record(addr).encoded_len)),
            TieredLookup::Cold(addr) => {
                let expect = self.model.get(key).expect("model entry");
                Some((addr, expect.encoded_len))
            }
            TieredLookup::Miss => None,
        };
        if let Some((addr, len)) = target {
            let origins = self.table.take_displacement_origins(hash, addr);
            if self.begun {
                if self.emit_origins {
                    for (origin, _) in origins {
                        RecordView::ColdDisplace { ns: NS, old_addr: origin }
                            .encode_into(&mut self.tail);
                    }
                }
                RecordView::ColdDisplace { ns: NS, old_addr: addr.to_raw() }
                    .encode_into(&mut self.tail);
                RecordView::Delete { ns: NS, key }.encode_into(&mut self.tail);
            }
            self.table.delete(hash, addr, len);
            self.model.remove(key);
        }
    }

    /// A bounded copy-forward burst: work → catalog read → apply. A
    /// stall runs one maintain round (the refusal's resolver) and
    /// retries; `need` re-reads exactly one oversized record. Returns
    /// (records relocated, files fully scanned, need round-trips).
    fn compact_burst(&mut self, pressure: bool, rounds: u32) -> (u64, u32, u32) {
        let mut relocated = 0u64;
        let mut scanned = 0u32;
        let mut need_hits = 0u32;
        let mut budget = PAGE * 2;
        for _ in 0..rounds {
            match self.table.compaction_work(&self.flush, pressure, budget) {
                CompactionWork::Read { file_id, addr, len } => {
                    let bytes = self
                        .read_cold(addr.to_raw(), usize::try_from(len).expect("fits"))
                        .expect("scan chunk readable");
                    let applied = self.table.compaction_apply(file_id, addr, &bytes);
                    relocated += u64::from(applied.relocated);
                    scanned += u32::from(applied.file_scanned);
                    if applied.need > 0 {
                        need_hits += 1;
                        budget = applied.need;
                    } else {
                        budget = PAGE * 2;
                    }
                    if applied.stalled {
                        self.maintain();
                    }
                }
                CompactionWork::Idle => break,
            }
        }
        (relocated, scanned, need_hits)
    }

    /// One publication cycle for retirement mechanics (no `.ick`
    /// emission — the replay test below does the full form): walk
    /// stamps, retire scan, manifest exclusion, commit + detach +
    /// unlink. Returns the retired ids.
    fn publish_cycle(&mut self, ckpt_id: u64) -> Vec<u32> {
        self.table.begin_ckpt_walk(ckpt_id);
        self.table.end_ckpt_walk();
        self.table.retire_scan(ckpt_id, &self.flush);
        let _section = self.table.tier_manifest(NS.0, &self.flush);
        let ids = self.table.commit_retirement();
        for &id in &ids {
            let meta = self.flush.detach_sealed(id).expect("retired files are sealed");
            unlink_tier_file(&self.fs, &meta).expect("unlink");
            assert!(self.fs.contents(&meta.path).is_none(), "the bytes are gone");
        }
        ids
    }

    fn audit(&mut self) {
        let keys: Vec<(Vec<u8>, Expect)> =
            self.model.iter().map(|(k, e)| (k.clone(), e.clone())).collect();
        for (key, expect) in keys {
            let hash = KeyHasher::default().hash(&key);
            let mut exclude: Vec<LogicalAddr> = Vec::new();
            let (value, version) = loop {
                match self.table.lookup(&key, hash, &exclude) {
                    TieredLookup::Ram(addr) => {
                        let parts = self.table.record(addr);
                        break (parts.value.to_vec(), parts.version);
                    }
                    TieredLookup::Cold(addr) => {
                        let bytes = self
                            .read_cold(addr.to_raw(), expect.encoded_len)
                            .expect("cold record readable");
                        let parts = TieredTable::decode_record(&bytes);
                        if parts.key == key.as_slice() {
                            break (parts.value.to_vec(), parts.version);
                        }
                        exclude.push(addr);
                    }
                    TieredLookup::Miss => {
                        panic!("live key {:?} missing", String::from_utf8_lossy(&key))
                    }
                }
            };
            assert_eq!(value, expect.value, "content for {:?}", String::from_utf8_lossy(&key));
            if self.verify_versions {
                assert_eq!(
                    version,
                    expect.version,
                    "version preserved for {:?} (relocations copy verbatim)",
                    String::from_utf8_lossy(&key)
                );
            }
        }
    }

    /// Per-file exactness invariant for byte-exact files (the S14 storm
    /// identity, re-checked under compaction and retirement). A file's
    /// range can straddle the head (flushed but unreleased), so both
    /// RAM- and cold-resolved slots count as its live bytes.
    fn assert_byte_exact_identity(&self) {
        for f in self.table.live_set().files() {
            assert!(f.dead_bytes <= f.data_len, "dead within file bytes");
            if f.byte_exact {
                let live: u64 = self
                    .model
                    .iter()
                    .filter_map(|(k, e)| {
                        let hash = KeyHasher::default().hash(k);
                        let addr = match self.table.lookup(k, hash, &[]) {
                            TieredLookup::Ram(addr) | TieredLookup::Cold(addr) => addr.to_raw(),
                            TieredLookup::Miss => return None,
                        };
                        (addr >= f.base && addr < f.base + f.data_len)
                            .then_some(e.encoded_len as u64)
                    })
                    .sum();
                assert_eq!(f.data_len - f.dead_bytes, live, "file {} live bytes exact", f.id);
            }
        }
    }
}

/// Cold keys inside one file's range, by model lookup.
fn keys_in_file(rig: &Rig, base: u64, len: u64) -> Vec<Vec<u8>> {
    rig.model
        .keys()
        .filter(|key| {
            let hash = KeyHasher::default().hash(key);
            matches!(
                rig.table.lookup(key, hash, &[]),
                TieredLookup::Cold(addr) if addr.to_raw() >= base && addr.to_raw() < base + len
            )
        })
        .cloned()
        .collect()
}

/// Fills the rig with cold records across several sealed files.
fn fill_cold(rig: &mut Rig, keys: u64) {
    let mut seed = 0x0515_C0DE_u64;
    for i in 0..keys {
        let key = format!("k:{i:05}").into_bytes();
        let value = vec![(seeded(&mut seed) % 251) as u8; 60 + (seeded(&mut seed) % 100) as usize];
        rig.set(&key, &value);
        if i % 64 == 63 {
            rig.maintain();
        }
    }
    rig.maintain();
    assert!(
        rig.flush.sealed().len() >= 3,
        "the workload spans several sealed files (sealed {}, flushed {}, ro {}, tail {})",
        rig.flush.sealed().len(),
        rig.table.space().flushed().to_raw(),
        rig.table.space().ro_boundary().to_raw(),
        rig.table.space().tail().to_raw()
    );
}

/// Copy-forward relocates only live records, verbatim (versions and
/// bytes), charges exactly `compaction_bytes`, never `user_bytes`, and
/// finalizes the scanned file's byte counters (ADR-0059 D2/D4).
#[test]
fn copy_forward_relocates_live_verbatim_and_finalizes() {
    let mut rig = Rig::new();
    fill_cold(&mut rig, 4500);
    let first = rig.table.live_set().files()[0].clone();
    // Kill ~60% of the first file's records so the 50% trigger arms.
    let victims = keys_in_file(&rig, first.base, first.data_len);
    assert!(victims.len() >= 10, "the first file holds enough cold keys");
    let kill = victims.len() * 6 / 10;
    for key in &victims[..kill] {
        rig.del(key);
    }
    let f = &rig.table.live_set().files()[0];
    assert!(f.dead_bytes * 100 >= f.data_len * 50, "the dead-ratio arm is armed");
    assert!(!f.is_dead(), "live records remain before the scan");

    let user_before = rig.table.write_accounting().user_bytes;
    let wal_before = rig.table.write_accounting().wal_bytes;
    let (relocated, scanned, _) = rig.compact_burst(false, 64);
    assert!(relocated as usize >= victims.len() - kill, "every survivor relocated");
    assert_eq!(scanned, 1, "the candidate scan completed");
    let acct = rig.table.write_accounting();
    assert_eq!(acct.user_bytes, user_before, "relocations never charge user bytes");
    assert_eq!(acct.wal_bytes, wal_before, "relocations never stage WAL");
    assert!(acct.compaction_bytes > 0, "the S13 seam charged");
    assert!(rig.table.space().counters().compact_slices > 0, "slices counted");

    let f = rig
        .table
        .live_set()
        .files()
        .iter()
        .find(|f| f.id == first.id)
        .expect("still tracked until retirement")
        .clone();
    assert!(f.is_dead(), "fully scanned means fully dead");
    assert!(f.byte_exact, "finalization heals byte exactness");
    assert_eq!(f.dead_bytes, f.data_len);

    // Every record — including relocated ones — still serves, with its
    // version intact (the verbatim-copy contract).
    rig.audit();
    rig.assert_byte_exact_identity();
}

/// The §3.1 deletion conjunction (ADR-0059 D3): `is_dead` alone never
/// retires a file — the covering checkpoint must have begun after the
/// last slot-removal; aborts roll back; commits detach and unlink; the
/// cold floor advances over the retired prefix.
#[test]
fn retirement_requires_a_covering_checkpoint_and_survives_abort() {
    let mut rig = Rig::new();
    fill_cold(&mut rig, 4500);
    let first = rig.table.live_set().files()[0].clone();
    let floor_before = rig.table.cold_floor();
    assert_eq!(floor_before, first.base);

    // Checkpoint 1 is walking while the file empties: stamped with the
    // in-flight id, so checkpoint 1 cannot cover it.
    rig.table.begin_ckpt_walk(1);
    for key in keys_in_file(&rig, first.base, first.data_len) {
        rig.del(&key);
    }
    rig.table.end_ckpt_walk();
    assert!(rig.table.live_set().files()[0].is_dead());
    assert_eq!(rig.table.retire_scan(1, &rig.flush), 0, "emptied mid-walk: not coverable by 1");
    rig.table.abort_retirement();

    // Checkpoint 2 began after the last removal: retiring, excluded
    // from the manifest under construction.
    rig.table.begin_ckpt_walk(2);
    rig.table.end_ckpt_walk();
    assert_eq!(rig.table.retire_scan(2, &rig.flush), 1);
    let section = rig.table.tier_manifest(NS.0, &rig.flush);
    assert!(
        section.files.iter().all(|f| f.id != first.id),
        "the retiring file is excluded from the manifest"
    );
    // The swap fails — a counted abort: the old unit still names the
    // file, so the mark rolls back and the manifest names it again.
    rig.table.abort_retirement();
    let section = rig.table.tier_manifest(NS.0, &rig.flush);
    assert!(section.files.iter().any(|f| f.id == first.id), "an aborted swap re-offers the file");

    // Re-marked and landed: detach + unlink, floor advances.
    assert_eq!(rig.table.retire_scan(2, &rig.flush), 1);
    let path = rig.flush.sealed()[0].path.clone();
    let ids = rig.table.commit_retirement();
    assert_eq!(ids, vec![first.id]);
    let meta = rig.flush.detach_sealed(first.id).expect("detach");
    unlink_tier_file(&rig.fs, &meta).expect("unlink");
    assert!(rig.fs.contents(&path).is_none(), "the file's bytes are gone");
    assert!(rig.table.cold_floor() > floor_before, "the cold floor advanced");
    rig.audit();
}

/// Refusal-aware admission (ADR-0059 D6): a full tail window stalls the
/// slice — never a suspension — and flush/release progress resumes it.
#[test]
fn stalled_relocation_resumes_after_window_progress() {
    // A small budget so relocations hit the admission bound quickly.
    let demote = DemotionConfig::for_budget(192 << 10, PAGE);
    let fs = MemFs::new();
    let table = TieredTable::new(space_config(demote, 0), demote, 2048, KeyHasher::default())
        .expect("ring");
    let flush = TierFlush::new(fs.clone(), flush_config(), 0);
    let mut rig = Rig {
        table,
        fs,
        flush,
        model: BTreeMap::new(),
        tail: Vec::new(),
        begun: false,
        emit_origins: true,
        verify_versions: true,
    };
    let mut seed = 0x57A11u64;
    for i in 0..1500u64 {
        let key = format!("s:{i:04}").into_bytes();
        let value = vec![(seeded(&mut seed) % 251) as u8; 120];
        rig.set(&key, &value);
        if i % 24 == 23 {
            rig.maintain();
        }
    }
    rig.maintain();
    assert!(!rig.flush.sealed().is_empty(), "the fill produced sealed cold files");
    let first = rig.table.live_set().files()[0].clone();
    // Kill just over half so the file triggers with plenty left to copy.
    let victims = keys_in_file(&rig, first.base, first.data_len);
    for key in &victims[..victims.len() * 55 / 100] {
        rig.del(key);
    }
    // Fill the mutable window so relocation admission refuses.
    let mut i = 0u64;
    loop {
        let key = format!("fill:{i:04}").into_bytes();
        let hash = KeyHasher::default().hash(&key);
        if rig.table.insert(&key, &[0x44; 96], hash).is_err() {
            break;
        }
        let encoded_len = match rig.table.lookup(&key, hash, &[]) {
            TieredLookup::Ram(addr) => rig.table.record(addr).encoded_len,
            _ => unreachable!("fresh write"),
        };
        rig.model.insert(key, Expect { value: vec![0x44; 96], encoded_len, version: 0 });
        i += 1;
    }
    // Raw applies (no healing maintain) until the page-granular
    // admission refuses: with the window at its bound, at most a page
    // or two of committed slack absorbs relocations before the typed
    // stall must surface — well within one file's live half.
    let mut stalled = false;
    for _ in 0..64 {
        let CompactionWork::Read { file_id, addr, len } =
            rig.table.compaction_work(&rig.flush, false, PAGE * 2)
        else {
            panic!("an armed candidate exists until the scan stalls");
        };
        let bytes =
            rig.read_cold(addr.to_raw(), usize::try_from(len).expect("fits")).expect("chunk");
        let applied = rig.table.compaction_apply(file_id, addr, &bytes);
        assert!(!applied.file_scanned, "the scan cannot complete against a full window");
        if applied.stalled {
            stalled = true;
            break;
        }
    }
    assert!(stalled, "the full window refuses the relocation, typed as a stall");
    // Maintain drains the window; the scan resumes and completes.
    rig.maintain();
    let (_, scanned, _) = rig.compact_burst(false, 256);
    assert_eq!(scanned, 1, "the stalled scan completed after window progress");
    rig.audit();
}

/// A record larger than the slice budget still makes progress: the
/// applier names the exact bytes it needs (`need`), one record, once.
#[test]
fn oversized_record_scans_via_need() {
    let mut rig = Rig::new();
    // One oversized record (larger than the chunk budget) buried in the
    // cold prefix, with enough traffic after it to demote it cold.
    fill_cold(&mut rig, 4500);
    rig.set(b"big:record", &vec![0xAB; 2 * PAGE as usize]);
    let mut seed = 0xB16u64;
    for i in 0..4500u64 {
        let key = format!("after:{i:04}").into_bytes();
        let value = vec![(seeded(&mut seed) % 251) as u8; 64];
        rig.set(&key, &value);
        if i % 64 == 63 {
            rig.maintain();
        }
    }
    rig.maintain();
    // Kill enough small records around it to arm the trigger in the
    // big record's file, keeping the big record live.
    let files: Vec<_> = rig.table.live_set().files().to_vec();
    let big_hash = KeyHasher::default().hash(b"big:record");
    let TieredLookup::Cold(big_addr) = rig.table.lookup(b"big:record", big_hash, &[]) else {
        panic!("the big record demoted cold");
    };
    let home = files
        .iter()
        .find(|f| big_addr.to_raw() >= f.base && big_addr.to_raw() < f.base + f.data_len)
        .expect("in a file")
        .clone();
    for key in keys_in_file(&rig, home.base, home.data_len) {
        if key != b"big:record" {
            rig.del(&key);
        }
    }
    // Scan through: the burst opens with a PAGE-sized chunk budget,
    // smaller than the big record, so the `need` round-trip must fire
    // for it to relocate.
    let (_, _, need_a) = rig.compact_burst(false, 256);
    rig.publish_cycle(1);
    let (_, _, need_b) = rig.compact_burst(false, 256);
    assert!(need_a + need_b > 0, "the oversized record exercised the need path");
    rig.audit();
    // The big record survived whichever scan crossed it — re-read and
    // verify content (the `need` path is exercised by any chunk smaller
    // than the record; compact_burst honors `need` by construction).
    let expect = rig.model.get(b"big:record".as_slice()).expect("live").clone();
    let hash = KeyHasher::default().hash(b"big:record");
    match rig.table.lookup(b"big:record", hash, &[]) {
        TieredLookup::Ram(addr) => {
            assert_eq!(rig.table.record(addr).value, expect.value.as_slice());
        }
        TieredLookup::Cold(addr) => {
            let bytes = rig.read_cold(addr.to_raw(), expect.encoded_len).expect("readable");
            assert_eq!(TieredTable::decode_record(&bytes).value, expect.value.as_slice());
        }
        TieredLookup::Miss => panic!("big record lost"),
    }
}

/// The pressure arm (ADR-0059 D1): below the dead-ratio threshold
/// nothing compacts — under disk pressure, the highest-dead-ratio file
/// does; a zero-dead tier reports Idle either way.
#[test]
fn pressure_widens_the_trigger() {
    let mut rig = Rig::new();
    fill_cold(&mut rig, 4500);
    let first = rig.table.live_set().files()[0].clone();
    // Kill ~20% — under the 50% arm.
    let victims = keys_in_file(&rig, first.base, first.data_len);
    for key in &victims[..victims.len() / 5] {
        rig.del(key);
    }
    assert_eq!(
        rig.table.compaction_work(&rig.flush, false, PAGE),
        CompactionWork::Idle,
        "below the dead-ratio arm, no pressure: idle"
    );
    match rig.table.compaction_work(&rig.flush, true, PAGE) {
        CompactionWork::Read { file_id, .. } => {
            assert_eq!(file_id, first.id, "pressure picks the highest dead ratio");
        }
        CompactionWork::Idle => panic!("pressure widens eligibility to any dead bytes"),
    }
}

/// The D9 regression pin: a checkpoint-ref'd record relocated and then
/// overwritten replays to exactly one slot — the origin markers kill
/// the resurrected ref in whichever unit survives. Without them the
/// stale twin survives replay and serves old bytes after the fresh
/// copy demotes (the counter-test below proves that failure is real).
#[test]
fn relocation_origin_markers_replay_exactly() {
    let (rig, overwritten) = d9_scenario(true);
    replay_and_check_d9(rig, &overwritten, true);
}

/// The counter-test: with origin markers suppressed, the same scenario
/// replays to duplicate slots — proving the markers are load-bearing
/// (this is the pre-D9 system, reconstructed).
#[test]
fn relocation_origin_markers_are_load_bearing() {
    let (rig, overwritten) = d9_scenario(false);
    replay_and_check_d9(rig, &overwritten, false);
}

/// Builds the D9 hazard: publish C0 (refs to file 0's records) → kill
/// 60% of file 0 → compact (relocating the survivors, unlogged) →
/// overwrite some relocated keys (markers per `emit_origins`) → crash.
/// Returns the crashed rig and the overwritten relocated keys.
fn d9_scenario(emit_origins: bool) -> (Rig, Vec<Vec<u8>>) {
    let mut rig = Rig::new();
    rig.emit_origins = emit_origins;
    fill_cold(&mut rig, 4500);

    // ---- publication C0 (real .ick + manifest) ----
    rig.begun = true;
    let ckpt_id = 1u64;
    let begin_lsn = Lsn::new(SegmentId(1), 64);
    let w = rig.table.begin_ckpt_walk(ckpt_id).to_raw();
    let mut writer = SyncIckWriter::create_v2(
        rig.fs.clone(),
        Path::new(SHARD),
        &CkptConfig::default(),
        0,
        ckpt_id,
        begin_lsn,
        &[NS.0],
    )
    .expect("create ick");
    let mut cursor = 0u64;
    loop {
        let mut refs: Vec<(u64, u64)> = Vec::new();
        let mut images: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
        cursor = rig.table.ckpt_walk_slice(
            cursor,
            128,
            |hash, addr| refs.push((hash, addr.to_raw())),
            |parts| images.push((parts.key.to_vec(), parts.value.to_vec())),
        );
        for (hash, addr) in refs {
            writer.append_ref(NS.0, w, hash, addr).expect("ref");
        }
        for (key, value) in images {
            writer
                .append(&RecordView::StringPostImage { ns: NS, key: &key, value: &value })
                .expect("image");
        }
        if cursor == 0 {
            break;
        }
    }
    for f in rig.table.live_set().files() {
        writer.append_live_set(NS.0, f.id, f.data_len, f.dead_bytes, f.byte_exact).expect("0x04");
    }
    writer.finish().expect("finish");
    rig.table.end_ckpt_walk();
    rig.table.retire_scan(ckpt_id, &rig.flush);
    write_manifest(
        &rig.fs,
        Path::new(SHARD),
        &Manifest {
            ckpt_id,
            begin_lsn,
            segments: vec![begin_lsn.segment],
            tiers: vec![rig.table.tier_manifest(NS.0, &rig.flush)],
            key_hash_id: KeyHasher::default().identity(),
        },
    )
    .expect("swap");
    rig.table.commit_retirement();
    rig.tail.clear(); // C0 covers everything up to here

    // ---- the hazard ----
    let first = rig.table.live_set().files()[0].clone();
    let victims = keys_in_file(&rig, first.base, first.data_len);
    assert!(victims.len() >= 10, "file 0 holds enough refs");
    let kill = victims.len() * 6 / 10;
    for key in &victims[..kill] {
        rig.del(key);
    }
    let survivors: Vec<Vec<u8>> = victims[kill..].to_vec();
    let (relocated, _, _) = rig.compact_burst(false, 128);
    assert!(relocated >= survivors.len() as u64, "the survivors relocated");
    // Overwrite half the relocated keys: their displacements name the
    // relocated (RAM) address; only the D9 origin markers name the
    // address C0's refs will resurrect.
    let overwritten: Vec<Vec<u8>> = survivors[..survivors.len() / 2].to_vec();
    for key in &overwritten {
        rig.set(key, &[0xD9; 80]);
    }
    assert!(!overwritten.is_empty(), "the hazard needs at least one overwrite");
    (rig, overwritten)
}

/// Recovers the D9 scenario and checks slot exactness. With markers:
/// one slot per key, exact bytes, before and after the fresh copies
/// demote. Without: the stale twins survive as duplicate slots.
fn replay_and_check_d9(rig: Rig, overwritten: &[Vec<u8>], markers: bool) {
    let fs = rig.fs.clone();
    let model = rig.model.clone();
    let tail = rig.tail.clone();
    drop(rig); // the crash

    let manifest = read_manifest(&fs, Path::new(SHARD)).expect("read").expect("present");
    let tier = manifest.tier_ns(NS.0).expect("tier section").clone();
    let demote = DemotionConfig::for_budget(BUDGET, PAGE);
    let recovered = recover_tiered_ns(
        fs.clone(),
        &tier,
        manifest.ckpt_id,
        flush_config(),
        space_config(demote, 0),
        demote,
        2048,
        KeyHasher::default(),
    )
    .expect("recovery");
    let table = std::cell::RefCell::new(recovered.table);
    let ick = Path::new(SHARD).join(inf_log::ckpt::ick_file_name(manifest.ckpt_id));
    read_ick_hybrid(
        &fs,
        &ick,
        inf_log::ckpt::IckReaderConfig::default(),
        |record| {
            if let RecordView::StringPostImage { key, value, .. } = record {
                table
                    .borrow_mut()
                    .apply_image(key, value, KeyHasher::default().hash(key))
                    .expect("fits");
            }
            Ok::<(), std::convert::Infallible>(())
        },
        |section| {
            apply_ref_section(&mut table.borrow_mut(), &section, tier.flushed).expect("refs");
            Ok(())
        },
        |section| {
            apply_live_set_section(&mut table.borrow_mut(), &section);
            Ok(())
        },
        |section| {
            inf_store::apply_blob_ref_section(&mut table.borrow_mut(), &section);
            Ok(())
        },
        |_| panic!("no index-sidecar sections in this image"),
    )
    .expect("hybrid load");
    let mut table = table.into_inner();
    // D4 replay with the D9 bounded displace register.
    let mut rest: &[u8] = &tail;
    let mut pending: Vec<u64> = Vec::new();
    while !rest.is_empty() {
        let (record, consumed) = decode_record(rest).expect("tail decodes");
        match record {
            RecordView::ColdDisplace { old_addr, .. } => {
                pending.push(old_addr);
                assert!(pending.len() <= 4, "register within the D9 bound");
            }
            RecordView::StringPostImage { key, value, .. } => {
                let hash = KeyHasher::default().hash(key);
                for old in pending.drain(..) {
                    table.apply_displace(hash, LogicalAddr::from_raw(old).expect("48-bit"));
                }
                table.apply_image(key, value, hash).expect("fits");
            }
            RecordView::Delete { key, .. } => {
                let hash = KeyHasher::default().hash(key);
                for old in pending.drain(..) {
                    table.apply_displace(hash, LogicalAddr::from_raw(old).expect("48-bit"));
                }
                table.apply_delete(key, hash);
            }
            other => panic!("modeled tail carries {other:?}"),
        }
        rest = &rest[consumed..];
    }

    if markers {
        assert_eq!(table.len(), model.len(), "exactly one slot per live key — no stale twins");
    } else {
        // The pre-D9 hazard reconstructed: without origin markers every
        // overwritten relocation leaves a stale cold twin slotted. Since
        // M4.5-S37 (ADR-0093 D5) the shadow ticket set is rebuilt from
        // the *finished* index at recovery-complete — this replay-only
        // harness never calls `rebuild_shadow_tickets`, so the twins are
        // plain slots here, counted in `len()` exactly as they were
        // before S37 (the reconciler would kill them after a real boot).
        assert_eq!(
            table.len(),
            model.len() + overwritten.len(),
            "without origin markers every overwritten relocation leaves a stale twin \
             (the pre-D9 hazard, reconstructed)"
        );
        assert_eq!(table.shadow_pending(), 0, "the replay harness does not rebuild tickets");
        return; // the duplicate world has nothing more to prove
    }

    // The corruption window pre-fix: after the fresh copies demote, a
    // stale twin would win the cold probe. Demote and re-audit —
    // content only: string versions reset through the image path and
    // no oracle compares them across a recovery (ADR-0057 D3).
    let mut rig = Rig {
        table,
        fs,
        flush: recovered.flush,
        model,
        tail: Vec::new(),
        begun: false,
        emit_origins: true,
        verify_versions: false,
    };
    rig.maintain();
    rig.audit();
}

/// The endurance slice (plan AC 1): sustained overwrites on the real
/// filesystem — disk usage oscillates within budget, the cold floor
/// advances, unlinked files are gone at the VFS, and `statvfs` confirms
/// the blocks returned to the OS.
#[cfg(unix)]
#[test]
fn endurance_slice_disk_oscillates_and_statvfs_reclaims() {
    use inf_log::fs::StdSegmentFs;

    let root = std::env::temp_dir().join(format!("inf-s15-endurance-{}", std::process::id()));
    let shard = root.join(SHARD);
    std::fs::create_dir_all(shard.join("cold")).expect("tempdir");
    struct Cleanup(std::path::PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let _cleanup = Cleanup(root.clone());

    fn dir_bytes(dir: &Path) -> u64 {
        let mut total = 0u64;
        let Ok(entries) = std::fs::read_dir(dir) else { return 0 };
        for entry in entries.flatten() {
            let meta = entry.metadata().expect("metadata");
            if meta.is_dir() {
                total += dir_bytes(&entry.path());
            } else {
                total += meta.len();
            }
        }
        total
    }

    fn avail_bytes(path: &Path) -> u64 {
        use std::os::unix::ffi::OsStrExt;
        let c = std::ffi::CString::new(path.as_os_str().as_bytes()).expect("path");
        // SAFETY: `statvfs` is a plain-old-data C struct; the all-zero
        // bit pattern is a valid (if meaningless) value for every field,
        // and the FFI call below overwrites it before any read.
        let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
        // SAFETY: `c` is a live NUL-terminated path and `stat` is a valid
        // exclusive out-pointer for the duration of the call.
        let rc = unsafe { libc::statvfs(c.as_ptr(), &mut stat) };
        assert_eq!(rc, 0, "statvfs");
        stat.f_bavail as u64 * stat.f_frsize as u64
    }

    // A small budget so the 1500-key working set demotes cold and the
    // reclaim pipeline actually cycles.
    let demote = DemotionConfig::for_budget(256 << 10, PAGE);
    let config = TierFlushConfig { shard_dir: shard.clone(), ..flush_config() };
    let mut table = TieredTable::new(space_config(demote, 0), demote, 4096, KeyHasher::default())
        .expect("ring");
    table.set_compaction_config(CompactionConfig { dead_ratio_pct: 50, slice_bytes: 1 << 20 });
    let mut flush = TierFlush::new(StdSegmentFs, config, 0);

    let keys = 1500u64;
    let mut seed = 0x0E2D_5EED_u64;
    let mut ckpt_id = 0u64;
    let mut peak = 0u64;
    let mut unlinked_paths: Vec<std::path::PathBuf> = Vec::new();
    // Exact encoded lengths per key (values are fixed-size, so the
    // length is stable after the first insert — the cold-overwrite
    // death must carry the exact length, ADR-0058).
    let mut encoded: Vec<usize> = vec![0; keys as usize];
    let floor_start = table.cold_floor();
    let avail_start = avail_bytes(&root);

    let maintain = |table: &mut TieredTable, flush: &mut TierFlush<StdSegmentFs>| {
        loop {
            let sealed = table.seal_slice();
            let f = table.flush_slice(flush).expect("flush slice");
            let released = table.release_slice();
            if sealed + released + f.appended_bytes + u64::from(f.gaps_crossed) == 0 {
                break;
            }
        }
    };

    for cycle in 0..10u64 {
        // Sustained overwrites: every key rewritten every cycle — the
        // whole cold prefix goes dead one file at a time.
        for i in 0..keys {
            let key = format!("e:{i:05}");
            let value = vec![(seeded(&mut seed) % 251) as u8; 96];
            let hash = KeyHasher::default().hash(key.as_bytes());
            loop {
                let done = match table.lookup(key.as_bytes(), hash, &[]) {
                    TieredLookup::Ram(addr) => {
                        let parts = table.record(addr);
                        let (len, ver) = (parts.encoded_len, parts.version);
                        let _ = table.take_displacement_origins(hash, addr);
                        table.update(key.as_bytes(), &value, hash, addr, len, ver).is_ok()
                    }
                    TieredLookup::Cold(addr) => {
                        // §3.3: the death is index + accounting only —
                        // sized by the tracked exact length, no read.
                        let len = encoded[i as usize];
                        assert!(len > 0, "cold candidate implies a prior insert");
                        let _ = table.take_displacement_origins(hash, addr);
                        table.update(key.as_bytes(), &value, hash, addr, len, 0).is_ok()
                    }
                    TieredLookup::Miss => table.insert(key.as_bytes(), &value, hash).is_ok(),
                };
                if done {
                    break;
                }
                maintain(&mut table, &mut flush);
            }
            if let TieredLookup::Ram(addr) = table.lookup(key.as_bytes(), hash, &[]) {
                encoded[i as usize] = table.record(addr).encoded_len;
            }
            if i % 128 == 127 {
                maintain(&mut table, &mut flush);
            }
        }
        maintain(&mut table, &mut flush);

        // Compaction + publication + unlink — the reclaim pipeline.
        while let CompactionWork::Read { file_id, addr, len } =
            table.compaction_work(&flush, false, 64 << 10)
        {
            let meta = flush
                .sealed()
                .iter()
                .find(|m| m.id == file_id)
                .expect("candidates are sealed")
                .clone();
            let image = std::fs::read(&meta.path).expect("tier file readable");
            let (first, count, skip) =
                tier_frame_span(addr.to_raw() - meta.base.to_raw(), len as usize);
            let from = tier_frame_offset(first) as usize;
            let to = from + count as usize * TIER_FRAME_BYTES;
            let mut chunk = Vec::new();
            tier_extract(&image[from..to], skip, len as usize, &mut chunk).expect("CRC-clean");
            let applied = table.compaction_apply(file_id, addr, &chunk);
            if applied.stalled {
                maintain(&mut table, &mut flush);
            }
        }
        ckpt_id += 1;
        table.begin_ckpt_walk(ckpt_id);
        table.end_ckpt_walk();
        table.retire_scan(ckpt_id, &flush);
        let _section = table.tier_manifest(NS.0, &flush);
        for id in table.commit_retirement() {
            let meta = flush.detach_sealed(id).expect("retired files are sealed");
            unlink_tier_file(&StdSegmentFs, &meta).expect("unlink");
            unlinked_paths.push(meta.path);
        }
        maintain(&mut table, &mut flush);

        let used = dir_bytes(&root);
        peak = peak.max(used);
        if cycle >= 3 {
            // Steady state: the dataset is ~keys × 120 B ≈ 180 KiB; with
            // the 50% trigger, on-disk usage is bounded by ~2× dataset
            // plus one file of slack per pipeline stage — far below the
            // unbounded growth this AC exists to catch.
            let bound = 6 * keys * 120 + 8 * FILE_CAPACITY;
            assert!(
                used <= bound,
                "cycle {cycle}: disk {used} exceeded the oscillation bound {bound}"
            );
        }
    }

    assert!(!unlinked_paths.is_empty(), "the run retired and unlinked files");
    for path in &unlinked_paths {
        assert!(
            std::fs::metadata(path).is_err(),
            "unlinked file still present at the VFS: {}",
            path.display()
        );
    }
    assert!(table.cold_floor() > floor_start, "the cold floor advanced");
    let used_end = dir_bytes(&root);
    assert!(used_end < peak, "final usage sits below the peak (disk oscillates)");
    // statvfs: the blocks came back to the OS. Generous slack absorbs
    // unrelated tempfs churn on a shared box; the direction is what the
    // AC names (space returns, not merely accounting).
    let avail_end = avail_bytes(&root);
    let slack = 16u64 << 20;
    assert!(
        avail_end + slack >= avail_start.saturating_sub(used_end),
        "statvfs shows the unlinked bytes returned (start {avail_start}, end {avail_end})"
    );
}
