//! FCR-STTIER-01 at the store tier (ADR-0174; DRR FCR-STTIER-01 §6):
//! boot replay of a tiered tail above the RAM window through the replay
//! machine — the window rows, the rewrite and delete rows, the shadow
//! pair settled at the seal, the charged-death and unmanifested-file
//! live-set rows, the hostile input no engine writes, the identity row at
//! the seal and the end settle, the partial tail frame, the two-crash
//! row, the forced 64-bit collision, the blob census with the
//! end-of-checkpoint release, the pad rows, and the fault row.
//!
//! The oracles (§6): a `BTreeMap` model of the acknowledged history, read
//! back after recovery through RAM or the test's own tier-file reader;
//! the **key census** over every slot (keys read from RAM or the tier
//! bytes, never through the index's hash); the **dead-byte census** per
//! catalogue file; the **blob census**; the **seal-reason census**. They
//! share the operation list and the record codec with the product,
//! nothing else.
#![allow(
    clippy::disallowed_types,
    reason = "test target: std containers in test code, outside cell code (ADR-0163 D2)"
)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use inf_foundation::fault::{self, FaultSpec};
use inf_log::blob::{ExtentId, ExtentWriter};
use inf_log::fs::SegmentFs;
use inf_log::fs::mem::{MemFile, MemFs};
use inf_log::tier::{SealReason, probe_tier_file};
use inf_log::{
    CkptConfig, Lsn, Manifest, NsId, RecordView, SegmentId, SyncIckWriter, TierFileMeta, TierFlush,
    TierIoMode, TierWriter, read_ick_hybrid, read_manifest, write_manifest,
};
use inf_store::KeyHasher;
use inf_store::{
    BlobConfig, ColdKeyError, DemotionConfig, LogicalAddr, RecoveredTier, ReplayCounters,
    ReplayPhase, ReplayRefusal, SettleProgress, TierReplay, TieredLookup, TieredTable, TypeTag,
    apply_blob_ref_section, apply_live_set_section, apply_ref_section, forced_collision_pair,
    recover_tiered_ns,
};

mod support;
use support::*;

const NS: NsId = NsId(61);
const FILE_CAPACITY: u64 = 256 << 10;
fn begin() -> Lsn {
    Lsn::new(SegmentId(1), 64)
}

/// The model's record of one live key.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Expect {
    value: Vec<u8>,
    /// `Some(extent id)` when the value lives out of line.
    extent: Option<u64>,
}

/// One life of a tiered namespace on `MemFs`, written through the live
/// rules (markers for every displacement, origins first — ADR-0059 D9),
/// its tail recorded once the checkpoint began.
struct Life {
    fs: MemFs,
    demote: DemotionConfig,
    table: TieredTable,
    flush: TierFlush<MemFs>,
    model: BTreeMap<Vec<u8>, Expect>,
    tail: Vec<u8>,
    begun: bool,
    /// Record lengths written this life, by key: a cold overwrite's
    /// death needs the old record's length without a read.
    lens: BTreeMap<Vec<u8>, (usize, u32)>,
}

impl Life {
    fn new(demote: DemotionConfig) -> Life {
        let fs = MemFs::new();
        fs.create_dir_all(Path::new(SHARD)).expect("shard dir");
        let table = TieredTable::new(space_config(demote, 0), demote, 4096, KeyHasher::default())
            .expect("ring");
        let flush = TierFlush::new(fs.clone(), flush_config(NS, FILE_CAPACITY), 0);
        Life {
            fs,
            demote,
            table,
            flush,
            model: BTreeMap::new(),
            tail: Vec::new(),
            begun: false,
            lens: BTreeMap::new(),
        }
    }

    fn maintain(&mut self) {
        maintain(&mut self.table, &mut self.flush);
    }

    fn hash(key: &[u8]) -> u64 {
        KeyHasher::default().hash(key)
    }

    /// The key's live slot and the facts an overwrite or delete needs; a
    /// cold candidate is read and verified (the S08 shape), a colliding
    /// key's record excluded.
    fn displaced(&self, key: &[u8]) -> Option<(LogicalAddr, usize, u32)> {
        let mut exclude: Vec<LogicalAddr> = Vec::new();
        loop {
            match self.table.lookup(key, Self::hash(key), &exclude) {
                TieredLookup::Ram(addr) => {
                    let parts = self.table.record(addr);
                    return Some((addr, parts.encoded_len, parts.version));
                }
                TieredLookup::Cold(addr) => {
                    let head = read_cold(&self.flush, &self.fs, addr.to_raw(), 8).expect("header");
                    let len = TieredTable::record_len_from_header(&head);
                    let bytes =
                        read_cold(&self.flush, &self.fs, addr.to_raw(), len).expect("record");
                    let parts = TieredTable::decode_record(&bytes);
                    if parts.key == key {
                        return Some((addr, parts.encoded_len, parts.version));
                    }
                    exclude.push(addr);
                }
                TieredLookup::Miss => return None,
            }
        }
    }

    fn stage_markers(&mut self, key: &[u8], old: LogicalAddr) {
        let hash = Self::hash(key);
        for (origin, _) in self.table.take_displacement_origins(hash, old) {
            if self.begun {
                RecordView::ColdDisplace { ns: NS, old_addr: origin }.encode_into(&mut self.tail);
            }
        }
        if self.begun {
            RecordView::ColdDisplace { ns: NS, old_addr: old.to_raw() }.encode_into(&mut self.tail);
        }
    }

    /// A live inline `SET`, maintaining on a window refusal as MAINTAIN
    /// would park the writer.
    fn set(&mut self, key: &[u8], value: &[u8]) {
        let hash = Self::hash(key);
        let old = self.displaced(key);
        if let Some((addr, _, _)) = old {
            self.stage_markers(key, addr);
        }
        let placed = match old {
            Some((addr, len, version)) => self.table.update(key, value, hash, addr, len, version),
            None => self.table.insert(key, value, hash),
        };
        let mut placed = placed;
        let mut rounds = 0;
        while placed.is_err() {
            self.maintain();
            rounds += 1;
            assert!(rounds < 64, "the live writer parks until MAINTAIN frees the window");
            placed = match old {
                Some((addr, len, version)) => {
                    self.table.update(key, value, hash, addr, len, version)
                }
                None => self.table.insert(key, value, hash),
            };
        }
        let placed = placed.expect("fits after MAINTAIN");
        if self.begun {
            RecordView::StringPostImage { ns: NS, key, value }.encode_into(&mut self.tail);
        }
        let parts = self.table.record(placed);
        self.lens.insert(key.to_vec(), (parts.encoded_len, parts.version));
        self.model.insert(key.to_vec(), Expect { value: value.to_vec(), extent: None });
    }

    /// A live blob `SET` (ADR-0061 D3): the extent sealed first, then the
    /// reference record.
    fn set_blob(&mut self, key: &[u8], value: &[u8]) {
        let hash = Self::hash(key);
        let old = self.displaced(key);
        if let Some((addr, _, _)) = old {
            self.stage_markers(key, addr);
        }
        let extent_id = ExtentId(self.table.allocate_extent_id());
        let mut w = ExtentWriter::create(
            &self.fs,
            Path::new(SHARD),
            extent_id,
            0,
            NS,
            value.len() as u64,
            TierIoMode::Buffered,
        )
        .expect("create extent");
        w.append_chunk(value).expect("chunk");
        let sealed = w.finish().expect("finish");
        let placed = match old {
            Some((addr, len, version)) => {
                self.table.update_extent(key, hash, &sealed, addr, len, version)
            }
            None => self.table.insert_extent(key, hash, &sealed),
        }
        .expect("fits");
        if self.begun {
            RecordView::StringExtentRef {
                ns: NS,
                key,
                extent_id: extent_id.0,
                offset: 0,
                len: value.len() as u64,
            }
            .encode_into(&mut self.tail);
        }
        let parts = self.table.record(placed);
        self.lens.insert(key.to_vec(), (parts.encoded_len, parts.version));
        self.model
            .insert(key.to_vec(), Expect { value: value.to_vec(), extent: Some(extent_id.0) });
    }

    /// The shadow write (ADR-0093 D2) over the key's one exact cold
    /// candidate: probe, admit, insert, register — no marker; the ticket
    /// stays open (no reconciler runs here), so a walk emits the twin as
    /// a ref, the winner as an image and the twin's blob reference.
    fn shadow_set(&mut self, key: &[u8], value: &[u8]) {
        let hash = Self::hash(key);
        let inf_store::ShadowProbe::One(cold) = self.table.shadow_probe(key, hash) else {
            panic!("shadow_set needs exactly one exact cold candidate");
        };
        let record_len = TieredTable::RECORD_HEADER_LEN + key.len() + value.len();
        self.table.shadow_admit(hash, cold, record_len).expect("admitted");
        let winner = self.table.insert(key, value, hash).expect("fits");
        self.table.register_shadow(hash, cold, winner);
        if self.begun {
            RecordView::StringPostImage { ns: NS, key, value }.encode_into(&mut self.tail);
        }
        let parts = self.table.record(winner);
        self.lens.insert(key.to_vec(), (parts.encoded_len, parts.version));
        self.model.insert(key.to_vec(), Expect { value: value.to_vec(), extent: None });
    }

    fn del(&mut self, key: &[u8]) {
        let hash = Self::hash(key);
        if let Some((addr, len, _)) = self.displaced(key) {
            self.stage_markers(key, addr);
            if self.begun {
                RecordView::Delete { ns: NS, key }.encode_into(&mut self.tail);
            }
            self.table.delete(hash, addr, len);
            self.model.remove(key);
            self.lens.remove(key);
        }
    }

    /// Begins the checkpoint walk; mutations record into the tail from
    /// here. `mutate_mid_walk` runs between pass 0 and pass 1.
    fn checkpoint(&mut self, ckpt_id: u64, mutate_mid_walk: impl FnOnce(&mut Life)) {
        self.checkpoint_ordered(ckpt_id, mutate_mid_walk, &[], &[]);
    }

    /// [`checkpoint`](Self::checkpoint) with the images of `front` keys
    /// emitted ahead of the rest and `back` keys after them (the walk's
    /// home-group order is the hash's; a row that needs a key's image
    /// sealed during image load places it first, one that needs it
    /// sealed by the tail places it last).
    fn checkpoint_ordered(
        &mut self,
        ckpt_id: u64,
        mutate_mid_walk: impl FnOnce(&mut Life),
        front: &[&[u8]],
        back: &[&[u8]],
    ) {
        self.begun = true;
        let w = self.table.begin_ckpt_walk(ckpt_id).to_raw();
        let mut writer = SyncIckWriter::create_v2(
            self.fs.clone(),
            Path::new(SHARD),
            &CkptConfig::default(),
            0,
            ckpt_id,
            begin(),
            &[NS.0],
        )
        .expect("create ick");
        let mut cursor = 0u64;
        loop {
            let mut refs: Vec<(u64, u64)> = Vec::new();
            cursor = self.table.ckpt_walk_slice(
                cursor,
                64,
                |hash, addr| refs.push((hash, addr.to_raw())),
                |_| {},
            );
            for (hash, addr) in refs {
                writer.append_ref(NS.0, w, hash, addr).expect("ref");
            }
            if cursor == 0 {
                break;
            }
        }
        mutate_mid_walk(self);
        let mut images: Vec<(Vec<u8>, Vec<u8>, Option<inf_store::ExtentRef>)> = Vec::new();
        loop {
            cursor = self.table.ckpt_walk_slice(
                cursor,
                64,
                |_, _| {},
                |parts| images.push((parts.key.to_vec(), parts.value.to_vec(), parts.extent_ref())),
            );
            if cursor == 0 {
                break;
            }
        }
        images.sort_by_key(|(key, _, _)| {
            if front.contains(&key.as_slice()) {
                0
            } else if back.contains(&key.as_slice()) {
                2
            } else {
                1
            }
        });
        for (key, value, ext) in images {
            match ext {
                Some(ext) => writer
                    .append(&RecordView::StringExtentRef {
                        ns: NS,
                        key: &key,
                        extent_id: ext.extent_id,
                        offset: ext.offset,
                        len: ext.len,
                    })
                    .expect("extent image"),
                None => writer
                    .append(&RecordView::StringPostImage { ns: NS, key: &key, value: &value })
                    .expect("image"),
            }
        }
        for f in self.table.live_set().files().to_vec() {
            writer
                .append_live_set(NS.0, f.id, f.data_len, f.dead_bytes, f.byte_exact)
                .expect("0x04");
        }
        let blob_entries: Vec<(u64, u64, u64)> = self.table.extent_ckpt_entries().collect();
        for (addr, extent_id, len) in blob_entries {
            writer.append_blob_ref(NS.0, addr, extent_id, len).expect("0x05");
        }
        writer.finish().expect("finish ick");
        self.table.end_ckpt_walk();
        let tier = self.table.tier_manifest(NS.0, &self.flush);
        write_manifest(
            &self.fs,
            Path::new(SHARD),
            &Manifest {
                ckpt_id,
                begin_lsn: begin(),
                segments: vec![SegmentId(1)],
                tiers: vec![tier],
                key_hash_id: KeyHasher::default().identity(),
            },
        )
        .expect("manifest swap");
    }

    /// The crash: RAM state gone, the durable unit and the model stay.
    fn crash(self) -> Durable {
        Durable { fs: self.fs, demote: self.demote, model: self.model, tail: self.tail }
    }
}

/// What survives a crash: the filesystem, the configuration, the model
/// of the acknowledged history and the tail since the checkpoint began.
struct Durable {
    fs: MemFs,
    demote: DemotionConfig,
    model: BTreeMap<Vec<u8>, Expect>,
    tail: Vec<u8>,
}

/// The boot, up to the end of the checkpoint (E14 applied).
struct Boot {
    fs: MemFs,
    table: TieredTable,
    replay: TierReplay<MemFs>,
    extents_listed: Vec<u64>,
    extents_quarantined: Vec<u64>,
    tier: inf_log::TierNsManifest,
}

impl Durable {
    fn boot(&self) -> Boot {
        self.boot_with(self.demote, |_| {})
    }

    /// Recovers under `demote` (a budget raised or lowered between lives
    /// is legal), loading the checkpoint through the machine; `blob`
    /// adjusts the recovered table before the load.
    fn boot_with(&self, demote: DemotionConfig, blob: impl FnOnce(&mut TieredTable)) -> Boot {
        let manifest = read_manifest(&self.fs, Path::new(SHARD)).expect("read").expect("present");
        let tier = manifest.tier_ns(NS.0).expect("tier section").clone();
        let RecoveredTier { table, replay, extents_listed, extents_quarantined, .. } =
            recover_tiered_ns(
                self.fs.clone(),
                &tier,
                manifest.ckpt_id,
                flush_config(NS, FILE_CAPACITY),
                space_config(demote, 0),
                demote,
                4096,
                KeyHasher::default(),
            )
            .expect("tier recovery");
        let mut table = table;
        blob(&mut table);
        let table = std::cell::RefCell::new(table);
        let replay = std::cell::RefCell::new(replay);
        let ick = Path::new(SHARD).join(inf_log::ckpt::ick_file_name(manifest.ckpt_id));
        read_ick_hybrid(
            &self.fs,
            &ick,
            inf_log::ckpt::IckReaderConfig::default(),
            |record| {
                match record {
                    RecordView::StringPostImage { key, value, .. } => {
                        let hash = Life::hash(key);
                        table
                            .borrow_mut()
                            .replay_upsert(Some(&mut *replay.borrow_mut()), &[], key, value, hash)
                            .expect("replays");
                    }
                    RecordView::StringExtentRef { key, extent_id, offset, len, .. } => {
                        let hash = Life::hash(key);
                        let ext = inf_store::ExtentRef { extent_id, offset, len };
                        table
                            .borrow_mut()
                            .replay_upsert_extent(
                                Some(&mut *replay.borrow_mut()),
                                &[],
                                key,
                                hash,
                                ext,
                            )
                            .expect("replays");
                    }
                    other => panic!("no {other:?} in this checkpoint"),
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
                apply_blob_ref_section(&mut table.borrow_mut(), &section);
                Ok(())
            },
            |_| panic!("no index-sidecar sections in this image"),
        )
        .expect("hybrid load");
        let mut table = table.into_inner();
        let mut replay = replay.into_inner();
        replay.end_of_checkpoint(&mut table);
        Boot { fs: self.fs.clone(), table, replay, extents_listed, extents_quarantined, tier }
    }
}

/// The node at `Ready`: the table, the plane's pipeline and handles, the
/// boot's counters.
struct Ready {
    fs: MemFs,
    table: TieredTable,
    flush: TierFlush<MemFs>,
    handles: Vec<(u32, MemFile)>,
    counters: ReplayCounters,
    /// The manifested catalogue the boot started from.
    manifested: Vec<u32>,
}

impl Boot {
    fn replay(&mut self, tail: &[u8]) {
        replay_tail(&mut self.table, &mut self.replay, tail);
    }

    /// E12, the extent sweep seed, ADR-0093's rebuild through the
    /// machine's settle read, E13.
    fn finish(mut self) -> Ready {
        self.replay.end_of_replay(&self.table);
        while self.replay.settle_step(&mut self.table, PAGE).expect("settle step")
            == SettleProgress::More
        {}
        let revive = self.table.extent_sweep_seed(&self.extents_listed, &self.extents_quarantined);
        assert!(revive.is_empty(), "nothing quarantined in these lives");
        let replay = &mut self.replay;
        self.table
            .rebuild_shadow_tickets(|slot| -> Result<inf_store::KeyWindow, String> {
                let window = replay.read_key_window(slot.cold).map_err(|e| e.to_string())?;
                Ok(inf_store::KeyWindow { bytes: window.bytes.to_vec(), left: window.left })
            })
            .expect("rebuild");
        let counters = self.replay.counters();
        let manifested = self.tier.files.iter().map(|f| f.id).collect();
        let handed = self.replay.hand_over(&mut self.table).expect("hands over");
        Ready {
            fs: self.fs,
            table: self.table,
            flush: handed.flush,
            handles: handed.handles,
            counters,
            manifested,
        }
    }
}

/// A slot as the census sees it: the key, its class and what the record
/// holds, read from RAM or the tier bytes.
struct Slot {
    addr: LogicalAddr,
    key: Vec<u8>,
    value: Vec<u8>,
    len: u64,
    tag: TypeTag,
    cold: bool,
}

impl Ready {
    fn hash(key: &[u8]) -> u64 {
        KeyHasher::default().hash(key)
    }

    /// Every slot, read from RAM or the tier bytes — never through the
    /// index's hash path.
    fn slots(&self) -> Vec<Slot> {
        let mut out = Vec::new();
        let mut cursor = 0u64;
        loop {
            let mut batch: Vec<LogicalAddr> = Vec::new();
            cursor = self.table.scan_slots(cursor, 256, |_hash, addr| batch.push(addr));
            for addr in batch {
                let cold = addr < self.table.space().head();
                let bytes = if cold {
                    let head = read_cold(&self.flush, &self.fs, addr.to_raw(), 8)
                        .expect("a cold slot's header lies in a catalogued file");
                    let len = TieredTable::record_len_from_header(&head);
                    read_cold(&self.flush, &self.fs, addr.to_raw(), len)
                        .expect("a cold slot's record lies in a catalogued file")
                } else {
                    let len = self.table.record(addr).encoded_len;
                    self.table.record_bytes(addr, len).to_vec()
                };
                let parts = TieredTable::decode_record(&bytes);
                out.push(Slot {
                    addr,
                    key: parts.key.to_vec(),
                    value: parts.value.to_vec(),
                    len: parts.encoded_len as u64,
                    tag: parts.type_tag,
                    cold,
                });
            }
            if cursor == 0 {
                break out;
            }
        }
    }

    /// The key census (§6): (a) the keys and values equal the model's;
    /// (b) no two cold slots carry one key; (c) a RAM slot and a cold
    /// slot of one key only where an open ticket names the pair, the
    /// RAM record `Open`; (d) no such pair at all in a namespace that
    /// demoted.
    fn key_census(&self, model: &BTreeMap<Vec<u8>, Expect>, demoted: bool) {
        let slots = self.slots();
        let keys: BTreeSet<&[u8]> = slots.iter().map(|s| s.key.as_slice()).collect();
        let expected: BTreeSet<&[u8]> = model.keys().map(Vec::as_slice).collect();
        assert_eq!(keys, expected, "(a) the slotted keys are the model's");
        for key in model.keys() {
            let hash = Self::hash(key);
            let mut exclude = Vec::new();
            let value = loop {
                match self.table.lookup(key, hash, &exclude) {
                    TieredLookup::Ram(addr) => break self.table.record(addr).value.to_vec(),
                    TieredLookup::Cold(addr) => {
                        let slot = slots.iter().find(|s| s.addr == addr).expect("slotted");
                        if slot.key == *key {
                            break slot.value.clone();
                        }
                        exclude.push(addr);
                    }
                    TieredLookup::Miss => panic!("(a) {:?} missing", String::from_utf8_lossy(key)),
                }
            };
            let expect = &model[key];
            match expect.extent {
                None => assert_eq!(value, expect.value, "(a) {:?}", String::from_utf8_lossy(key)),
                Some(id) => {
                    let ext = inf_store::ExtentRef::decode(&value);
                    assert_eq!(ext.extent_id, id, "(a) {:?} extent", String::from_utf8_lossy(key));
                }
            }
        }
        let mut cold_keys: BTreeMap<&[u8], u32> = BTreeMap::new();
        for s in slots.iter().filter(|s| s.cold) {
            *cold_keys.entry(&s.key).or_default() += 1;
        }
        assert!(
            cold_keys.values().all(|&n| n == 1),
            "(b) two cold slots carry one key: {:?}",
            cold_keys.iter().filter(|(_, n)| **n > 1).map(|(k, _)| String::from_utf8_lossy(k))
        );
        let tickets: Vec<(LogicalAddr, LogicalAddr)> =
            self.table.shadow_tickets().map(|t| (t.cold, t.winner)).collect();
        for ram in slots.iter().filter(|s| !s.cold) {
            for cold in slots.iter().filter(|s| s.cold && s.key == ram.key) {
                assert!(!demoted, "(d) same-key pair in a namespace that demoted: {:?}", ram.key);
                assert!(
                    tickets.contains(&(cold.addr, ram.addr)),
                    "(c) a same-key pair with no ticket: {:?}",
                    String::from_utf8_lossy(&ram.key)
                );
                assert!(ram.addr >= self.table.space().ro_boundary(), "(c) the RAM record is Open");
            }
        }
    }

    /// The dead-byte census (§6, I13): per catalogue file, the bytes the
    /// slots name read from tier bytes; a recovered file's `dead_bytes`
    /// never exceeds `data_len −` that sum, a boot-sealed file is
    /// byte-exact with equality.
    fn dead_byte_census(&self) {
        let slots = self.slots();
        for f in self.table.live_set().files() {
            // Every slot inside the file's range, cold or still RAM-resident
            // between `head` and `flushed`: the file holds them all.
            let live: u64 = slots
                .iter()
                .filter(|s| s.addr.to_raw() >= f.base && s.addr.to_raw() < f.base + f.data_len)
                .map(|s| s.len)
                .sum();
            let truth = f.data_len - live;
            if f.recovered {
                assert!(
                    f.dead_bytes <= truth,
                    "recovered file {} over-counts dead: {} > {truth} (I13)",
                    f.id,
                    f.dead_bytes
                );
            } else {
                assert!(f.byte_exact, "boot file {} is byte-exact (I14)", f.id);
                assert_eq!(f.dead_bytes, truth, "boot file {} dead bytes are exact", f.id);
            }
        }
    }

    /// The blob census (§6, I12): the reference map's addresses equal
    /// the slotted extent-typed records'.
    fn blob_census(&self) {
        let slotted: BTreeSet<u64> = self
            .slots()
            .iter()
            .filter(|s| s.tag == TypeTag::StringExtent)
            .map(|s| s.addr.to_raw())
            .collect();
        let mapped: BTreeSet<u64> = self.table.extent_references().map(|(a, _, _)| a).collect();
        assert_eq!(mapped, slotted, "the reference map names exactly the slotted extent records");
    }

    /// The seal-reason census (§6): every boot file's footer carries a
    /// reason the live flush or the hand-over gives — never the stall
    /// reason, never `Recovered`.
    fn seal_reason_census(&self) {
        for meta in self.flush.sealed() {
            if self.manifested.contains(&meta.id) {
                continue;
            }
            let (_, footer) = probe_tier_file(&self.fs, &meta.path).expect("probe");
            let reason = footer.expect("a boot file is sealed").reason;
            assert!(
                matches!(
                    reason,
                    SealReason::Capacity | SealReason::RingTopGap | SealReason::Shutdown
                ),
                "boot file {} sealed {reason:?}",
                meta.id
            );
        }
    }

    fn boot_files(&self) -> Vec<&TierFileMeta> {
        self.flush.sealed().iter().filter(|m| !self.manifested.contains(&m.id)).collect()
    }

    /// Every census, plus the handle count (I11).
    fn audit(&self, model: &BTreeMap<Vec<u8>, Expect>, demoted: bool) {
        self.key_census(model, demoted);
        self.dead_byte_census();
        self.blob_census();
        self.seal_reason_census();
        assert_eq!(self.handles.len(), self.flush.sealed().len(), "one handle per sealed file");
        assert!(self.flush.active().is_none(), "the hand-over sealed the active file");
    }
}

/// A budget lowered between lives (`INF.NS SET … MEM-BUDGET` on the
/// running node, then a stop): the images alone exceed the new window,
/// so image load itself demotes (DRR FCR-STTIER-01 §6's extent-typed
/// twin row; the shape every row that needs a seal during image load
/// uses).
fn lowered() -> DemotionConfig {
    DemotionConfig::for_budget(BUDGET / 4, PAGE)
}

fn zero_set(c: ReplayCounters) -> u64 {
    c.demote_steps
        + c.pads_placed
        + c.tier_bytes
        + c.barriers
        + c.files_sealed
        + c.settle_reads
        + c.settled_same_key
        + c.settled_distinct
        + c.deletes_verified
        + c.blob_releases
}

fn page_ceil(bytes: u64) -> u64 {
    bytes.div_ceil(PAGE) * PAGE
}

/// A life whose checkpoint names nothing (everything flushed and
/// released before it began), then `n` distinct tail records of
/// `value_len` bytes.
fn life_with_tail(n: u64, value_len: usize) -> (Durable, u64) {
    let demote = DemotionConfig::for_budget(BUDGET, PAGE);
    let mut life = Life::new(demote);
    life.checkpoint(1, |_| {});
    let value = vec![0xAB; value_len];
    let mut record_len = 0u64;
    for i in 0..n {
        let key = format!("tail:{i:06}").into_bytes();
        life.set(&key, &value);
        record_len = life.lens[&key].0 as u64;
        if i.is_multiple_of(64) {
            life.maintain();
        }
    }
    life.maintain();
    (life.crash(), record_len)
}

// ---- §6 row 1: committed pages at the end of replay ------------------------

/// Window − 1 page and window: a boot that fits — zero demote steps, the
/// zero set at zero, no tier file created, no record byte appended (I9);
/// window + 1 page, 3 × and 16 ×: at least one. Every boot serves the
/// model.
#[test]
fn committed_pages_around_the_window_decide_whether_the_boot_demotes() {
    let demote = DemotionConfig::for_budget(BUDGET, PAGE);
    let window = demote.mem_budget_bytes + demote.slice_bytes;
    let record = {
        let (_, len) = life_with_tail(1, 1000 - 20);
        len
    };
    for (target, demotes) in [
        (window - PAGE, false),
        (window, false),
        (window + PAGE, true),
        (3 * window, true),
        (16 * window, true),
    ] {
        let n = target / record;
        assert_eq!(page_ceil(n * record), target, "the tail commits exactly the target pages");
        let (durable, _) = life_with_tail(n, 1000 - 20);
        let mut boot = durable.boot();
        let files_before = boot.replay.sealed().len();
        boot.replay(&durable.tail);
        let counters = boot.replay.counters();
        if demotes {
            assert!(counters.demote_steps >= 1, "{target}: the boot demoted");
            assert_eq!(boot.replay.phase(), ReplayPhase::Spilling);
        } else {
            assert_eq!(zero_set(counters), 0, "{target}: the zero set is zero (I9)");
            assert_eq!(boot.replay.phase(), ReplayPhase::Fitting);
            assert_eq!(boot.replay.sealed().len(), files_before, "{target}: no tier file");
            assert!(boot.replay.active().is_none(), "{target}: no record byte appended");
            assert_eq!(boot.table.space().report().committed_bytes, target);
        }
        assert!(boot.table.space().report().committed_bytes <= window, "I1 at {target}");
        let ready = boot.finish();
        ready.audit(&durable.model, demotes);
    }
}

// ---- §6 rows: rewrite distance, rewrites still open, deletes -----------

/// Rewrites of one key under a window, over a window, and a tail that is
/// one key rewritten throughout: one slot, the newest value.
#[test]
fn a_rewritten_key_keeps_one_slot_with_the_newest_value() {
    let demote = DemotionConfig::for_budget(BUDGET, PAGE);
    let window = demote.mem_budget_bytes + demote.slice_bytes;
    let value = vec![0x33; 900];
    for distance in [window / 4, 2 * window] {
        let mut life = Life::new(demote);
        life.checkpoint(1, |_| {});
        life.set(b"hot", b"first");
        let mut written = 0u64;
        let mut i = 0u64;
        while written < distance {
            let key = format!("filler:{i:06}").into_bytes();
            life.set(&key, &value);
            written += life.lens[&key].0 as u64;
            i += 1;
            if i.is_multiple_of(64) {
                life.maintain();
            }
        }
        life.set(b"hot", b"second");
        life.maintain();
        let durable = life.crash();
        let mut boot = durable.boot();
        boot.replay(&durable.tail);
        let demoted = boot.replay.counters().demote_steps > 0;
        assert_eq!(demoted, distance > window, "the regime engages above the window");
        let ready = boot.finish();
        ready.audit(&durable.model, demoted);
        let hot: Vec<_> = ready.slots().into_iter().filter(|s| s.key == b"hot").collect();
        assert_eq!(hot.len(), 1, "one slot for the rewritten key");
        assert_eq!(hot[0].value, b"second");
    }
    // Every record the same key, three windows of rewrites.
    let mut life = Life::new(demote);
    life.checkpoint(1, |_| {});
    let mut written = 0u64;
    let mut i = 0u64;
    while written < 3 * window {
        let value = format!("v{i}").into_bytes();
        life.set(b"same", &value);
        written += life.lens[b"same".as_slice()].0 as u64;
        i += 1;
    }
    life.maintain();
    let durable = life.crash();
    let mut boot = durable.boot();
    boot.replay(&durable.tail);
    assert!(boot.replay.counters().demote_steps > 0, "three windows of one key demote");
    let ready = boot.finish();
    ready.audit(&durable.model, true);
    assert_eq!(ready.slots().len(), 1, "one slot");
}

/// More than 4 096 keys demoted by the boot, then rewritten inside the
/// last window: the end-of-replay settle (E12) removes every demoted
/// twin, so the rebuild tickets nothing, the census holds, and the first
/// `SET` after `Ready` is admitted (through MAINTAIN when the window is
/// full, never a pinned stall). Red under `inf_canary_replay_no_end_settle`:
/// the rebuild tickets 4 096 same-key pairs in a namespace that demoted.
#[test]
fn rewrites_still_open_at_the_end_of_replay_are_settled_before_ready() {
    let demote = DemotionConfig::for_budget(BUDGET, PAGE);
    let window = demote.mem_budget_bytes + demote.slice_bytes;
    let keys = 4200u64;
    let mut life = Life::new(demote);
    life.checkpoint(1, |_| {});
    for i in 0..keys {
        life.set(&format!("open:{i:05}").into_bytes(), &[0x11; 150]);
        if i.is_multiple_of(64) {
            life.maintain();
        }
    }
    let mut written = 0u64;
    let mut i = 0u64;
    while written < window {
        let key = format!("filler:{i:06}").into_bytes();
        life.set(&key, &[0x22; 900]);
        written += life.lens[&key].0 as u64;
        i += 1;
        if i.is_multiple_of(64) {
            life.maintain();
        }
    }
    for i in 0..keys {
        life.set(&format!("open:{i:05}").into_bytes(), &[0x33; 150]);
        if i.is_multiple_of(64) {
            life.maintain();
        }
    }
    life.maintain();
    let durable = life.crash();
    let mut boot = durable.boot();
    boot.replay(&durable.tail);
    assert!(boot.replay.counters().demote_steps > 0);
    boot.replay.end_of_replay(&boot.table);
    let before = boot.replay.counters();
    let mut steps = 0u32;
    while boot.replay.settle_step(&mut boot.table, PAGE).expect("settle") == SettleProgress::More {
        steps += 1;
    }
    let after = boot.replay.counters();
    let ready = boot.finish();
    // The oracle first: census (d) finds the ticketed same-key pairs of a
    // namespace that demoted when the end settle was skipped.
    ready.audit(&durable.model, true);
    assert_eq!(ready.table.shadow_pending(), 0, "no same-key ticket at Ready");
    // Then the engagement.
    assert!(steps > 1, "the end settle yields at the step budget");
    assert!(
        after.settled_same_key - before.settled_same_key >= keys,
        "E12 settled every rewritten key's demoted twin"
    );
    let mut ready = ready;
    let hash = Ready::hash(b"after-ready");
    if ready.table.insert(b"after-ready", b"ok", hash).is_err() {
        assert!(
            ready.table.write_stall_target(b"after-ready", b"ok").is_some(),
            "a full window parks on the demotion cycle, not on a pin"
        );
        maintain(&mut ready.table, &mut ready.flush);
        ready.table.insert(b"after-ready", b"ok", hash).expect("the first SET after Ready");
    }
}

/// A `DEL` of a demoted key, `DEL` then `SET`, `SET` then `DEL`: absent
/// or present as the model says; the verified delete counted. Red under
/// `inf_canary_replay_del_no_verify` (E5 skipped): the deleted key's
/// demoted copy serves.
#[test]
fn deletes_in_the_tail_resolve_against_demoted_copies() {
    let demote = DemotionConfig::for_budget(BUDGET, PAGE);
    let window = demote.mem_budget_bytes + demote.slice_bytes;
    let mut life = Life::new(demote);
    life.checkpoint(1, |_| {});
    life.set(b"del-demoted", &[0x44; 300]);
    life.set(b"del-then-set", &[0x55; 300]);
    life.set(b"set-then-del", &[0x66; 300]);
    let mut written = 0u64;
    let mut i = 0u64;
    while written < 2 * window {
        let key = format!("filler:{i:06}").into_bytes();
        life.set(&key, &[0x22; 900]);
        written += life.lens[&key].0 as u64;
        i += 1;
        if i.is_multiple_of(64) {
            life.maintain();
        }
    }
    life.del(b"del-demoted");
    life.del(b"del-then-set");
    life.set(b"del-then-set", b"back");
    life.set(b"set-then-del", b"newer");
    life.del(b"set-then-del");
    life.maintain();
    let durable = life.crash();
    let mut boot = durable.boot();
    boot.replay(&durable.tail);
    let counters = boot.replay.counters();
    let ready = boot.finish();
    // The oracle first: census (a) finds a deleted key present when E5
    // skipped its reads.
    ready.audit(&durable.model, true);
    assert!(counters.demote_steps > 0);
    assert!(counters.deletes_verified >= 2, "the DELs of demoted keys verified their copies");
    assert!(matches!(
        ready.table.lookup(b"del-demoted", Ready::hash(b"del-demoted"), &[]),
        TieredLookup::Miss
    ));
    assert!(matches!(
        ready.table.lookup(b"set-then-del", Ready::hash(b"set-then-del"), &[]),
        TieredLookup::Miss
    ));
    assert!(ready.slots().iter().any(|s| s.key == b"del-then-set" && s.value == b"back"));
}

// ---- §6 rows: the shadow pair in the unit, the charged death ----------------

/// A shadow pair in the unit — the twin a ref, the winner an image, no
/// marker — with the winner sealed by the boot: E10 settles the ref at
/// the seal, one slot survives, the ref's address rides the survivor's
/// origins (I10). Red under `inf_canary_replay_seal_no_settle`: census
/// (b), two cold slots with one key.
#[test]
fn a_shadow_pair_in_the_unit_settles_at_the_sealed_winner() {
    let demote = DemotionConfig::for_budget(BUDGET, PAGE);
    let mut life = Life::new(demote);
    life.table.set_shadow_enabled(true);
    life.set(b"paired", &[0x77; 400]);
    for i in 0..600u64 {
        life.set(&format!("cold:{i:05}").into_bytes(), &[0x11; 900]);
        if i.is_multiple_of(64) {
            life.maintain();
        }
    }
    life.maintain();
    assert!(matches!(
        life.table.lookup(b"paired", Life::hash(b"paired"), &[]),
        TieredLookup::Cold(_)
    ));
    for i in 0..600u64 {
        life.set(&format!("img:{i:05}").into_bytes(), &[0x11; 900]);
    }
    life.shadow_set(b"paired", &[0x88; 400]);
    life.checkpoint_ordered(1, |_| {}, &[b"paired"], &[]);
    for i in 0..200u64 {
        life.set(&format!("tail:{i:06}").into_bytes(), &[0x22; 900]);
    }
    let durable = life.crash();
    let mut boot = durable.boot_with(lowered(), |_| {});
    let hash = Life::hash(b"paired");
    let after_images = boot.replay.counters();
    boot.replay(&durable.tail);
    let winner = match boot.table.lookup(b"paired", hash, &[]) {
        TieredLookup::Ram(a) | TieredLookup::Cold(a) => a,
        TieredLookup::Miss => panic!("paired is live"),
    };
    let origins = boot.table.displacement_origins_len(hash, winner);
    let ready = boot.finish();
    // The oracle first: the census sees two cold slots with one key when
    // the seal skipped its settle.
    ready.audit(&durable.model, true);
    assert_eq!(ready.slots().iter().filter(|s| s.key == b"paired").count(), 1, "one slot");
    // Then the engagement: image load demoted and settled the ref at the
    // winner's seal, and the ref's address rides the survivor's origins.
    assert!(after_images.demote_steps > 0, "image load demoted under the lowered budget");
    assert!(after_images.settled_same_key >= 1, "the ref settled against its sealed winner");
    assert_eq!(origins, 1, "the ref's address rides the survivor's origins (I10)");
}

/// A death the crashed life already charged: a verified overwrite of a
/// cold key between its ref (pass 0) and its file's live-set entry
/// (pass 2), with a window of tail records written before it, so the
/// boot seals the key's image — placed at image load — before the
/// overwrite's marker replays. The settle counts and stamps and charges
/// no bytes (R8); the dead-byte census holds. Red under
/// `inf_canary_replay_ref_settle_charges`: the recovered file above its
/// true dead bytes.
#[test]
fn a_death_the_crashed_life_charged_is_not_charged_again() {
    let demote = DemotionConfig::for_budget(BUDGET, PAGE);
    let window = demote.mem_budget_bytes + demote.slice_bytes;
    let mut life = Life::new(demote);
    life.set(b"charged", &[0x77; 400]);
    for i in 0..600u64 {
        life.set(&format!("cold:{i:05}").into_bytes(), &[0x11; 900]);
        if i.is_multiple_of(64) {
            life.maintain();
        }
    }
    life.maintain();
    assert!(matches!(
        life.table.lookup(b"charged", Life::hash(b"charged"), &[]),
        TieredLookup::Cold(_)
    ));
    life.checkpoint_ordered(
        1,
        |life| {
            // Half a window of tail records fits beside the pinned walk's
            // RAM, then the overwrite: the marker follows them in the tail.
            let mut written = 0u64;
            let mut i = 0u64;
            while written < window / 2 {
                let key = format!("tail:{i:06}").into_bytes();
                life.set(&key, &[0x22; 900]);
                written += life.lens[&key].0 as u64;
                i += 1;
            }
            life.set(b"charged", &[0x88; 400]);
        },
        &[],
        &[b"charged"],
    );
    let durable = life.crash();
    let hash = Life::hash(b"charged");
    let mut boot = durable.boot_with(lowered(), |_| {});
    let origin = boot.table.space().life_origin();
    let pre_life_before = {
        let mut n = 0;
        let mut cursor = 0u64;
        loop {
            cursor = boot.table.scan_slots(cursor, 256, |h, a| {
                if h == hash && a < origin {
                    n += 1;
                }
            });
            if cursor == 0 {
                break n;
            }
        }
    };
    assert_eq!(pre_life_before, 1, "the ref survived image load: the image was placed last");
    let charged_file = boot
        .table
        .live_set()
        .files()
        .iter()
        .find(|f| f.recovered && f.dead_bytes > 0)
        .map(|f| (f.id, f.dead_bytes))
        .expect("the live-set entry restored the crashed life's charge");
    boot.replay(&durable.tail);
    let counters = boot.replay.counters();
    let ready = boot.finish();
    // The oracle first: the dead-byte census finds a recovered file above
    // its true dead bytes when the settle charged the death again.
    ready.audit(&durable.model, true);
    let after =
        ready.table.live_set().files().iter().find(|f| f.id == charged_file.0).expect("file");
    assert_eq!(after.dead_bytes, charged_file.1, "the settle charged no bytes (R8)");
    // Then the engagement.
    assert!(counters.demote_steps > 0, "the boot demoted");
    assert!(
        counters.settled_same_key >= 1,
        "the ref settled at the image's seal, before its marker"
    );
    assert_eq!(ready.slots().iter().filter(|s| s.key == b"charged").count(), 1, "one slot");
    let _ = origin;
}

// ---- §6 row: a live-set entry for an unmanifested file (E15) -----------------

/// ADR-0174 R11 (E15) at its narrowest: a live-set entry whose id names
/// a file this boot's flush created — equal length and all — restores
/// nothing: a boot file's counters are its own. Red before the rule, and
/// red under `inf_canary_replay_restore_unguarded`: the entry overwrote
/// the boot file's dead bytes and its byte-exactness.
#[test]
fn a_live_set_entry_naming_a_boot_file_restores_nothing() {
    let demote = DemotionConfig::for_budget(BUDGET, PAGE);
    let mut life = Life::new(demote);
    life.checkpoint(1, |_| {});
    for i in 0..1400u64 {
        life.set(&format!("k:{i:04}").into_bytes(), &[0x5A; 900]);
        if i.is_multiple_of(64) {
            life.maintain();
        }
    }
    life.maintain();
    let durable = life.crash();
    let mut boot = durable.boot();
    boot.replay(&durable.tail);
    assert!(boot.replay.counters().demote_steps > 0, "the boot filed a file of its own");
    let boot_file =
        boot.table.live_set().files().iter().find(|f| !f.recovered).cloned().expect("filed");
    boot.table.restore_live_entry(&inf_log::LiveSetFileEntry {
        file_id: boot_file.id,
        data_len: boot_file.data_len,
        dead_bytes: boot_file.data_len / 2,
        byte_exact: false,
    });
    let ready = boot.finish();
    // The oracle first: a boot file that is not byte-exact.
    ready.dead_byte_census();
    let after =
        ready.table.live_set().files().iter().find(|f| f.id == boot_file.id).expect("filed");
    assert_eq!(after.dead_bytes, boot_file.dead_bytes, "a boot file's counters are its own (R11)");
    assert!(after.byte_exact);
}

/// The crashed life's last file filed under one frame and never named
/// by the manifest; image load demotes and reuses its id: the live-set
/// entry naming that id restores nothing, and the boot file stays
/// byte-exact.
#[test]
fn an_unmanifested_file_reused_by_the_boot_stays_byte_exact() {
    let demote = DemotionConfig::for_budget(BUDGET, PAGE);
    let window = demote.mem_budget_bytes + demote.slice_bytes;
    let mut life = Life::new(demote);
    // A window of RAM images at the walk; the boot's lowered budget makes
    // image load demote.
    for i in 0..1000u64 {
        life.set(&format!("img:{i:05}").into_bytes(), &[0x11; 900]);
    }
    assert!(
        life.table.space().tail().to_raw() - life.table.space().flushed().to_raw() > window / 2
    );
    // The crashed life's last file: filed under one frame, its seal never
    // confirmed — the manifest will not name it, the live set will.
    let tail = life.table.space().tail();
    life.table.space_mut().advance_ro_boundary(tail);
    let before = life.flush.sealed().len();
    life.table.flush_slice(&mut life.flush).expect("one slice");
    assert_eq!(life.flush.sealed().len(), before);
    let (unnamed_id, _, data_len, _, _) = life.flush.active().expect("filed");
    assert!(data_len > 0);
    // The walk watermark is `flushed`, below the filed-but-unconfirmed
    // bytes: every record images, the file's entry rides the live set.
    life.checkpoint(1, |_| {});
    let durable = life.crash();
    let manifest = read_manifest(&durable.fs, Path::new(SHARD)).expect("read").expect("present");
    let tier = manifest.tier_ns(NS.0).expect("section");
    assert!(tier.files.iter().all(|f| f.id != unnamed_id), "the manifest does not name it");
    let boot = durable.boot_with(lowered(), |_| {});
    assert!(boot.replay.counters().demote_steps > 0, "the images demoted");
    assert!(
        boot.replay.sealed().iter().any(|m| m.id == unnamed_id)
            || boot.replay.active().is_some_and(|(id, ..)| id == unnamed_id),
        "the boot reused the dead-life id"
    );
    let ready = boot.finish();
    ready.audit(&durable.model, true);
    let reused = ready.table.live_set().files().iter().find(|f| f.id == unnamed_id).expect("filed");
    assert!(reused.byte_exact && !reused.recovered, "the boot file's counters are its own (R11)");
}

// ---- §6 rows: input no engine writes; the identity row --------------------

/// Four refs of one key under one sealed winner: the fourth same-key
/// settle has no origin room — the typed refusal of E10, never a panic,
/// the first three settled exactly.
#[test]
fn four_refs_of_one_key_under_one_sealed_winner_refuse_typed() {
    let demote = DemotionConfig::for_budget(BUDGET, PAGE);
    let window = demote.mem_budget_bytes + demote.slice_bytes;
    let mut life = Life::new(demote);
    // Four copies of the key's record, every one in a tier file.
    let mut copies: Vec<u64> = Vec::new();
    for g in 0..4u8 {
        life.set(b"quad", &[g; 300]);
        let hash = Life::hash(b"quad");
        let TieredLookup::Ram(addr) = life.table.lookup(b"quad", hash, &[]) else { panic!() };
        copies.push(addr.to_raw());
        for i in 0..300u64 {
            life.set(&format!("pad:{g}:{i:04}").into_bytes(), &[0x11; 900]);
            if i.is_multiple_of(64) {
                life.maintain();
            }
        }
        life.maintain();
    }
    // The hand-written checkpoint: refs at all four addresses, no images.
    let hash = Life::hash(b"quad");
    let w = life.table.begin_ckpt_walk(1).to_raw();
    let mut writer = SyncIckWriter::create_v2(
        life.fs.clone(),
        Path::new(SHARD),
        &CkptConfig::default(),
        0,
        1,
        begin(),
        &[NS.0],
    )
    .expect("create ick");
    for &addr in &copies {
        assert!(addr < w);
        writer.append_ref(NS.0, w, hash, addr).expect("ref");
    }
    writer.finish().expect("finish");
    life.table.end_ckpt_walk();
    let tier = life.table.tier_manifest(NS.0, &life.flush);
    write_manifest(
        &life.fs,
        Path::new(SHARD),
        &Manifest {
            ckpt_id: 1,
            begin_lsn: begin(),
            segments: vec![SegmentId(1)],
            tiers: vec![tier],
            key_hash_id: KeyHasher::default().identity(),
        },
    )
    .expect("manifest");
    let mut tail = Vec::new();
    RecordView::StringPostImage { ns: NS, key: b"quad", value: b"winner" }.encode_into(&mut tail);
    let filler = vec![0x22; 900];
    let mut written = 0u64;
    let mut i = 0u64;
    while written < 2 * window {
        let key = format!("tail:{i:06}").into_bytes();
        RecordView::StringPostImage { ns: NS, key: &key, value: &filler }.encode_into(&mut tail);
        written += 920;
        i += 1;
    }
    let durable = Durable { fs: life.fs.clone(), demote, model: BTreeMap::new(), tail };
    let mut boot = durable.boot();
    assert_eq!(
        copies
            .iter()
            .filter(|&&a| boot.table.contains_pair(hash, LogicalAddr::from_raw(a).expect("48-bit")))
            .count(),
        4
    );
    let hasher = KeyHasher::default();
    let mut rest: &[u8] = &durable.tail;
    let mut refused = None;
    while !rest.is_empty() {
        let (record, consumed) = inf_log::decode_record(rest).expect("decodes");
        let RecordView::StringPostImage { key, value, .. } = record else { panic!() };
        match boot.table.replay_upsert(Some(&mut boot.replay), &[], key, value, hasher.hash(key)) {
            Ok(_) => {}
            Err(err) => {
                refused = Some(err);
                break;
            }
        }
        rest = &rest[consumed..];
    }
    match refused.expect("the fourth same-key twin refuses typed") {
        ReplayRefusal::OriginRoom { cold, .. } => assert!(copies.contains(&cold.to_raw())),
        other => panic!("{other}"),
    }
    assert_eq!(boot.replay.counters().settled_same_key, 3, "three settled, exactly");
    assert_eq!(
        copies
            .iter()
            .filter(|&&a| boot.table.contains_pair(hash, LogicalAddr::from_raw(a).expect("48-bit")))
            .count(),
        1
    );
}

/// A hand-built manifested file whose refs point at: another key's
/// record, a record with a type tag of 0, a header whose length runs
/// past the file, and a record shorter than the key window at the file's
/// end. At the seal (E10) and at the end settle (E12) the first three
/// are the typed identity refusal naming the check, never "distinct";
/// the last settles.
#[test]
fn a_settle_keeps_or_removes_only_on_a_verified_record_of_its_hash() {
    use inf_log::tier::TierIdentity;
    fn image(key: &[u8], value: &[u8]) -> Vec<u8> {
        let demote = DemotionConfig::for_budget(BUDGET, PAGE);
        let mut t = TieredTable::new(space_config(demote, 0), demote, 64, KeyHasher::default())
            .expect("ring");
        let addr = t.insert(key, value, KeyHasher::default().hash(key)).expect("fits");
        let len = t.record(addr).encoded_len;
        t.record_bytes(addr, len).to_vec()
    }
    let demote = DemotionConfig::for_budget(BUDGET, PAGE);
    let window = demote.mem_budget_bytes + demote.slice_bytes;
    let (k1, k2) = forced_collision_pair(11);
    let hash = KeyHasher::default().hash(&k1);
    type Check = fn(&ColdKeyError) -> bool;
    let arms: [(&str, Vec<u8>, Check); 3] = [
        ("another key's record", image(b"other", &[0x11; 300]), |e| {
            matches!(e, ColdKeyError::HashMismatch { .. })
        }),
        (
            "a type tag of 0",
            {
                let mut bytes = image(&k1, &[0x22; 300]);
                bytes[0] &= 0x0F;
                bytes
            },
            |e| matches!(e, ColdKeyError::TypeTag { bits: 0 }),
        ),
        ("a length past the file", image(&k1, &[0x33; 300])[..200].to_vec(), |e| {
            matches!(e, ColdKeyError::LengthPastFile { .. })
        }),
    ];
    for at_seal in [true, false] {
        for (name, bytes, is_expected) in &arms {
            let name = *name;
            let fs = MemFs::new();
            fs.create_dir_all(Path::new(SHARD)).expect("dir");
            // File 0 holds the hostile bytes at address 0; the manifested
            // watermark is its end.
            let flush_cfg = flush_config(NS, FILE_CAPACITY);
            let mut w = TierWriter::create(
                &fs,
                Path::new(SHARD),
                0,
                0,
                NS,
                LogicalAddr::ZERO,
                TierIoMode::Buffered,
            )
            .expect("writer");
            w.append(LogicalAddr::ZERO, bytes).expect("append");
            let (sealed, _) = w.seal(SealReason::Capacity).expect("seal");
            let origin = sealed.data_len;
            let _ = TierIdentity { cell: 0, ns: NS, base: LogicalAddr::ZERO };
            let mut writer = SyncIckWriter::create_v2(
                fs.clone(),
                Path::new(SHARD),
                &CkptConfig::default(),
                0,
                1,
                begin(),
                &[NS.0],
            )
            .expect("create ick");
            writer.append_ref(NS.0, origin, hash, 0).expect("ref");
            writer.finish().expect("finish");
            write_manifest(
                &fs,
                Path::new(SHARD),
                &Manifest {
                    ckpt_id: 1,
                    begin_lsn: begin(),
                    segments: vec![SegmentId(1)],
                    tiers: vec![inf_log::TierNsManifest {
                        ns: NS.0,
                        flushed: origin,
                        files: vec![inf_log::TierFileRange { id: 0, base: 0, durable_len: origin }],
                    }],
                    key_hash_id: KeyHasher::default().identity(),
                },
            )
            .expect("manifest");
            let _ = flush_cfg;
            let durable = Durable { fs, demote, model: BTreeMap::new(), tail: Vec::new() };
            let mut boot = durable.boot();
            let zero = LogicalAddr::ZERO;
            assert!(boot.table.contains_pair(hash, zero));
            // At the seal: k1's record first, then two windows of filler,
            // so a demote step seals it. At the end: the filler first (the
            // boot demotes), then k1, still `Open` when replay ends.
            let filler = vec![0x22; 900];
            let fill = |boot: &mut Boot| -> Result<(), ReplayRefusal> {
                let mut written = 0u64;
                let mut i = 0u64;
                while written < 2 * window {
                    let key = format!("tail:{i:06}").into_bytes();
                    let h = KeyHasher::default().hash(&key);
                    boot.table.replay_upsert(Some(&mut boot.replay), &[], &key, &filler, h)?;
                    written += 920;
                    i += 1;
                }
                Ok(())
            };
            let outcome: Result<(), ReplayRefusal> = if at_seal {
                boot.table
                    .replay_upsert(Some(&mut boot.replay), &[], &k1, b"winner", hash)
                    .expect("fits");
                fill(&mut boot)
            } else {
                fill(&mut boot).expect("no settle without a twin");
                boot.table
                    .replay_upsert(Some(&mut boot.replay), &[], &k1, b"winner", hash)
                    .expect("fits");
                boot.replay.end_of_replay(&boot.table);
                boot.replay.settle_step(&mut boot.table, 1 << 20).map(|_| ())
            };
            match outcome.expect_err(name) {
                ReplayRefusal::SettleIdentity { addr, cause } => {
                    assert_eq!(addr, zero, "{name}");
                    assert!(is_expected(&cause), "{name} at seal {at_seal}: {cause}");
                }
                other => panic!("{name} at seal {at_seal}: {other}"),
            }
            assert!(boot.table.contains_pair(hash, zero), "{name}: the slot stays, unsettled");
            assert_eq!(boot.replay.counters().settled_distinct, 0, "{name}: never distinct");
            assert_eq!(boot.replay.counters().settled_same_key, 0, "{name}: never settled");
        }
    }
    // The legal short window: k1's record shorter than the key window at
    // the file's end settles against its true owner at the seal.
    let short = image(&k1, b"v");
    assert!(short.len() < TieredTable::KEY_PREFIX_LEN);
    let fs = MemFs::new();
    fs.create_dir_all(Path::new(SHARD)).expect("dir");
    let mut w = TierWriter::create(
        &fs,
        Path::new(SHARD),
        0,
        0,
        NS,
        LogicalAddr::ZERO,
        TierIoMode::Buffered,
    )
    .expect("writer");
    w.append(LogicalAddr::ZERO, &short).expect("append");
    let (sealed, _) = w.seal(SealReason::Capacity).expect("seal");
    let origin = sealed.data_len;
    let mut writer = SyncIckWriter::create_v2(
        fs.clone(),
        Path::new(SHARD),
        &CkptConfig::default(),
        0,
        1,
        begin(),
        &[NS.0],
    )
    .expect("create ick");
    writer.append_ref(NS.0, origin, hash, 0).expect("ref");
    writer.finish().expect("finish");
    write_manifest(
        &fs,
        Path::new(SHARD),
        &Manifest {
            ckpt_id: 1,
            begin_lsn: begin(),
            segments: vec![SegmentId(1)],
            tiers: vec![inf_log::TierNsManifest {
                ns: NS.0,
                flushed: origin,
                files: vec![inf_log::TierFileRange { id: 0, base: 0, durable_len: origin }],
            }],
            key_hash_id: KeyHasher::default().identity(),
        },
    )
    .expect("manifest");
    let durable = Durable { fs, demote, model: BTreeMap::new(), tail: Vec::new() };
    let mut boot = durable.boot();
    let filler = vec![0x22; 900];
    let mut written = 0u64;
    let mut i = 0u64;
    while written < 2 * window {
        let key = format!("tail:{i:06}").into_bytes();
        let h = KeyHasher::default().hash(&key);
        boot.table.replay_upsert(Some(&mut boot.replay), &[], &key, &filler, h).expect("fits");
        written += 920;
        i += 1;
    }
    // k2 walks first: its read finds k1's record a distinct key; then k1
    // settles it.
    boot.table.replay_upsert(Some(&mut boot.replay), &[], &k2, b"two", hash).expect("fits");
    let winner =
        boot.table.replay_upsert(Some(&mut boot.replay), &[], &k1, b"new", hash).expect("fits");
    boot.replay.end_of_replay(&boot.table);
    boot.replay.settle_step(&mut boot.table, 1 << 20).expect("a verified short record settles");
    assert!(!boot.table.contains_pair(hash, LogicalAddr::ZERO), "settled");
    assert_eq!(boot.table.displacement_origins_len(hash, winner), 1, "chained into its owner");
    assert_eq!(boot.replay.counters().settled_same_key, 1);
    assert_eq!(boot.replay.counters().settled_distinct, 1, "k2's read found k1's record distinct");
}

// ---- §6 row: a cold record in the active file's partial tail frame ----------

/// A `DEL` of a key whose record lies in the last frame the boot flushed,
/// applied after a release passed that record and before the next flush
/// (E5 reads the active file's partial tail frame through the writer's
/// handle); the next step extends the frame, and a second key of that
/// frame is deleted: both reads parse and both keys are gone.
#[test]
fn a_del_reads_the_active_files_partial_tail_frame_before_and_after_its_rewrite() {
    use inf_log::TIER_FRAME_DATA;
    let demote = DemotionConfig::for_budget(BUDGET, PAGE);
    let window = demote.mem_budget_bytes + demote.slice_bytes;
    let fs = MemFs::new();
    fs.create_dir_all(Path::new(SHARD)).expect("dir");
    let mut life = Life::new(demote);
    life.checkpoint(1, |_| {});
    let value = vec![0x22; 900];
    let mut keys: Vec<Vec<u8>> = Vec::new();
    let mut written = 0u64;
    let mut i = 0u64;
    while written < 2 * window {
        let key = format!("tail:{i:06}").into_bytes();
        life.set(&key, &value);
        written += life.lens[&key].0 as u64;
        keys.push(key);
        i += 1;
        if i.is_multiple_of(64) {
            life.maintain();
        }
    }
    life.maintain();
    let durable = life.crash();
    let mut boot = durable.boot();
    // Replay the images step by step until a demote step leaves the
    // active file's partial tail frame holding two record starts: each
    // step's cut lands one lead past its need, so the frame's fill walks
    // the frame in fixed strides and reaches two records within a few
    // dozen steps (the arithmetic is the data's, not a schedule's).
    let hasher = KeyHasher::default();
    let mut rest: &[u8] = &durable.tail;
    let mut applied: Vec<Vec<u8>> = Vec::new();
    let mut in_frame: Vec<Vec<u8>> = Vec::new();
    let mut frame_start = 0u64;
    for _ in 0..64 {
        let steps = boot.replay.counters().demote_steps;
        while boot.replay.counters().demote_steps == steps {
            let (record, consumed) = inf_log::decode_record(rest).expect("decodes");
            let RecordView::StringPostImage { key, value, .. } = record else { panic!() };
            boot.table
                .replay_upsert(Some(&mut boot.replay), &[], key, value, hasher.hash(key))
                .expect("fits");
            applied.push(key.to_vec());
            rest = &rest[consumed..];
        }
        let (_, base, data_len, durable_len, _) =
            boot.replay.active().expect("the step left a file open");
        assert_eq!(data_len, durable_len, "the barrier claimed every appended byte");
        if data_len % TIER_FRAME_DATA as u64 == 0 {
            continue;
        }
        frame_start = base.to_raw() + (data_len / TIER_FRAME_DATA as u64) * TIER_FRAME_DATA as u64;
        // A release passed the frame's records (the step released to its
        // target; the row's release goes to the barrier-claimed end).
        while boot.table.release_slice() > 0 {}
        assert_eq!(boot.table.space().head(), boot.table.space().flushed());
        in_frame = applied
            .iter()
            .filter(|k| match boot.table.lookup(k, hasher.hash(k), &[]) {
                TieredLookup::Cold(a) => a.to_raw() >= frame_start,
                _ => false,
            })
            .cloned()
            .collect();
        if in_frame.len() >= 2 {
            break;
        }
    }
    assert!(in_frame.len() >= 2, "VACUOUS: no step left two records in the partial tail frame");
    assert!(frame_start > 0);
    let reads = boot.replay.counters().settle_reads;
    boot.table
        .replay_delete(Some(&mut boot.replay), &[], &in_frame[0], hasher.hash(&in_frame[0]))
        .expect("E5 reads the partial frame");
    assert_eq!(boot.replay.counters().settle_reads, reads + 1, "one read covered the tail frame");
    assert_eq!(boot.replay.counters().deletes_verified, 1);
    // Extend the frame with the next records (the next step rewrites it
    // in place), then delete the second key of that frame.
    let mut model = durable.model.clone();
    model.remove(&in_frame[0]);
    let mut more = 0;
    let steps = boot.replay.counters().demote_steps;
    while boot.replay.counters().demote_steps == steps {
        let (record, consumed) = inf_log::decode_record(rest).expect("decodes");
        let RecordView::StringPostImage { key, value, .. } = record else { panic!() };
        boot.table
            .replay_upsert(Some(&mut boot.replay), &[], key, value, hasher.hash(key))
            .expect("fits");
        rest = &rest[consumed..];
        more += 1;
    }
    assert!(more > 0);
    boot.table
        .replay_delete(Some(&mut boot.replay), &[], &in_frame[1], hasher.hash(&in_frame[1]))
        .expect("E5 after the rewrite");
    assert_eq!(boot.replay.counters().deletes_verified, 2);
    model.remove(&in_frame[1]);
    // The rest of the tail, then the audit against the adjusted model.
    while !rest.is_empty() {
        let (record, consumed) = inf_log::decode_record(rest).expect("decodes");
        let RecordView::StringPostImage { key, value, .. } = record else { panic!() };
        boot.table
            .replay_upsert(Some(&mut boot.replay), &[], key, value, hasher.hash(key))
            .expect("fits");
        rest = &rest[consumed..];
    }
    let ready = boot.finish();
    ready.audit(&model, true);
}

// ---- §6 row: two crashes (I10) ------------------------------------------

/// Boot 1 settles a shadow pair's ref into its sealed winner (no marker
/// anywhere names the ref: the pair was a ticket); after `Ready` the key
/// is deleted live, and the origin marker boot 1 chained rides the
/// delete; crash; boot 2 with the budget raised fits: that marker names
/// the ref below the origin and finds the pair present, and the key
/// stays deleted. Red under `inf_canary_replay_origin_drop` (E10 chains
/// nothing): no marker names the ref, it survives boot 2 and the key
/// resurrects.
#[test]
fn a_ref_settled_by_boot_one_stays_deleted_across_a_second_crash() {
    let small = DemotionConfig::for_budget(BUDGET, PAGE);
    let mut life = Life::new(small);
    life.table.set_shadow_enabled(true);
    life.set(b"twice", &[0x77; 400]);
    for i in 0..600u64 {
        life.set(&format!("cold:{i:05}").into_bytes(), &[0x11; 900]);
        if i.is_multiple_of(64) {
            life.maintain();
        }
    }
    life.maintain();
    assert!(matches!(
        life.table.lookup(b"twice", Life::hash(b"twice"), &[]),
        TieredLookup::Cold(_)
    ));
    for i in 0..600u64 {
        life.set(&format!("img:{i:05}").into_bytes(), &[0x11; 900]);
    }
    life.shadow_set(b"twice", &[0x88; 400]);
    life.checkpoint_ordered(1, |_| {}, &[b"twice"], &[]);
    for i in 0..200u64 {
        life.set(&format!("tail:{i:06}").into_bytes(), &[0x22; 900]);
    }
    let durable = life.crash();
    // Boot 1 under a lowered budget: image load seals the winner, so the
    // ref settles at the seal (E10) and rides the survivor's origins.
    // Then the live DEL.
    let mut boot = durable.boot_with(lowered(), |_| {});
    assert!(boot.replay.counters().settled_same_key >= 1, "boot 1 settled the ref at a seal");
    boot.replay(&durable.tail);
    let ready = boot.finish();
    ready.audit(&durable.model, true);
    let Ready { fs, table, flush, .. } = ready;
    let mut life2 = Life {
        fs,
        demote: lowered(),
        table,
        flush,
        model: durable.model.clone(),
        tail: durable.tail.clone(),
        begun: true,
        lens: BTreeMap::new(),
    };
    let hash = Life::hash(b"twice");
    let a = match life2.table.lookup(b"twice", hash, &[]) {
        TieredLookup::Ram(a) | TieredLookup::Cold(a) => a,
        TieredLookup::Miss => panic!("live"),
    };
    let chained = life2.table.displacement_origins_len(hash, a);
    let markers_before = life2.tail.len();
    life2.del(b"twice");
    assert!(life2.tail.len() > markers_before);
    let durable2 = life2.crash();
    // Boot 2 under a raised budget: the same unit, a longer tail, fits.
    let raised = DemotionConfig::for_budget(4 * BUDGET, PAGE);
    let mut boot2 = durable2.boot_with(raised, |_| {});
    let origin = boot2.table.space().life_origin();
    let ref_addr = {
        let mut found = None;
        let mut cursor = 0u64;
        loop {
            cursor = boot2.table.scan_slots(cursor, 256, |h, a| {
                if h == hash && a < origin {
                    found = Some(a);
                }
            });
            if cursor == 0 {
                break found.expect("the ref is restored again");
            }
        }
    };
    boot2.replay(&durable2.tail);
    let counters2 = boot2.replay.counters();
    let ref_present = boot2.table.contains_pair(hash, ref_addr);
    let ready2 = boot2.finish();
    // The oracle first: the key resurrects from the ref when boot 1's
    // settle chained nothing.
    ready2.audit(&durable2.model, false);
    assert!(
        matches!(ready2.table.lookup(b"twice", hash, &[]), TieredLookup::Miss),
        "stays deleted"
    );
    // Then the engagement: boot 1 chained the ref, boot 2 fit, its marker
    // removed the ref below the origin and skipped the this-life one.
    assert_eq!(chained, 1, "boot 1 chained the ref (I10)");
    assert_eq!(counters2.demote_steps, 0, "boot 2 fits");
    assert!(!ref_present, "the marker removed the ref (E6)");
    assert!(counters2.markers_skipped >= 1, "the this-life marker was skipped (E7)");
}

// ---- §6 row: forced 64-bit collisions ----------------------------------------

/// Two keys with one 64-bit hash, one demoted by the boot: both survive;
/// the pair is the one ticket a demoted namespace holds at `Ready`.
#[test]
fn a_colliding_pair_with_one_demoted_survives_as_the_one_ticket() {
    let demote = DemotionConfig::for_budget(BUDGET, PAGE);
    let window = demote.mem_budget_bytes + demote.slice_bytes;
    let (k1, k2) = forced_collision_pair(21);
    let mut life = Life::new(demote);
    life.checkpoint(1, |_| {});
    life.set(&k1, b"one");
    let mut written = 0u64;
    let mut i = 0u64;
    while written < 2 * window {
        let key = format!("tail:{i:06}").into_bytes();
        life.set(&key, &[0x22; 900]);
        written += life.lens[&key].0 as u64;
        i += 1;
        if i.is_multiple_of(64) {
            life.maintain();
        }
    }
    life.set(&k2, b"two");
    life.maintain();
    let durable = life.crash();
    let mut boot = durable.boot();
    boot.replay(&durable.tail);
    assert!(boot.replay.counters().demote_steps > 0);
    let ready = boot.finish();
    ready.audit(&durable.model, true);
    assert_eq!(ready.table.shadow_pending(), 1, "the colliding pair is the one ticket");
    assert!(ready.counters.settled_distinct >= 1, "k2's seal or end settle read k1 as distinct");
}

// ---- §6 row: an extent-typed twin and the blob census (E14) -----------------

/// A blob key's cold record under an open shadow ticket at the walk:
/// the twin is a ref (pass 0), the winner an image (pass 1, ADR-0093
/// A12) and the twin's blob reference rides the 0x05 section (pass 3).
/// The boot seals the winner during image load, so the settle precedes
/// the 0x05 section and the end of the checkpoint releases the twin's
/// blob reference (R9). The blob census holds. Red under
/// `inf_canary_replay_blob_release_skip`: a reference with no slot.
#[test]
fn a_blob_ref_settled_during_image_load_is_released_at_the_end_of_the_checkpoint() {
    let demote = DemotionConfig::for_budget(BUDGET, PAGE);
    let blob = BlobConfig { threshold_bytes: 2048, max_bytes: 1 << 20 };
    let mut life = Life::new(demote);
    life.table.set_blob_config(blob);
    life.table.set_shadow_enabled(true);
    life.set_blob(b"blobby", &[0x99; 4096]);
    for i in 0..600u64 {
        life.set(&format!("cold:{i:05}").into_bytes(), &[0x11; 900]);
        if i.is_multiple_of(64) {
            life.maintain();
        }
    }
    life.maintain();
    assert!(matches!(
        life.table.lookup(b"blobby", Life::hash(b"blobby"), &[]),
        TieredLookup::Cold(_)
    ));
    // A window of RAM images, then the shadow write over the cold blob
    // record: the ticket is open through the walk.
    for i in 0..600u64 {
        life.set(&format!("img:{i:05}").into_bytes(), &[0x11; 900]);
    }
    life.shadow_set(b"blobby", b"inline-now");
    assert_eq!(life.table.shadow_pending(), 1);
    life.checkpoint_ordered(1, |_| {}, &[b"blobby"], &[]);
    let durable = life.crash();
    let mut boot = durable.boot_with(lowered(), |table| table.set_blob_config(blob));
    let counters = boot.replay.counters();
    boot.replay(&durable.tail);
    let ready = boot.finish();
    // The oracle first: the blob census finds a reference with no slot
    // when the end of the checkpoint released nothing.
    ready.audit(&durable.model, true);
    // Then the engagement.
    assert!(counters.demote_steps > 0, "image load demoted");
    assert!(counters.settled_same_key >= 1, "the ref settled at a seal during image load");
    assert_eq!(counters.blob_releases, 1, "the end of the checkpoint released the ref's entry");
}

// ---- §6 row: a window below its ring, records at the inline maximum -------

/// Case (a): `MEM-BUDGET 4mb BLOB-THRESHOLD 3mb` with records of 2.9 MiB;
/// case (b): `4mb + 64kb` with the largest threshold the ring allows and
/// records of 3.9 MiB. Both boot; pads are placed in each arm.
#[test]
fn a_window_below_its_ring_pads_the_tail_for_records_at_the_inline_maximum() {
    let mib = 1u64 << 20;
    let cases: [(DemotionConfig, u32, usize); 2] = [
        (DemotionConfig::for_budget(4 * mib, mib), 3 << 20, (29 * mib / 10) as usize),
        (
            DemotionConfig {
                mem_budget_bytes: 4 * mib,
                mutable_permille: 250,
                slice_bytes: 64 << 10,
            },
            4 << 20,
            (39 * mib / 10) as usize,
        ),
    ];
    for (demote, threshold, record) in cases {
        let blob = BlobConfig { threshold_bytes: threshold, max_bytes: 1 << 30 };
        let mut life = Life::new(demote);
        life.table.set_blob_config(blob);
        assert!(record < life.table.blob_config().threshold_bytes as usize, "inline");
        life.checkpoint(1, |_| {});
        // The tail placed by hand (the live path parks on a stall target
        // above the tail for these records — FCR-STTIER-N4): eight
        // records at the inline maximum, acknowledged by the crashed life.
        let value = vec![0x5A; record];
        let mut model = BTreeMap::new();
        let mut tail = Vec::new();
        for i in 0..8u64 {
            let key = format!("big:{i:02}").into_bytes();
            RecordView::StringPostImage { ns: NS, key: &key, value: &value }.encode_into(&mut tail);
            model.insert(key, Expect { value: value.clone(), extent: None });
        }
        let durable = Durable { fs: life.fs.clone(), demote, model, tail };
        let mut boot = durable.boot_with(demote, |table| table.set_blob_config(blob));
        boot.replay(&durable.tail);
        let counters = boot.replay.counters();
        assert!(counters.demote_steps > 0, "the regime engaged");
        assert!(counters.pads_placed > 0, "VACUOUS: no pad placed in this arm");
        let ready = boot.finish();
        ready.audit(&durable.model, true);
        assert!(ready.boot_files().len() <= 8 + 2 + 1, "files sealed within §3's sum");
    }
}

// ---- §2 row: the settle read fails (`replay_settle_read_fail`) --------------

/// The injected settle-read failure: a `DEL` whose read fails is the
/// typed refusal and has changed nothing (I3); the seal's read failing
/// leaves the boundary where it was; a later boot with the fault cleared
/// recovers the same unit.
#[test]
fn a_failed_settle_read_refuses_typed_and_changes_nothing() {
    let demote = DemotionConfig::for_budget(BUDGET, PAGE);
    let window = demote.mem_budget_bytes + demote.slice_bytes;
    let mut life = Life::new(demote);
    life.checkpoint(1, |_| {});
    life.set(b"victim", &[0x44; 300]);
    let mut written = 0u64;
    let mut i = 0u64;
    while written < 2 * window {
        let key = format!("filler:{i:06}").into_bytes();
        life.set(&key, &[0x22; 900]);
        written += life.lens[&key].0 as u64;
        i += 1;
        if i.is_multiple_of(64) {
            life.maintain();
        }
    }
    life.del(b"victim");
    life.maintain();
    let durable = life.crash();
    // The DEL's read fails: nothing changed, the key still resolves cold.
    let hasher = KeyHasher::default();
    let hash = hasher.hash(b"victim");
    let mut boot = durable.boot();
    let mut rest: &[u8] = &durable.tail;
    let mut markers: Vec<LogicalAddr> = Vec::new();
    loop {
        let (record, consumed) = inf_log::decode_record(rest).expect("decodes");
        rest = &rest[consumed..];
        match record {
            RecordView::StringPostImage { key, value, .. } => {
                boot.table
                    .replay_upsert(Some(&mut boot.replay), &markers, key, value, hasher.hash(key))
                    .expect("fits");
                markers.clear();
            }
            RecordView::ColdDisplace { old_addr, .. } => {
                markers.push(LogicalAddr::from_raw(old_addr).expect("48-bit"));
            }
            RecordView::Delete { key, .. } => {
                assert_eq!(key, b"victim");
                let before = boot.table.space().ro_boundary();
                let slots_before = boot.table.len();
                fault::arm(inf_log::fault::REPLAY_SETTLE_READ_FAIL, FaultSpec::Nth(1));
                let err = boot
                    .table
                    .replay_delete(Some(&mut boot.replay), &markers, key, hash)
                    .expect_err("the injected read failure refuses");
                fault::disarm_all();
                assert!(matches!(err, ReplayRefusal::SettleRead { .. }), "{err}");
                assert!(matches!(boot.table.lookup(b"victim", hash, &[]), TieredLookup::Cold(_)));
                assert_eq!(boot.table.len(), slots_before, "no slot moved (I3)");
                assert_eq!(boot.table.space().ro_boundary(), before);
                // The same boot, the fault cleared: the DEL applies.
                boot.table
                    .replay_delete(Some(&mut boot.replay), &markers, key, hash)
                    .expect("verified");
                markers.clear();
                break;
            }
            other => panic!("{other:?}"),
        }
    }
    assert!(rest.is_empty());
    let ready = boot.finish();
    ready.audit(&durable.model, true);
    // The seal's read failing: the boundary stays, the next boot recovers.
    let mut life = Life::new(demote);
    life.set(b"paired", &[0x77; 400]);
    for i in 0..600u64 {
        life.set(&format!("cold:{i:05}").into_bytes(), &[0x11; 900]);
        if i.is_multiple_of(64) {
            life.maintain();
        }
    }
    life.maintain();
    life.checkpoint(1, |life| life.set(b"paired", &[0x88; 400]));
    let mut written = 0u64;
    let mut i = 0u64;
    while written < 2 * window {
        let key = format!("tail:{i:06}").into_bytes();
        life.set(&key, &[0x22; 900]);
        written += life.lens[&key].0 as u64;
        i += 1;
        if i.is_multiple_of(64) {
            life.maintain();
        }
    }
    life.maintain();
    let durable = life.crash();
    // The read fails at image load's first seal: the boot refuses typed.
    fault::arm(inf_log::fault::REPLAY_SETTLE_READ_FAIL, FaultSpec::Nth(1));
    let manifest = read_manifest(&durable.fs, Path::new(SHARD)).expect("read").expect("present");
    let tier = manifest.tier_ns(NS.0).expect("tier section").clone();
    let recovered = recover_tiered_ns(
        durable.fs.clone(),
        &tier,
        manifest.ckpt_id,
        flush_config(NS, FILE_CAPACITY),
        space_config(lowered(), 0),
        lowered(),
        4096,
        KeyHasher::default(),
    )
    .expect("tier recovery");
    let table = std::cell::RefCell::new(recovered.table);
    let replay = std::cell::RefCell::new(recovered.replay);
    let ick = Path::new(SHARD).join(inf_log::ckpt::ick_file_name(manifest.ckpt_id));
    let mut refused = false;
    let loaded = read_ick_hybrid(
        &durable.fs,
        &ick,
        inf_log::ckpt::IckReaderConfig::default(),
        |record| {
            let RecordView::StringPostImage { key, value, .. } = record else { panic!() };
            let ro = table.borrow().space().ro_boundary();
            let placed = table.borrow_mut().replay_upsert(
                Some(&mut *replay.borrow_mut()),
                &[],
                key,
                value,
                hasher.hash(key),
            );
            match placed {
                Ok(_) => Ok(()),
                Err(ReplayRefusal::SettleRead { .. }) => {
                    assert_eq!(table.borrow().space().ro_boundary(), ro, "the boundary stayed");
                    refused = true;
                    Err(())
                }
                Err(other) => panic!("{other}"),
            }
        },
        |section| {
            apply_ref_section(&mut table.borrow_mut(), &section, tier.flushed).expect("refs");
            Ok(())
        },
        |_| Ok(()),
        |_| Ok(()),
        |_| panic!("no index-sidecar sections in this image"),
    );
    fault::disarm_all();
    assert!(loaded.is_err() && refused, "the seal's read refused typed during image load");
    drop((table, replay));
    let mut again = durable.boot_with(lowered(), |_| {});
    again.replay(&durable.tail);
    let ready = again.finish();
    ready.audit(&durable.model, true);
}
