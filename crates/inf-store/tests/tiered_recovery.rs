//! M4-S12 — the unified recovery picture at the seam tier (ADR-0057):
//! write → flush → hybrid checkpoint walk (refs below the walk
//! watermark, images above, fuzzy against interleaved mutations) →
//! MANIFEST v2 swap → crash → recovery (map tier files → seed the new
//! life at the manifested watermark → load the checkpoint → replay the
//! modeled WAL tail through the D4 rules) → byte-exact content oracle.
//!
//! The WAL tail is modeled as a stream of **real record-v1 encodings**
//! (`ColdDisplace` pairing included) held by the harness — the M2 frame/
//! segment plumbing around them is unchanged machinery; what S12 adds
//! and what this test proves is the record *application* semantics.
//! Oracles compare content and versions, never addresses (§3.1).

use std::collections::BTreeMap;
use std::path::Path;

use inf_log::fs::mem::MemFs;
use inf_log::{
    CkptConfig, Lsn, Manifest, NsId, RecordView, SegmentId, SyncIckWriter, TIER_FRAME_BYTES,
    TierFlush, TierFlushConfig, TierIoMode, decode_record, read_ick_hybrid, read_manifest,
    tier_extract, tier_frame_offset, tier_frame_span, write_manifest,
};
use inf_store::{
    AddressSpaceConfig, DemotionConfig, FileLiveSet, LogicalAddr, TieredLookup, TieredTable,
    apply_live_set_section, apply_ref_section, recover_tiered_ns,
};

const NS: NsId = NsId(41);
const PAGE: u64 = 4 << 10;
const BUDGET: u64 = 1 << 20;
const FILE_CAPACITY: u64 = 96 << 10;
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

/// The harness's model of one live key.
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
    /// Encoded record-v1 stream since `ckpt-begin` — the modeled WAL
    /// tail recovery replays (ColdDisplace pairing per ADR-0057 D4).
    tail: Vec<u8>,
    /// Whether mutations record into the tail (set at begin).
    begun: bool,
}

impl Rig {
    fn new() -> Rig {
        Self::with_demote(DemotionConfig::for_budget(BUDGET, PAGE))
    }

    fn with_demote(demote: DemotionConfig) -> Rig {
        let fs = MemFs::new();
        let table = TieredTable::new(space_config(demote, 0), demote, 2048).expect("ring");
        let flush = TierFlush::new(fs.clone(), flush_config(), 0);
        Rig { table, fs, flush, model: BTreeMap::new(), tail: Vec::new(), begun: false }
    }

    /// One MAINTAIN round: seal → flush → release until the backlog
    /// drains (release clamps at the walk watermark while one is
    /// pinned — asserted by `walk_pin_clamps_release`).
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

    /// Reads one cold record from the tier bytes through the catalog —
    /// the audit's cold path (CRC-verified by `tier_extract`).
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

    /// SET through the live-path rules, recording tail records once
    /// begun. Every displacement carries its `ColdDisplace` marker
    /// (ADR-0057 D4 — unconditional).
    fn set(&mut self, key: &[u8], value: &[u8]) {
        let hash = TieredTable::hash_key(key);
        let displaced: Option<(LogicalAddr, usize, u32)> = match self.table.lookup(key, hash, &[]) {
            TieredLookup::Ram(addr) => {
                let parts = self.table.record(addr);
                Some((addr, parts.encoded_len, parts.version))
            }
            TieredLookup::Cold(addr) => {
                // The S08 shape: fetch + verify before the overwrite.
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
                if self.begun {
                    RecordView::ColdDisplace { ns: NS, old_addr: old.to_raw() }
                        .encode_into(&mut self.tail);
                }
                self.table.update(key, value, hash, old, old_len, old_version).expect("fits");
                // In-place exact-fit keeps the address; version bumps
                // either way (the model tracks it for cold fetch sizing).
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

    /// DEL through the live-path rules (index-only for cold — §3.3),
    /// with the D4 marker once begun.
    fn del(&mut self, key: &[u8]) {
        let hash = TieredTable::hash_key(key);
        let target = match self.table.lookup(key, hash, &[]) {
            TieredLookup::Ram(addr) => Some((addr, self.table.record(addr).encoded_len)),
            TieredLookup::Cold(addr) => {
                let expect = self.model.get(key).expect("model entry");
                Some((addr, expect.encoded_len))
            }
            TieredLookup::Miss => None,
        };
        if let Some((addr, len)) = target {
            if self.begun {
                RecordView::ColdDisplace { ns: NS, old_addr: addr.to_raw() }
                    .encode_into(&mut self.tail);
                RecordView::Delete { ns: NS, key }.encode_into(&mut self.tail);
            }
            self.table.delete(hash, addr, len);
            self.model.remove(key);
        }
    }

    /// Full content audit: every model key serves its exact bytes (RAM
    /// or cold), every deleted key misses. Content, never addresses.
    fn audit(&mut self) {
        let keys: Vec<(Vec<u8>, Expect)> =
            self.model.iter().map(|(k, e)| (k.clone(), e.clone())).collect();
        for (key, expect) in keys {
            let hash = TieredTable::hash_key(&key);
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
                        exclude.push(addr); // fingerprint false positive
                    }
                    TieredLookup::Miss => {
                        panic!("live key {:?} missing", String::from_utf8_lossy(&key))
                    }
                }
            };
            assert_eq!(
                value,
                expect.value,
                "content mismatch for {:?}",
                String::from_utf8_lossy(&key)
            );
        }
    }
}

/// Replays one modeled WAL tail through the ADR-0057 D4 rules.
fn replay_tail(table: &mut TieredTable, tail: &[u8]) {
    let mut rest = tail;
    let mut pending_displace: Option<u64> = None;
    while !rest.is_empty() {
        let (record, consumed) = decode_record(rest).expect("tail records decode");
        match record {
            RecordView::ColdDisplace { old_addr, .. } => {
                assert!(pending_displace.is_none(), "displace markers never stack");
                pending_displace = Some(old_addr);
            }
            RecordView::StringPostImage { key, value, .. } => {
                let hash = TieredTable::hash_key(key);
                if let Some(old) = pending_displace.take() {
                    table.apply_displace(hash, LogicalAddr::from_raw(old).expect("48-bit"));
                }
                table.apply_image(key, value, hash).expect("fits");
            }
            RecordView::Delete { key, .. } => {
                let hash = TieredTable::hash_key(key);
                if let Some(old) = pending_displace.take() {
                    table.apply_displace(hash, LogicalAddr::from_raw(old).expect("48-bit"));
                }
                table.apply_delete(key, hash);
            }
            other => panic!("modeled tail carries no {other:?}"),
        }
        rest = &rest[consumed..];
    }
    assert!(pending_displace.is_none(), "a trailing displace marker is a stream error");
}

/// The full picture: fuzzy hybrid checkpoint + manifest v2 + crash +
/// recovery + tail replay round-trips every record byte-exactly, with
/// the walker touching zero cold state.
#[test]
fn unified_recovery_round_trips_all_classes() {
    let mut rig = Rig::new();
    let mut seed = 0x512E_C0DEu64;
    // Enough live bytes to exceed the 25% mutable target of the 1 MiB
    // budget, so phase A demotes for real (seal → flush → release).
    let keys = 2500u64;

    // Phase A (pre-begin): populate + demote so the space spans every
    // class — cold (released), flushed-but-RAM, sealed, mutable.
    for i in 0..keys {
        let key = format!("k:{i:05}").into_bytes();
        let value = vec![(seeded(&mut seed) % 251) as u8; 40 + (seeded(&mut seed) % 160) as usize];
        rig.set(&key, &value);
        if i % 64 == 63 {
            rig.maintain();
        }
    }
    rig.maintain();
    let space = rig.table.space();
    assert!(space.head().to_raw() > 0, "phase A produced released cold records");
    assert!(space.tail().to_raw() > space.flushed().to_raw(), "and unflushed RAM records");

    // Begin the fuzzy walk: latch W, then interleave walk slices with
    // post-begin mutations (the tail) and maintain rounds.
    rig.begun = true;
    let ckpt_id = 1u64;
    let w = rig.table.begin_ckpt_walk(ckpt_id).to_raw();
    let begin_lsn = Lsn::new(SegmentId(1), 64);
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
    let mut refs_emitted = 0u64;
    let mut images_emitted = 0u64;
    let mut round = 0u64;
    loop {
        let cold_resolves_before = rig.table.space().counters().cold_resolves;
        let mut refs: Vec<(u64, u64)> = Vec::new();
        let mut images: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
        cursor = rig.table.ckpt_walk_slice(
            cursor,
            64,
            |hash, addr| refs.push((hash, addr.to_raw())),
            |parts| images.push((parts.key.to_vec(), parts.value.to_vec())),
        );
        // The walker touched zero cold state (ADR-0057 D2 — structural,
        // and here observed: the resolve counter is flat across slices).
        assert_eq!(
            rig.table.space().counters().cold_resolves,
            cold_resolves_before,
            "the walker never resolves a cold address"
        );
        for (hash, addr) in refs {
            assert!(addr < w, "refs sit below the walk watermark");
            writer.append_ref(NS.0, w, hash, addr).expect("ref");
            refs_emitted += 1;
        }
        for (key, value) in images {
            writer
                .append(&RecordView::StringPostImage { ns: NS, key: &key, value: &value })
                .expect("image");
            images_emitted += 1;
        }
        if cursor == 0 {
            break;
        }
        // Fuzzy interleaving: mutations and demotion progress mid-walk.
        round += 1;
        for _ in 0..8 {
            let idx = seeded(&mut seed) % keys;
            let key = format!("k:{idx:05}").into_bytes();
            if seeded(&mut seed).is_multiple_of(5) {
                rig.del(&key);
            } else {
                let value =
                    vec![(seeded(&mut seed) % 251) as u8; 40 + (seeded(&mut seed) % 160) as usize];
                rig.set(&key, &value);
            }
        }
        if round.is_multiple_of(4) {
            rig.maintain();
        }
    }
    assert!(refs_emitted > 0, "the walk exercised the ref class");
    assert!(images_emitted > 0, "the walk exercised the image class");
    // Live-set emission (M4-S14, ADR-0058 D3): one 0x04 section per
    // namespace, after its record/ref emission — the counters cover
    // every attribution up to walk end.
    let live_files: Vec<FileLiveSet> = rig.table.live_set().files().to_vec();
    assert!(!live_files.is_empty(), "phase A filed tier files");
    assert!(
        live_files.iter().any(|f| f.dead_bytes > 0),
        "the mutation mix attributed dead bytes into files"
    );
    for f in &live_files {
        assert!(f.byte_exact && !f.recovered, "life-1 files are byte-exact by construction");
        writer
            .append_live_set(NS.0, f.id, f.data_len, f.dead_bytes, f.byte_exact)
            .expect("live set");
    }
    writer.finish().expect("finish ick");
    rig.table.end_ckpt_walk();

    // Publish the recovery unit: one manifest names {ckpt, segments,
    // tier files, watermark} (ADR-0057 D5). Flushed only advanced since
    // the walk began, so every ref is covered (asserted at decode too).
    let tier_section = rig.table.tier_manifest(NS.0, &rig.flush);
    assert!(tier_section.flushed >= w, "publication covers the walk watermark");
    write_manifest(
        &rig.fs,
        Path::new(SHARD),
        &Manifest { ckpt_id, begin_lsn, segments: vec![SegmentId(1)], tiers: vec![tier_section] },
    )
    .expect("manifest swap");

    // Post-publication tail (still replayed from begin), then crash.
    for _ in 0..200 {
        let idx = seeded(&mut seed) % keys;
        let key = format!("k:{idx:05}").into_bytes();
        if seeded(&mut seed).is_multiple_of(6) {
            rig.del(&key);
        } else {
            let value =
                vec![(seeded(&mut seed) % 251) as u8; 40 + (seeded(&mut seed) % 160) as usize];
            rig.set(&key, &value);
        }
    }
    rig.maintain();

    let fs = rig.fs.clone();
    let model = rig.model.clone();
    let tail = rig.tail.clone();
    drop(rig); // the crash: RAM state gone, durable state stays

    // ---- recovery (ADR-0057 D6) ----
    let manifest = read_manifest(&fs, Path::new(SHARD)).expect("read").expect("present");
    assert_eq!(manifest.ckpt_id, ckpt_id);
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
    )
    .expect("tier recovery");
    assert_eq!(
        recovered.table.space().life_origin().to_raw(),
        tier.flushed,
        "new life at the watermark"
    );
    assert!(recovered.stats.files_sealed + recovered.stats.files_resealed > 0);

    let table = std::cell::RefCell::new(recovered.table);
    let ick_path = Path::new(SHARD).join(inf_log::ckpt::ick_file_name(ckpt_id));
    let (info, _summary) = read_ick_hybrid(
        &fs,
        &ick_path,
        inf_log::ckpt::IckReaderConfig::default(),
        |record| {
            if let RecordView::StringPostImage { key, value, .. } = record {
                table
                    .borrow_mut()
                    .apply_image(key, value, TieredTable::hash_key(key))
                    .expect("fits");
            }
            Ok::<(), std::convert::Infallible>(())
        },
        |section| {
            apply_ref_section(&mut table.borrow_mut(), &section, tier.flushed)
                .expect("refs inside the unit");
            Ok(())
        },
        |section| {
            assert_eq!(section.ns, NS.0);
            apply_live_set_section(&mut table.borrow_mut(), &section);
            Ok(())
        },
        |section| {
            inf_store::apply_blob_ref_section(&mut table.borrow_mut(), &section);
            Ok(())
        },
    )
    .expect("hybrid load");
    assert_eq!(info.ckpt_id, ckpt_id);
    assert_eq!(info.begin_lsn, begin_lsn);
    let mut table = table.into_inner();
    replay_tail(&mut table, &tail);

    // Live-set reconciliation oracle (M4-S14, ADR-0058 D4): by
    // replay-complete, every recovered file's slot count equals the
    // index's ground truth, and restored byte counters obey the
    // sound-direction rule (dead only ever under-counts).
    assert_live_set_reconciled(&mut table, &tier);

    // The recovered rig serves every byte — cold through the recovered
    // catalog, RAM through the re-appended new life.
    let mut recovered_rig =
        Rig { table, fs, flush: recovered.flush, model, tail: Vec::new(), begun: false };
    recovered_rig.audit();

    // Post-recovery cold mutations (M4-S14): overwrite and delete
    // records that live in the *manifested* files — the pre-life death
    // routing (ADR-0058 D2) must charge the recovered file (count and
    // byte lower bound) without touching this life's space accounting.
    let cold_keys: Vec<Vec<u8>> = recovered_rig
        .model
        .keys()
        .filter(|key| {
            let hash = TieredTable::hash_key(key);
            matches!(recovered_rig.table.lookup(key, hash, &[]), TieredLookup::Cold(_))
        })
        .take(40)
        .cloned()
        .collect();
    assert!(cold_keys.len() >= 20, "the recovered majority is cold");
    let space_dead_before = recovered_rig.table.space().report().dead_bytes;
    let counted_before: u64 =
        recovered_rig.table.live_set().files().iter().map(|f| f.live_count).sum();
    for (i, key) in cold_keys.iter().enumerate() {
        if i % 3 == 0 {
            recovered_rig.del(key);
        } else {
            recovered_rig.set(key, &[0xD7; 90]);
        }
    }
    assert_eq!(
        recovered_rig.table.space().report().dead_bytes,
        space_dead_before,
        "pre-life deaths never inflate this life's space accounting"
    );
    let counted_after: u64 =
        recovered_rig.table.live_set().files().iter().map(|f| f.live_count).sum();
    assert_eq!(
        counted_after,
        counted_before - cold_keys.len() as u64,
        "every cold mutation uncounted exactly one slot"
    );
    recovered_rig.audit();

    // And the recovered pipeline keeps working: new writes flush into
    // files adjacent to the manifested set, this-life files are
    // byte-exact, and re-audit holds.
    for i in 0..64u64 {
        let key = format!("post:{i:03}").into_bytes();
        let value = vec![0xC5; 64];
        recovered_rig.set(&key, &value);
    }
    recovered_rig.maintain();
    recovered_rig.audit();
    let live_set = recovered_rig.table.live_set();
    assert!(
        live_set.files().iter().any(|f| !f.recovered && f.byte_exact),
        "this life's flush created byte-exact files"
    );
    let origin = recovered_rig.table.space().life_origin().to_raw();
    for f in live_set.files() {
        assert_eq!(f.recovered, f.base < origin, "recovered = manifested, exactly");
    }
}

/// Asserts the ADR-0058 D4 post-recovery contract: per recovered file,
/// `live_count` equals the number of index slots naming its range
/// (ground truth via a full pinned index walk — every pre-life slot
/// enumerates as a ref below the new life's flushed watermark), and
/// byte counters never over-count dead.
fn assert_live_set_reconciled(table: &mut TieredTable, tier: &inf_log::manifest::TierNsManifest) {
    let mut per_file_truth: BTreeMap<u32, u64> = BTreeMap::new();
    // A ground-truth enumeration, not a real publication — any monotone
    // id works; the boot id + 1 is what a first post-boot walk would use.
    let w = table.begin_ckpt_walk(2).to_raw();
    assert_eq!(w, tier.flushed, "nothing flushed yet in the new life");
    let mut cursor = 0u64;
    loop {
        let mut refs: Vec<u64> = Vec::new();
        cursor = table.ckpt_walk_slice(cursor, 512, |_, addr| refs.push(addr.to_raw()), |_| {});
        for addr in refs {
            let range = tier
                .files
                .iter()
                .find(|f| addr >= f.base && addr < f.base + f.durable_len)
                .expect("every pre-life slot names a manifested file");
            *per_file_truth.entry(range.id).or_default() += 1;
        }
        if cursor == 0 {
            break;
        }
    }
    table.end_ckpt_walk();
    for f in table.live_set().files() {
        assert!(f.recovered, "only manifested files exist before the new life flushes");
        assert_eq!(
            f.live_count,
            per_file_truth.get(&f.id).copied().unwrap_or(0),
            "file {} slot count reconciles with the index",
            f.id
        );
        assert!(f.dead_bytes <= f.data_len, "dead never exceeds file bytes");
        if f.byte_exact {
            assert_eq!(f.dead_bytes, f.data_len, "recovered byte-exact means fully dead");
        }
    }
}

/// The ADR-0057 D2 pin: while a walk is in flight, release stops at the
/// walk watermark even when flush advances past it; the debt drains
/// after `end_ckpt_walk`.
#[test]
fn walk_pin_clamps_release() {
    // A 1% mutable fraction so a small fill demotes aggressively — the
    // test needs flush to outrun the pinned walk watermark.
    let mut rig = Rig::with_demote(DemotionConfig {
        mem_budget_bytes: BUDGET,
        mutable_permille: 10,
        slice_bytes: PAGE,
    });
    for i in 0..200u64 {
        let key = format!("p:{i:04}").into_bytes();
        rig.set(&key, &[0x11; 200]);
    }
    rig.maintain();
    let w = rig.table.begin_ckpt_walk(1).to_raw();
    // More writes + seal + flush move `flushed` past W…
    for i in 200..400u64 {
        let key = format!("p:{i:04}").into_bytes();
        rig.set(&key, &[0x22; 200]);
    }
    loop {
        let sealed = rig.table.seal_slice();
        let f = rig.table.flush_slice(&mut rig.flush).expect("flush");
        if sealed + f.appended_bytes + u64::from(f.gaps_crossed) == 0 {
            break;
        }
    }
    assert!(rig.table.space().flushed().to_raw() > w, "flush passed the walk watermark");
    // …but release clamps at W.
    while rig.table.release_slice() > 0 {}
    assert_eq!(rig.table.space().head().to_raw(), w, "release pinned at the walk watermark");
    rig.table.end_ckpt_walk();
    while rig.table.release_slice() > 0 {}
    assert_eq!(
        rig.table.space().head(),
        rig.table.space().flushed(),
        "the pinned debt drains after the walk"
    );
}

/// D4 exactness: ref application is idempotent by `(hash, addr)`; a
/// displacement removes exactly its slot and tolerates absence. The
/// live-set counts ride exactly the actual insert/removal (M4-S14):
/// duplicates and absent removals never move a counter.
#[test]
fn ref_apply_idempotent_and_displace_exact() {
    let demote = DemotionConfig::for_budget(BUDGET, PAGE);
    let mut table = TieredTable::new(space_config(demote, 1 << 20), demote, 64).expect("ring");
    // The manifested catalog the refs land in, seeded recovery-shaped.
    table.seed_recovered_files(
        &[inf_log::TierFileMeta {
            id: 0,
            base: LogicalAddr::ZERO,
            data_len: 16384,
            reason: inf_log::tier::SealReason::Capacity,
            path: Path::new("shard-0/cold/tier-000000.itier").to_path_buf(),
        }],
        1,
    );
    let addr = LogicalAddr::from_raw(4096).expect("48-bit");
    let twin = LogicalAddr::from_raw(8192).expect("48-bit");
    let hash = 0xFEED_F00D_u64;
    table.apply_ref(hash, addr);
    table.apply_ref(hash, addr); // the walker's at-least-once duplicate
    table.apply_ref(hash, twin); // a full-hash coincidence: both live
    assert_eq!(table.len(), 2, "duplicate refs collapse; distinct addrs coexist");
    assert_eq!(table.live_set().files()[0].live_count, 2, "counts follow actual inserts");
    assert!(!table.apply_displace(hash, LogicalAddr::from_raw(12288).expect("48-bit")));
    assert_eq!(table.live_set().files()[0].live_count, 2, "absent removal moves no counter");
    assert!(table.apply_displace(hash, addr), "exact removal by (hash, addr)");
    assert!(!table.apply_displace(hash, addr), "second removal is absent");
    assert_eq!(table.len(), 1, "the twin survives — displacement is per-address");
    assert_eq!(table.live_set().files()[0].live_count, 1, "counts follow actual removals");
}

/// Regression pin for the cross-life displacement collision the
/// m4-recovery sweep found (seeds 0x514d731/0x514d19e, fixed in M4-S14):
/// addresses are per-life (§3.1), so a `ColdDisplace` marker's crashed-
/// life address can numerically equal a *different key's* slot address
/// after recovery — and the index's `(ctrl tag, addr)` match would
/// remove that key (never-none violation, ~2⁻⁷ per address collision).
/// The exact-pair discipline ADR-0057 D4 states must hold against the
/// full sidecar hash: probing many foreign hashes at a live slot's exact
/// address must never remove it.
#[test]
fn displacement_never_removes_a_foreign_key_at_a_colliding_address() {
    let demote = DemotionConfig::for_budget(BUDGET, PAGE);
    let mut table = TieredTable::new(space_config(demote, 1 << 20), demote, 64).expect("ring");
    table.seed_recovered_files(
        &[inf_log::TierFileMeta {
            id: 0,
            base: LogicalAddr::ZERO,
            data_len: 1 << 20,
            reason: inf_log::tier::SealReason::Capacity,
            path: Path::new("shard-0/cold/tier-000000.itier").to_path_buf(),
        }],
        1,
    );
    // One recovered ref slot for key J at a fixed pre-life address.
    let hash_j = TieredTable::hash_key(b"victim-key");
    let addr = LogicalAddr::from_raw(4096).expect("48-bit");
    table.apply_ref(hash_j, addr);
    assert_eq!(table.len(), 1);
    // 10k foreign hashes name J's exact address: enough trials that the
    // pre-fix (tag, addr) match collides with near-certainty, and none
    // may remove J.
    for i in 0..10_000u64 {
        let key = format!("foreign:{i}");
        let hash_k = TieredTable::hash_key(key.as_bytes());
        if hash_k == hash_j {
            continue; // a genuine 2⁻⁶⁴ coincidence would be legal removal
        }
        assert!(
            !table.apply_displace(hash_k, addr),
            "foreign hash {i} removed the victim's slot (exact-pair discipline broken)"
        );
    }
    assert_eq!(table.len(), 1, "the victim survives every foreign displacement");
    assert_eq!(table.live_set().files()[0].live_count, 1, "and stays counted in its file");
}
