//! M4.5-S30 — read-driven promotion at the seam tier (ADR-0085): the
//! second-touch admission filter, the verbatim relocation through the
//! modeled read path, the D3 skip vetoes (walk pin, demote pressure,
//! origin cap), and the D9 inheritance proof — promotion is the second
//! producer of the unlogged relocation, so the origin markers must keep
//! ADR-0057 D4's exact replay exact across promotions too, with the
//! counter-test proving they are load-bearing here as well.
//!
//! The rig mirrors `tiered_compaction.rs` (the proven scaffolding for
//! unlogged-relocation replay proofs); `get_promote` models the plane's
//! S08 read path — resolve, cold-fetch, key-verify, then offer the
//! verbatim image (ADR-0085 D1).

use std::collections::BTreeMap;
use std::path::Path;

use inf_log::fs::mem::MemFs;
use inf_log::{
    CkptConfig, Lsn, Manifest, NsId, RecordView, SegmentId, SyncIckWriter, TIER_FRAME_BYTES,
    TierFlush, TierFlushConfig, TierIoMode, decode_record, read_ick_hybrid, read_manifest,
    tier_extract, tier_frame_offset, tier_frame_span, write_manifest,
};
use inf_store::KeyHasher;
use inf_store::{
    AddressSpaceConfig, DemotionConfig, LogicalAddr, TieredLookup, TieredTable,
    apply_live_set_section, apply_ref_section, recover_tiered_ns,
};

const NS: NsId = NsId(53);
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

    /// SET with the D4 marker discipline, D9 origin markers included —
    /// the displacing mutation that stages a promoted record's origins.
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
        match displaced {
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
            }
            None => {
                self.table.insert(key, value, hash).expect("fits");
            }
        }
        if self.begun {
            RecordView::StringPostImage { ns: NS, key, value }.encode_into(&mut self.tail);
        }
        let encoded_len = match self.table.lookup(key, hash, &[]) {
            TieredLookup::Ram(addr) => self.table.record(addr).encoded_len,
            _ => unreachable!("a fresh write is RAM-resident"),
        };
        self.model.insert(key.to_vec(), Expect { value: value.to_vec(), encoded_len });
    }

    /// The plane's read path at the seam (ADR-0085 D1): resolve,
    /// cold-fetch, key-verify, then offer the verbatim image. RAM hits
    /// never reach the promotion hook — exactly the plane's shape.
    fn get_promote(&mut self, key: &[u8]) -> Vec<u8> {
        let hash = KeyHasher::default().hash(key);
        let mut exclude: Vec<LogicalAddr> = Vec::new();
        loop {
            match self.table.lookup(key, hash, &exclude) {
                TieredLookup::Ram(addr) => return self.table.record(addr).value.to_vec(),
                TieredLookup::Cold(addr) => {
                    let len = self.model.get(key).expect("live key").encoded_len;
                    let image = self.read_cold(addr.to_raw(), len).expect("cold record readable");
                    let parts = TieredTable::decode_record(&image);
                    if parts.key == key {
                        let value = parts.value.to_vec();
                        self.table.try_promote(hash, addr, &image);
                        return value;
                    }
                    exclude.push(addr);
                }
                TieredLookup::Miss => panic!("live key resolved Miss"),
            }
        }
    }

    /// Promotes one currently-cold key: quiesce MAINTAIN (so the demote
    /// veto cannot fire), then the two verified cold reads of the D2
    /// second-touch rule. Asserts the promotion actually landed.
    fn promote_key(&mut self, key: &[u8]) {
        self.maintain();
        let hash = KeyHasher::default().hash(key);
        assert!(
            matches!(self.table.lookup(key, hash, &[]), TieredLookup::Cold(_)),
            "promote_key needs a cold candidate for {:?}",
            String::from_utf8_lossy(key)
        );
        let before = self.table.promotion_counters().promotions;
        let v1 = self.get_promote(key);
        let v2 = self.get_promote(key);
        assert_eq!(v1, v2, "both reads serve the same bytes");
        assert_eq!(
            self.table.promotion_counters().promotions,
            before + 1,
            "the second verified cold read promoted {:?}",
            String::from_utf8_lossy(key)
        );
        assert!(
            matches!(self.table.lookup(key, hash, &[]), TieredLookup::Ram(_)),
            "a promoted record is RAM-resident"
        );
    }

    /// One publication cycle (walk stamps + retirement mechanics, no
    /// `.ick` emission — the replay tests below do the full form).
    fn publish_cycle(&mut self, ckpt_id: u64) {
        self.table.begin_ckpt_walk(ckpt_id);
        self.table.end_ckpt_walk();
        self.table.retire_scan(ckpt_id, &self.flush);
        let _section = self.table.tier_manifest(NS.0, &self.flush);
        self.table.commit_retirement();
    }

    fn audit(&mut self) {
        let keys: Vec<(Vec<u8>, Expect)> =
            self.model.iter().map(|(k, e)| (k.clone(), e.clone())).collect();
        for (key, expect) in keys {
            let hash = KeyHasher::default().hash(&key);
            let mut exclude: Vec<LogicalAddr> = Vec::new();
            let value = loop {
                match self.table.lookup(&key, hash, &exclude) {
                    TieredLookup::Ram(addr) => break self.table.record(addr).value.to_vec(),
                    TieredLookup::Cold(addr) => {
                        let bytes = self
                            .read_cold(addr.to_raw(), expect.encoded_len)
                            .expect("cold record readable");
                        let parts = TieredTable::decode_record(&bytes);
                        if parts.key == key.as_slice() {
                            break parts.value.to_vec();
                        }
                        exclude.push(addr);
                    }
                    TieredLookup::Miss => {
                        panic!("live key {:?} missing", String::from_utf8_lossy(&key))
                    }
                }
            };
            assert_eq!(value, expect.value, "content for {:?}", String::from_utf8_lossy(&key));
        }
    }
}

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
    let mut seed = 0x0530_C0DE_u64;
    for i in 0..keys {
        let key = format!("k:{i:05}").into_bytes();
        let value = vec![(seeded(&mut seed) % 251) as u8; 60 + (seeded(&mut seed) % 100) as usize];
        rig.set(&key, &value);
        if i % 64 == 63 {
            rig.maintain();
        }
    }
    rig.maintain();
    assert!(rig.flush.sealed().len() >= 3, "the workload spans several sealed files");
}

/// The core mechanism (ADR-0085 D2/D4): the first verified cold read
/// records a touch; the second promotes — verbatim (bytes preserved),
/// charging neither `user_bytes` nor WAL, attributing the old copy's
/// death to its file, and repointing the index to RAM.
#[test]
fn second_cold_read_promotes_verbatim() {
    let mut rig = Rig::new();
    fill_cold(&mut rig, 4500);
    let first = rig.table.live_set().files()[0].clone();
    let key = keys_in_file(&rig, first.base, first.data_len)
        .first()
        .expect("file 0 holds cold keys")
        .clone();
    let hash = KeyHasher::default().hash(&key);
    let expect_len = rig.model.get(&key).expect("live").encoded_len as u64;

    let acct_before = rig.table.write_accounting();
    let dead_before = rig.table.live_set().files()[0].dead_bytes;

    let _ = rig.get_promote(&key);
    let c = rig.table.promotion_counters();
    assert_eq!(c.first_touch, 1, "first touch recorded, no promotion");
    assert_eq!(c.promotions, 0);
    assert!(
        matches!(rig.table.lookup(&key, hash, &[]), TieredLookup::Cold(_)),
        "one touch does not promote"
    );

    let _ = rig.get_promote(&key);
    let c = rig.table.promotion_counters();
    assert_eq!(c.promotions, 1, "the second touch promoted");
    assert_eq!(c.promoted_bytes, expect_len);
    assert!(matches!(rig.table.lookup(&key, hash, &[]), TieredLookup::Ram(_)));

    let acct = rig.table.write_accounting();
    assert_eq!(acct.user_bytes, acct_before.user_bytes, "promotion never charges user bytes");
    assert_eq!(acct.wal_bytes, acct_before.wal_bytes, "promotion never stages WAL");
    assert_eq!(
        rig.table.live_set().files()[0].dead_bytes,
        dead_before + expect_len,
        "the old copy's death attributes to its file (the compaction trigger's input)"
    );
    rig.audit();
}

/// One-touch traffic — a sweep over every cold key — never promotes
/// (the ADR-0085 D2 admission rule): enumeration is not access.
#[test]
fn one_touch_sweep_never_promotes() {
    let mut rig = Rig::new();
    fill_cold(&mut rig, 4500);
    let cold: Vec<Vec<u8>> = rig
        .model
        .keys()
        .filter(|k| {
            let hash = KeyHasher::default().hash(k);
            matches!(rig.table.lookup(k, hash, &[]), TieredLookup::Cold(_))
        })
        .cloned()
        .collect();
    assert!(cold.len() > 500, "most records are cold");
    for key in &cold {
        let _ = rig.get_promote(key);
    }
    let c = rig.table.promotion_counters();
    assert_eq!(c.promotions, 0, "a one-touch sweep admits nothing");
    assert_eq!(c.first_touch as usize, cold.len());
}

/// The finding's shape at the seam: a re-read working set converges to
/// RAM — pass 1 fills the filter, pass 2 promotes, pass 3 is all RAM
/// hits — bounded at one promotion per key.
#[test]
fn promotion_converges_a_reread_working_set() {
    let mut rig = Rig::new();
    fill_cold(&mut rig, 4500);
    let hot: Vec<Vec<u8>> = rig
        .model
        .keys()
        .filter(|k| {
            let hash = KeyHasher::default().hash(k);
            matches!(rig.table.lookup(k, hash, &[]), TieredLookup::Cold(_))
        })
        .take(64)
        .cloned()
        .collect();
    assert_eq!(hot.len(), 64);
    for pass in 0..3 {
        for key in &hot {
            let _ = rig.get_promote(key);
        }
        rig.maintain();
        let resident = hot
            .iter()
            .filter(|k| {
                let hash = KeyHasher::default().hash(k);
                matches!(rig.table.lookup(k, hash, &[]), TieredLookup::Ram(_))
            })
            .count();
        if pass >= 1 {
            assert_eq!(resident, hot.len(), "the working set is resident after pass {pass}");
        }
    }
    let c = rig.table.promotion_counters();
    assert_eq!(c.promotions as usize, hot.len(), "exactly one promotion per hot key");
    rig.audit();
}

/// D3 vetoes: a pinned checkpoint walk skips (D9-1 inherited — the
/// counted `skip_pinned`), the filter tag survives the veto, and the
/// promotion lands on the next cold read after the walk ends.
#[test]
fn promotion_skips_under_walk_pin_and_retries() {
    let mut rig = Rig::new();
    fill_cold(&mut rig, 4500);
    let first = rig.table.live_set().files()[0].clone();
    let key = keys_in_file(&rig, first.base, first.data_len)[0].clone();
    let hash = KeyHasher::default().hash(&key);

    rig.table.begin_ckpt_walk(1);
    let _ = rig.get_promote(&key); // first touch
    let _ = rig.get_promote(&key); // second touch — pinned, skipped
    let c = rig.table.promotion_counters();
    assert_eq!(c.skip_pinned, 1, "the walk pin vetoed the second touch");
    assert_eq!(c.promotions, 0);
    assert!(matches!(rig.table.lookup(&key, hash, &[]), TieredLookup::Cold(_)));
    rig.table.end_ckpt_walk();

    let _ = rig.get_promote(&key); // the tag survived: this promotes
    assert_eq!(rig.table.promotion_counters().promotions, 1);
    assert!(matches!(rig.table.lookup(&key, hash, &[]), TieredLookup::Ram(_)));
}

/// D3 vetoes: a full tail window skips the promotion (the ADR-0059 D6
/// allocator-never-waiter rule — the same admission foreground obeys),
/// the filter tag survives, and the promotion lands once MAINTAIN
/// frees the window.
#[test]
fn promotion_skips_on_window_refusal_and_retries() {
    let mut rig = Rig::new();
    fill_cold(&mut rig, 4500);
    let first = rig.table.live_set().files()[0].clone();
    let key = keys_in_file(&rig, first.base, first.data_len)[0].clone();
    let hash = KeyHasher::default().hash(&key);

    let _ = rig.get_promote(&key); // first touch
    // Drive the tail window to the admission wall (alloc refusal) with
    // fills the model does not track — this test never audits.
    let mut i = 0u32;
    loop {
        let fill = format!("fill:{i:05}");
        let fill_hash = KeyHasher::default().hash(fill.as_bytes());
        if rig.table.insert(fill.as_bytes(), &[0x42; 120], fill_hash).is_err() {
            break;
        }
        i += 1;
        assert!(i < 20_000, "the window wall arms within the budget");
    }
    let _ = rig.get_promote(&key); // second touch — window full, skipped
    let c = rig.table.promotion_counters();
    assert_eq!(c.skip_window, 1, "the window refusal vetoed the promotion");
    assert_eq!(c.promotions, 0);
    assert!(matches!(rig.table.lookup(&key, hash, &[]), TieredLookup::Cold(_)));

    rig.maintain();
    let _ = rig.get_promote(&key); // the tag survived: this promotes
    assert_eq!(rig.table.promotion_counters().promotions, 1, "landed once the window freed");
}

/// D3 vetoes: repeated promote→age cycles chain origins until the D9
/// cap defers further promotion; a covering swap drains the entry and
/// promotion resumes (deferral, never a dropped origin).
#[test]
fn promotion_defers_at_the_origin_cap_until_a_covering_swap() {
    let mut rig = Rig::new();
    fill_cold(&mut rig, 4500);
    let first = rig.table.live_set().files()[0].clone();
    let key = keys_in_file(&rig, first.base, first.data_len)[0].clone();
    let hash = KeyHasher::default().hash(&key);

    // Three promote→age-to-cold cycles: the origin chain reaches the
    // RELOC_ORIGIN_CAP (3). Aging needs a mutable-target's worth of
    // fresh data behind the promoted copy before it can seal.
    for cycle in 0..3 {
        rig.promote_key(&key);
        for j in 0..2400 {
            rig.set(format!("age:{cycle}:{j:05}").as_bytes(), &[0x51; 120]);
        }
        rig.maintain(); // the promoted copy ages back to cold
        assert!(
            matches!(rig.table.lookup(&key, hash, &[]), TieredLookup::Cold(_)),
            "cycle {cycle}: the promoted copy demoted again"
        );
    }
    // Cycle 4: the second touch defers at the cap.
    rig.maintain();
    let _ = rig.get_promote(&key);
    let _ = rig.get_promote(&key);
    let c = rig.table.promotion_counters();
    assert_eq!(c.promotions, 3, "the fourth promotion deferred");
    assert_eq!(c.skip_cap, 1, "counted as the origin-cap deferral");
    assert!(matches!(rig.table.lookup(&key, hash, &[]), TieredLookup::Cold(_)));

    // A covering swap drains the origin entry; promotion resumes.
    rig.publish_cycle(1);
    let _ = rig.get_promote(&key); // the tag survived the veto
    assert_eq!(rig.table.promotion_counters().promotions, 4, "resumed after the swap");
}

/// The `tiered-promote-on-read no` arm is fully inert — no filter
/// traffic, no counters: byte-for-byte the pre-S30 read path.
#[test]
fn disabled_promotion_is_inert() {
    let mut rig = Rig::new();
    fill_cold(&mut rig, 4500);
    rig.table.set_promote_enabled(false);
    let first = rig.table.live_set().files()[0].clone();
    let key = keys_in_file(&rig, first.base, first.data_len)[0].clone();
    let hash = KeyHasher::default().hash(&key);

    for _ in 0..3 {
        let _ = rig.get_promote(&key);
    }
    assert_eq!(rig.table.promotion_counters(), Default::default(), "no counter moved");
    assert!(matches!(rig.table.lookup(&key, hash, &[]), TieredLookup::Cold(_)));

    // Re-enabling starts from a clean filter: two touches to promote.
    rig.table.set_promote_enabled(true);
    let _ = rig.get_promote(&key);
    assert_eq!(rig.table.promotion_counters().promotions, 0, "disabled touches did not count");
    let _ = rig.get_promote(&key);
    assert_eq!(rig.table.promotion_counters().promotions, 1);
}

/// The D9 inheritance proof, promotion edition: with origin markers,
/// a checkpoint whose refs name pre-promotion addresses replays to
/// exactly one slot per key (no stale twins) even after promoted
/// records are overwritten; content audits exactly.
#[test]
fn promotion_origin_markers_replay_exactly() {
    let (rig, overwritten) = promotion_d9_scenario(true);
    replay_and_check(rig, &overwritten, true);
}

/// The counter-test: with origin markers suppressed, every overwritten
/// promotion leaves a stale twin at replay — the markers are
/// load-bearing for promotions exactly as for compaction relocations.
#[test]
fn promotion_origin_markers_are_load_bearing() {
    let (rig, overwritten) = promotion_d9_scenario(false);
    replay_and_check(rig, &overwritten, false);
}

/// Promotions that were never displaced need no marker at all: the
/// checkpoint's refs resurrect the old addresses, whose bytes are
/// identical to the promoted copies (the verbatim rule) — one slot per
/// key and exact content.
#[test]
fn unmutated_promotions_replay_to_identical_bytes() {
    let (rig, _) = promotion_d9_scenario_promote_only();
    replay_and_check(rig, &[], true);
}

/// Builds the promotion hazard: publish C0 (refs to file 0's records)
/// → promote a slice of file 0's keys through the read path (unlogged
/// relocations) → overwrite half of them (markers per `emit_origins`)
/// → crash. Returns the crashed rig and the overwritten promoted keys.
fn promotion_d9_scenario(emit_origins: bool) -> (Rig, Vec<Vec<u8>>) {
    let (mut rig, promoted) = promoted_after_c0(emit_origins);
    let overwritten: Vec<Vec<u8>> = promoted[..promoted.len() / 2].to_vec();
    for key in &overwritten {
        rig.set(key, &[0x85; 80]);
    }
    assert!(!overwritten.is_empty(), "the hazard needs at least one overwrite");
    (rig, overwritten)
}

/// The promote-only arm: C0 then promotions, no displacing mutation.
fn promotion_d9_scenario_promote_only() -> (Rig, Vec<Vec<u8>>) {
    promoted_after_c0(true)
}

/// Publishes C0 (real `.ick` + manifest), then promotes a slice of
/// file 0's keys via the modeled read path.
fn promoted_after_c0(emit_origins: bool) -> (Rig, Vec<Vec<u8>>) {
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

    // ---- the unlogged relocations: promotions via the read path ----
    let first = rig.table.live_set().files()[0].clone();
    let victims = keys_in_file(&rig, first.base, first.data_len);
    assert!(victims.len() >= 10, "file 0 holds enough refs");
    let promoted: Vec<Vec<u8>> = victims[..victims.len() / 2].to_vec();
    for key in &promoted {
        rig.promote_key(key);
    }
    (rig, promoted)
}

/// Recovers the promotion scenario and checks slot exactness — the
/// same shape as `tiered_compaction.rs`'s D9 check: with markers, one
/// slot per key before and after the fresh copies demote; without,
/// every overwritten promotion leaves a stale twin.
fn replay_and_check(rig: Rig, overwritten: &[Vec<u8>], markers: bool) {
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
            other => panic!("modeled tail carries {other:?}"),
        }
        rest = &rest[consumed..];
    }

    if markers {
        assert_eq!(table.len(), model.len(), "exactly one slot per live key — no stale twins");
    } else {
        // The pre-D9 hazard reconstructed for the second producer:
        // without origin markers every overwritten promotion leaves a
        // stale cold twin. Since M4.5-S37 (ADR-0093 D5) the shadow ticket
        // set is rebuilt from the *finished* index at recovery-complete,
        // which this replay-only harness never invokes — so the twins are
        // plain slots, counted in `len()` as before S37.
        assert_eq!(
            table.len(),
            model.len() + overwritten.len(),
            "without origin markers every overwritten promotion leaves a stale twin \
             (the pre-D9 hazard, reconstructed for the second producer)"
        );
        assert_eq!(table.shadow_pending(), 0, "the replay harness does not rebuild tickets");
        return; // the duplicate world has nothing more to prove
    }

    // Demote the fresh copies and re-audit content — the corruption
    // window a stale twin would win.
    let mut rig = Rig {
        table,
        fs,
        flush: recovered.flush,
        model,
        tail: Vec::new(),
        begun: false,
        emit_origins: true,
    };
    rig.maintain();
    rig.audit();
}
