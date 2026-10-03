//! `m4-recovery` (M4-S12, ADR-0057 D8): the unified recovery picture
//! under seeded power cuts — the never-none invariant checked directly.
//!
//! Each seeded run is a chain of lives on one [`SimDisk`]. Per life:
//! mutate (the modeled WAL tail carries real record-v1 encodings,
//! `ColdDisplace` pairing included) → run the fuzzy hybrid walk (refs
//! below the walk watermark, images above, mutations and demotion
//! interleaved between slices; the release pin holds — D2) → publish
//! `.ick` v2 + MANIFEST v2 as one recovery unit → more tail ops → a
//! seeded **power cut** tears every un-fsynced byte → recover
//! (`recover_tiered_ns` + hybrid checkpoint load + D4 tail replay) →
//! the oracle: every model-live key serves its exact bytes (cold ranges
//! CRC-verified from the recovered catalog), every model-dead key
//! misses. Content — canonical bytes — never addresses, and never
//! string versions (per-life artifacts, ADR-0057 D3).
//!
//! **Shadow-slot ops (M4.5-S37, ADR-0093):** a seeded share of the
//! SETs over a demoted key take the shadow path — the record appends,
//! the cold twin stays slotted as a ticket, no marker is staged — and
//! the harness reconciles tickets at seeded points (some are deliberately
//! left open across the walk and the cut). Recovery re-forms the pairs
//! from the checkpoint's ref + image and the tail's image (the D5
//! rebuild), the harness reconciles them again, and the never-none
//! oracle plus a **cardinality oracle** (`len()` equals the model's key
//! count after reconciliation — no orphan slot) close the row.
//!
//! Seed classes (deterministic per seed, disclosed in the report):
//! - **cut-before-publish** lives: the walk runs but the swap never
//!   lands — recovery resolves the *previous* unit and the WAL tail
//!   keeps accumulating (the truncation rule's negative half: nothing
//!   truncates without a durable name).
//! - **flush-lag** lives: demotion is suppressed during the walk, so
//!   RAM-resident records span checkpoints and must re-image every time
//!   (the D7 falsifier: coverage never leans on a previous checkpoint).
//!
//! Every event folds into `trace_hash`; `--verify-determinism` runs the
//! scenario twice and requires hash identity (L7).

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use inf_foundation::hash64;
use inf_foundation::rng::{Entropy, SplitMix64};
use inf_foundation::time::Nanos;
use inf_log::blob::{ExtentId, ExtentWriter, list_extent_ids, open_extent, unlink_extent_file};
use inf_log::ckpt::{IckReaderConfig, ick_file_name};
use inf_log::flush::HandedOver;
use inf_log::flush::{TierFileMeta, unlink_tier_file};
use inf_log::fs::SegmentFs;
use inf_log::fs::sim::SimDisk;
use inf_log::manifest::TierNsManifest;
use inf_log::{
    CkptConfig, Lsn, Manifest, MutationEffect, NsId, RecordView, SegmentId, StagingConfig,
    StagingRing, SyncIckWriter, TIER_FRAME_BYTES, TierFlush, TierFlushConfig, TierIoMode,
    decode_record, read_ick_hybrid, read_manifest, tier_extract, tier_frame_offset,
    tier_frame_span, write_manifest,
};
use inf_store::{
    AddressSpaceConfig, BlobConfig, CompactionWork, DemotionConfig, EXTENT_REF_LEN, ExtentRef,
    FsyncClass, KeyHasher, KeyWindow, Keyspace, LogicalAddr, NsMode, NsSpec, ReplayCounters,
    ReplaySpill, Room, SettleProgress, StoreConfig, TierReplay, TierSpec, TieredLookup,
    TieredTable, WallAnchor, apply_blob_ref_section, apply_live_set_section, apply_ref_section,
    forced_collision_pair, recover_tiered_ns,
};

mod shadow;

const NS: NsId = NsId(88);
const PAGE: u64 = 4 << 10;
const BUDGET: u64 = 1 << 20;
/// Small tier files so lives span rotations and gaps.
const FILE_CAPACITY: u64 = 48 << 10;
/// Out-of-line threshold for the blob leg (M4-S17, ADR-0061) — above
/// every inline value the op generator emits, below every blob value.
const BLOB_THRESHOLD: u32 = 256;

/// The spec-variant seed class (ADR-0174 D2 rule 6; the record's §6
/// second row, at the harness's 4 KiB commit page): each boot recovers at
/// a window below its 8-page ring, with records up to the inline maximum,
/// so a record's need can lie above the boot's tail and replay pads.
/// Every live life runs at the whole ring and lowers `MEM-BUDGET` to the
/// variant's before its cut (a lowered budget keeps the ring, so live and
/// boot share one ring; ADR-0062 D3), and raises it back after the boot.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum SpecVariant {
    /// Case (a), `MEM-BUDGET 4mb BLOB-THRESHOLD 3mb` scaled: a 4-page
    /// budget and a 1-page slice (a 5-page window), a 3-page threshold,
    /// records from 2.125 pages to the threshold's longest, just above 3
    /// — where a ring-top hole's pages and the record's exceed the window,
    /// the tail pads to the ring top.
    RingTop,
    /// Case (b), `MEM-BUDGET 4mb MAINTAIN-SLICE 64kb` scaled: a 4-page
    /// budget and a quarter-page slice (a window of half the ring), the
    /// largest threshold the ring allows, records of 3 to 4 pages and of
    /// exactly half the ring — from an unaligned tail such a record spans
    /// more pages than the window, so the tail pads to the next page.
    Page,
}

impl SpecVariant {
    /// The seed's variant, one of the two.
    #[must_use]
    pub fn of_seed(seed: u64) -> SpecVariant {
        if (seed >> 2).is_multiple_of(2) { SpecVariant::RingTop } else { SpecVariant::Page }
    }
}

/// The tier spec a run's lives and boots use: the boot's (what the spec
/// holds at a cut), the live lives', and the blob threshold of both.
#[derive(Copy, Clone, Debug)]
struct Spec {
    boot: DemotionConfig,
    live: DemotionConfig,
    blob_threshold: u32,
    variant: Option<SpecVariant>,
}

impl Spec {
    fn of(variant: Option<SpecVariant>) -> Spec {
        let Some(variant) = variant else {
            let boot = demote();
            return Spec { boot, live: boot, blob_threshold: BLOB_THRESHOLD, variant: None };
        };
        let ring = 8 * PAGE;
        let (slice_bytes, blob_threshold) = match variant {
            SpecVariant::RingTop => (PAGE, u32::try_from(3 * PAGE).expect("three pages")),
            SpecVariant::Page => (PAGE / 4, TierSpec::blob_threshold_max(ring)),
        };
        let boot = DemotionConfig { mem_budget_bytes: 4 * PAGE, mutable_permille: 40, slice_bytes };
        let spec = Spec { boot, live: raise(boot), blob_threshold, variant: Some(variant) };
        debug_assert_eq!(spec.ring(), ring, "both variants reserve an 8-page ring");
        spec
    }

    /// The ring both the live table and the boot reserve.
    fn ring(&self) -> u64 {
        u64::try_from(self.boot.ring_reserve_bytes().expect("valid budget")).expect("u64")
    }

    /// The boot's window: `MEM-BUDGET + MAINTAIN-SLICE` in whole pages.
    fn window(&self) -> u64 {
        (self.boot.mem_budget_bytes + self.boot.slice_bytes) / PAGE * PAGE
    }

    /// The slack a replay unit leaves the boot's window to fit by
    /// construction: the page rounding at both ends and a ring-top hole
    /// under the longest record the mix places — an inline value under the
    /// blob threshold and a key of at most 64 bytes, or, in a variant, a
    /// record of half the ring.
    fn fit_margin(&self) -> u64 {
        match self.variant {
            None => 2 * PAGE + 1024,
            Some(_) => 2 * PAGE + self.ring() / 2,
        }
    }

    /// A blob configuration at the spec's threshold.
    fn blob(&self) -> BlobConfig {
        BlobConfig { threshold_bytes: self.blob_threshold, max_bytes: 1 << 20 }
    }
}

/// Scenario knobs — the DSL v0 shape (a struct, not a language).
#[derive(Debug)]
pub struct RecoveryScenario {
    pub seed: u64,
    /// Distinct keys in play.
    pub keys: u64,
    /// Lives (cut + recover cycles) per run.
    pub lives: u64,
    /// Mutations per life phase.
    pub ops_per_phase: u64,
    /// The replay-above-window seed class (ADR-0174 D1), one seed in
    /// four: before each cut, tiered writes fill the tail until the bytes
    /// the next boot re-appends reach a multiple of the window, so the
    /// boot must demote during replay (`Run::fill_replay_unit`). Its last
    /// life carries the two-crash row (ADR-0174 I10, `Run::two_crash_coda`):
    /// that life publishes, shadow-writes keys its checkpoint names by a
    /// ref, and fills just past the window; after the boot settles those
    /// refs, the window rises to its ring, the keys are deleted live, the
    /// power is cut again, and the second boot must remove each ref by the
    /// `DEL`'s marker. `--replay-above-window` forces the class on any
    /// seed; a run in it that never exceeded a window, whose boots never
    /// demoted, or whose two-crash row settled no ref or whose second boot
    /// did not fit, reports `VACUOUS`.
    pub replay_above_window: bool,
    /// The spec-variant seed class (ADR-0174 D2 rule 6), one seed in four,
    /// the variant by seed ([`SpecVariant::of_seed`]): every boot recovers
    /// at a window below its ring, and before each cut the fill
    /// (`Run::fill_replay_unit`) writes records up to the inline maximum
    /// until the unit reaches a multiple of that window drawn as the class
    /// above draws it. Live writes run at the whole ring; a long record
    /// the live window cannot place yet is written short instead. No
    /// two-crash row. `--spec-variant ring-top|page` forces it on any seed;
    /// a run whose boots placed no pad or made no demote step reports
    /// `VACUOUS`.
    pub spec_variant: Option<SpecVariant>,
}

impl RecoveryScenario {
    #[must_use]
    pub fn m4_recovery(seed: u64) -> RecoveryScenario {
        // 480 ops/phase (was 320 pre-S17): the blob leg moved a sixth of
        // the op mix out of line, thinning record volume — the bump
        // keeps demotion, flush rotation, and copy-forward relocation
        // coverage on the smoke seed (coverage disclosed, never
        // assumed).
        RecoveryScenario {
            seed,
            keys: 800,
            lives: 4,
            ops_per_phase: 480,
            replay_above_window: seed % 4 == 1,
            spec_variant: (seed % 4 == 3).then(|| SpecVariant::of_seed(seed)),
        }
    }

    /// The spec-variant class forced on this seed, in place of any other
    /// class.
    #[must_use]
    pub fn with_spec_variant(mut self, variant: SpecVariant) -> RecoveryScenario {
        self.replay_above_window = false;
        self.spec_variant = Some(variant);
        self
    }

    /// The replay-above-window class forced on this seed, at the
    /// scenario's own spec.
    #[must_use]
    pub fn with_replay_above_window(mut self) -> RecoveryScenario {
        self.replay_above_window = true;
        self.spec_variant = None;
        self
    }
}

#[derive(Debug, Default)]
pub struct RecoveryReport {
    pub violations: Vec<String>,
    pub lives: u64,
    pub refs_emitted: u64,
    pub images_emitted: u64,
    pub tail_records: u64,
    pub cut_before_publish: u64,
    pub flush_lag_lives: u64,
    pub keys_audited: u64,
    /// `.ick` 0x04 live-set entries emitted across all publishes
    /// (M4-S14 — coverage disclosed, never assumed).
    pub live_entries_emitted: u64,
    /// Copy-forward records relocated across all lives (M4-S15 —
    /// coverage disclosed: a sweep that never compacted proves nothing).
    pub relocations: u64,
    /// Files copy-forward fully scanned (byte counters finalized).
    pub files_scanned: u64,
    /// Files retired through a landed covering swap (ADR-0059 D3).
    pub files_retired: u64,
    /// Retired files unlinked in-life.
    pub files_unlinked: u64,
    /// Retired files deliberately left for the boot GC (the
    /// swap ↔ unlink crash window, driven).
    pub unlinks_left_to_boot_gc: u64,
    /// Compaction slices that stalled on the tail window (refusal-aware
    /// admission observed working, ADR-0059 D6).
    pub compaction_stalls: u64,
    /// Blob extents written and referenced (M4-S17 — coverage disclosed:
    /// a refcount oracle over zero blobs proves nothing).
    pub blobs_written: u64,
    /// Orphan extents deliberately planted (durable bytes, no reference
    /// — the AC1 cut, seeded).
    pub blob_orphans_planted: u64,
    /// Extents reclaimed in-life (refcount zero, death durable) plus
    /// orphans swept at boot.
    pub blob_extents_reclaimed: u64,
    /// M4.5-S37 (ADR-0093): shadow tickets opened by the op mix, left
    /// open across a cut, re-formed by recovery, and the verdicts.
    pub shadow_opened: u64,
    pub shadow_open_at_cut: u64,
    pub shadow_reformed: u64,
    pub shadow_same_key: u64,
    pub shadow_collision: u64,
    /// ADR-0093 A7: ops on the crafted colliding pairs (two real keys
    /// with one 64-bit hash), rebuilt slots the boot read and settled
    /// by their full key (A4), and the `DBSIZE`-shaped drain checks
    /// (A3: verify every unverified ticket, then `len()` must equal the
    /// model with the verified tickets still open).
    pub shadow_collide_ops: u64,
    pub shadow_settled_at_boot: u64,
    pub shadow_drain_checks: u64,
    /// Review of 2026-08-30 (F-L07-01; batch 23, ADR-0093 A10/A11):
    /// winners the boot rebuild left carrying several tickets and the
    /// directed `DEL`s that met them; the directed rows that opened a
    /// ticket on a twin carrying relocation origins and deleted the
    /// winner, and the origins those deletes covered with markers.
    pub shadow_multi_ticket_winners: u64,
    pub shadow_multi_ticket_dels: u64,
    pub shadow_twin_origin_rows: u64,
    pub shadow_twin_origins_covered: u64,
    /// ADR-0093 A12 (batch 23): tickets held open through the walk and
    /// the cut with their winner flushed below the walk watermark, and
    /// how many the boot re-formed (pre-fix the walk referenced the
    /// winner and the key came back as two cold slots with no ticket).
    pub shadow_held_rows: u64,
    pub shadow_held_reformed: u64,
    /// Held twins an older manifest never referenced (cut-before-publish
    /// lives): legitimately absent after the boot, the key serves its
    /// image — disclosed, never a pass on its own.
    pub shadow_held_not_restored: u64,
    /// ADR-0174 D1: lives whose replay unit — the published checkpoint's
    /// images and the tail's records — exceeded the RAM window, and the
    /// largest unit in window multiples (engagement, disclosed).
    pub replay_above_window_lives: u64,
    pub replay_unit_windows_max: u64,
    /// Boots that demoted, boots whose unit fit by construction (each
    /// checked to leave the zero set at zero), boot replay's counters
    /// summed over every boot, and the live writes that parked on MAINTAIN
    /// because the window was full (ADR-0174 D5, D6).
    pub demoting_boots: u64,
    pub fitting_boots_checked: u64,
    pub boot_replay: ReplayCounters,
    pub writer_parks: u64,
    /// Parked writes a checkpoint walk's release pin kept waiting past the
    /// harness's walk slice — dropped unacknowledged.
    pub writes_parked_past_a_walk: u64,
    /// Held tickets whose injected read error a parked write cleared.
    pub held_released_by_park: u64,
    /// Boot-sealed files the dead-byte census read (engagement).
    pub boot_files_censused: u64,
    /// The two-crash row (ADR-0174 I10, the class's last life): keys a
    /// shadow write left beside the ref the checkpoint names them by, the
    /// rows whose ref the demoting boot settled (R8: removed, chained into
    /// the record's origins), and the refs the second boot found slotted
    /// after the checkpoint and saw a marker of the live `DEL` remove.
    pub two_crash_rows_opened: u64,
    pub two_crash_rows_settled: u64,
    pub two_crash_markers_removed: u64,
    /// The row did not run: the last life's unit (the walk's images and
    /// the tail since) already lay past the raised window, so the second
    /// boot could not fit it — disclosed, never a pass on its own.
    pub two_crash_unit_past_window: u64,
    /// The spec-variant class (ADR-0174 D2 rule 6): the run's variant, the
    /// long records the fill wrote (from the case's shortest: 2.125 pages,
    /// or the window less a page), and those the live window could not
    /// place yet, written short instead (engagement, disclosed).
    pub spec_variant: Option<SpecVariant>,
    pub long_records_written: u64,
    pub long_records_shortened: u64,
    pub trace_hash: u64,
    pub state_hash: u64,
    state: crate::state::StateHash,
}

impl RecoveryReport {
    #[must_use]
    pub fn ok(&self) -> bool {
        self.violations.is_empty()
    }
}

#[derive(Clone)]
struct Expect {
    value: Vec<u8>,
    /// The referenced extent when the value is out of line (M4-S17).
    extent: Option<u64>,
}

struct Life {
    table: TieredTable,
    flush: TierFlush<SimDisk>,
    /// Staging admission for the epoch stamp (M4-S17, ADR-0061 D5) —
    /// the harness models WAL durability, but the reclaim gate's epoch
    /// runs through the production `stage_wal` path.
    ring: StagingRing,
    /// Suppress demotion during the walk (the flush-lag class).
    flush_lag: bool,
}

/// The replay clock and wall anchor at boot (tiered records carry no
/// expiry, so neither decides anything here).
const BOOT_NOW: Nanos = Nanos(1);
const BOOT_ANCHOR: WallAnchor = WallAnchor { internal_ms: 0, unix_ms: 0 };

/// The seam the recovery driver lends (ADR-0174 D1): the namespace's
/// boot machine, reached by `Keyspace::apply_record` only.
struct Lent {
    machine: TierReplay<SimDisk>,
}

impl ReplaySpill for Lent {
    type Fs = SimDisk;

    fn replay_mut(&mut self, ns: NsId) -> Option<&mut TierReplay<SimDisk>> {
        (ns == NS).then_some(&mut self.machine)
    }
}

/// One booting cell as the server's recovery driver holds it: the
/// recovered table inside a keyspace and the namespace's machine lent
/// through the seam, so every checkpoint image and tail record enters
/// through `Keyspace::apply_record` — the shipped dispatcher, never a
/// copy of it.
struct Booting {
    ks: Keyspace,
    lent: Lent,
}

impl Booting {
    fn new(
        mut table: TieredTable,
        machine: TierReplay<SimDisk>,
        hasher: KeyHasher,
        spec: &Spec,
    ) -> Booting {
        // The spec's threshold holds during replay, as the catalog's does
        // at a server's boot: a value at or above it is a typed refusal.
        table.set_blob_config(spec.blob());
        let mut ks = Keyspace::new(StoreConfig { hasher, ..StoreConfig::default() });
        ks.ns_create(NsSpec {
            id: NS,
            name: b"tiered-88".to_vec(),
            mode: NsMode::Durable,
            fsync: Some(FsyncClass::Everysec),
            policy: None,
            maxmemory: None,
            tier: Some(TierSpec::for_budget(4 << 20)),
        })
        .expect("create the tiered namespace");
        // The harness's table keeps its own knobs (budget, blob, shadow).
        *ks.tiered_store_mut(NS).expect("materialized") = table;
        Booting { ks, lent: Lent { machine } }
    }

    fn table(&self) -> &TieredTable {
        self.ks.tiered_store(NS).expect("materialized")
    }

    fn table_mut(&mut self) -> &mut TieredTable {
        self.ks.tiered_store_mut(NS).expect("materialized")
    }

    /// The checkpoint (ADR-0057 D6 step 3): images through the dispatcher,
    /// the ref, live-set and blob-reference sections onto the table, then
    /// the end of the checkpoint (ADR-0174 R9).
    fn load_checkpoint(&mut self, disk: &SimDisk, ick: &Path, flushed: u64) -> Result<(), String> {
        let node = std::cell::RefCell::new(&mut *self);
        read_ick_hybrid(
            disk,
            ick,
            IckReaderConfig::default(),
            |record| {
                let mut node = node.borrow_mut();
                let Booting { ks, lent } = &mut **node;
                ks.apply_record(&record, BOOT_NOW, BOOT_ANCHOR, lent)
                    .map(|_| ())
                    .map_err(|e| format!("image: {e:?}"))
            },
            |section| {
                apply_ref_section(node.borrow_mut().table_mut(), &section, flushed)
                    .map_err(|e| format!("refs: {e}"))
            },
            |section| {
                apply_live_set_section(node.borrow_mut().table_mut(), &section);
                Ok(())
            },
            |section| {
                apply_blob_ref_section(node.borrow_mut().table_mut(), &section);
                Ok(())
            },
            |_| Err("an index-sidecar section in this image".to_owned()),
        )
        .map_err(|e| format!("checkpoint load failed: {e:?}"))?;
        let Booting { ks, lent } = self;
        lent.machine.end_of_checkpoint(ks.tiered_store_mut(NS).expect("materialized"));
        Ok(())
    }

    /// The tail (ADR-0174 D3): every record through the dispatcher — the
    /// keyspace parks each marker until its mutation, which drains it
    /// after the room question and a `DEL`'s reads.
    fn replay_tail(&mut self, tail: &[u8]) -> Result<(), String> {
        let mut rest = tail;
        while !rest.is_empty() {
            let (record, consumed) = decode_record(rest).expect("tail records decode");
            match record {
                RecordView::ColdDisplace { .. }
                | RecordView::StringPostImage { .. }
                | RecordView::StringExtentRef { .. }
                | RecordView::Delete { .. } => {}
                other @ (RecordView::ExpireAt { .. }
                | RecordView::NsOp { .. }
                | RecordView::CkptBegin { .. }
                | RecordView::DocDelta { .. }
                | RecordView::DocFull { .. }) => {
                    return Err(format!("modeled tail carries {other:?}"));
                }
            }
            self.ks
                .apply_record(&record, BOOT_NOW, BOOT_ANCHOR, &mut self.lent)
                .map_err(|e| format!("tail record: {e:?}"))?;
            rest = &rest[consumed..];
        }
        match self.ks.displace_register_len() {
            0 => Ok(()),
            n => Err(format!("the tail ends with {n} unpaired displacement markers")),
        }
    }

    /// The end of replay (ADR-0174 R10): a boot that demoted settles every
    /// RAM record it has not sealed; ADR-0093's rebuild settles through
    /// the machine's read (the held handle, the key window); then the
    /// hand-over, and the table leaves the keyspace for the life.
    fn finish(
        mut self,
        settled_at_boot: &mut u64,
    ) -> Result<(TieredTable, HandedOver<SimDisk>, ReplayCounters), String> {
        let Booting { ks, lent } = &mut self;
        let table = ks.tiered_store_mut(NS).expect("materialized");
        let machine = &mut lent.machine;
        machine.end_of_replay(table);
        while machine.settle_step(table, PAGE).map_err(|e| format!("end settle: {e}"))?
            == SettleProgress::More
        {}
        table
            .rebuild_shadow_tickets(|slot| -> Result<KeyWindow, String> {
                let window = machine
                    .read_key_window(slot.cold)
                    .map_err(|e| format!("unreadable while its slot is live: {e}"))?;
                *settled_at_boot += 1;
                Ok(KeyWindow { bytes: window.bytes.to_vec(), left: window.left })
            })
            .map_err(|e| e.to_string())?;
        let Booting { mut ks, lent } = self;
        let table = ks.tiered_store_mut(NS).expect("materialized");
        let done = lent.machine.hand_over(table).map_err(|e| format!("hand-over: {e}"))?;
        let table = std::mem::replace(table, tiered_table(&Spec::of(None), 0, table.hasher()));
        Ok((table, done.handed, done.counters))
    }
}

fn tiered_table(spec: &Spec, origin: u64, hasher: KeyHasher) -> TieredTable {
    let mut table =
        TieredTable::new(space_config(spec, origin), spec.live, 1024, hasher).expect("ring");
    table.set_blob_config(spec.blob());
    // The shadow arm (M4.5-S37, ADR-0093 D8) runs on in this harness —
    // the store-level DST's authority over the mechanism.
    table.set_shadow_enabled(true);
    table
}

fn flush_config(shard: &Path) -> TierFlushConfig {
    TierFlushConfig {
        shard_dir: shard.to_path_buf(),
        cell: 0,
        ns: NS,
        mode: TierIoMode::Buffered,
        file_capacity: FILE_CAPACITY,
        slice_bytes: PAGE,
    }
}

fn demote() -> DemotionConfig {
    // A small mutable fraction keeps all three residency classes in
    // play at this corpus size.
    DemotionConfig { mem_budget_bytes: BUDGET, mutable_permille: 40, slice_bytes: PAGE }
}

/// `boot` raised to its whole ring: `MEM-BUDGET` up to the ring `boot`
/// reserved (a window above the ring is refused, ADR-0062 D3) — the
/// two-crash row's second window, and a variant's live one.
fn raise(boot: DemotionConfig) -> DemotionConfig {
    let ring = u64::try_from(boot.ring_reserve_bytes().expect("valid budget")).expect("u64");
    DemotionConfig { mem_budget_bytes: ring - boot.slice_bytes, ..boot }
}

/// The two-crash row's raised window, at the scenario's own spec.
fn raised() -> DemotionConfig {
    raise(demote())
}

/// Every address a displacement marker of `tail` names.
fn tail_marker_addrs(tail: &[u8]) -> BTreeSet<u64> {
    let mut addrs = BTreeSet::new();
    let mut rest = tail;
    // Bound: one decode per tail record.
    while !rest.is_empty() {
        let (record, consumed) = decode_record(rest).expect("tail records decode");
        if let RecordView::ColdDisplace { old_addr, .. } = record {
            addrs.insert(old_addr);
        }
        rest = &rest[consumed..];
    }
    addrs
}

fn space_config(spec: &Spec, origin: u64) -> AddressSpaceConfig {
    AddressSpaceConfig {
        reserve_bytes: spec.boot.ring_reserve_bytes().expect("valid budget"),
        page_bytes: PAGE as usize,
        life_origin: LogicalAddr::from_raw(origin).expect("48-bit"),
    }
}

/// Reads one cold record straight from the tier bytes through the
/// catalog (CRC-verified; the read path the §3.1 oracle stands on).
fn read_cold(disk: &SimDisk, flush: &TierFlush<SimDisk>, addr: u64, len: usize) -> Option<Vec<u8>> {
    let contains = |base: u64, flen: u64| addr >= base && addr + len as u64 <= base + flen;
    let (base, path) = flush
        .sealed()
        .iter()
        .find(|m| contains(m.base.to_raw(), m.data_len))
        .map(|m| (m.base.to_raw(), m.path.clone()))
        .or_else(|| {
            let (_, base, _, durable_len, path) = flush.active()?;
            contains(base.to_raw(), durable_len).then(|| (base.to_raw(), path.to_path_buf()))
        })?;
    let file = disk.open_read(&path).ok()?;
    let (first, count, skip) = tier_frame_span(addr - base, len);
    let from = tier_frame_offset(first);
    let span = count as usize * TIER_FRAME_BYTES;
    let mut window = vec![0u8; span];
    let mut done = 0usize;
    while done < span {
        use inf_log::fs::SegmentFile;
        let n = file.read_at(from + done as u64, &mut window[done..]).ok()?;
        if n == 0 {
            return None;
        }
        done += n;
    }
    let mut out = Vec::new();
    tier_extract(&window, skip, len, &mut out).ok()?;
    Some(out)
}

/// Reads one whole cold record: the header window sizes it, the exact
/// span follows (never a model-length read — a colliding cold candidate
/// is another key of another length, ADR-0093 A7).
fn read_cold_record(disk: &SimDisk, flush: &TierFlush<SimDisk>, addr: u64) -> Option<Vec<u8>> {
    let head = read_cold(disk, flush, addr, TieredTable::RECORD_HEADER_LEN)?;
    let len = TieredTable::record_len_from_header(&head);
    read_cold(disk, flush, addr, len)
}

/// The op mix's key (ADR-0093 A7): one in sixteen is a crafted colliding
/// key — either side of one of `pairs` — so the shadow, `DEL`, walk and
/// recovery paths meet two real keys with one hash on every seed.
fn seeded_key(rng: &mut SplitMix64, keys: u64, crafted: &[[u8; 48]]) -> Vec<u8> {
    if rng.next_u64().is_multiple_of(16) {
        return crafted[(rng.next_u64() % crafted.len() as u64) as usize].to_vec();
    }
    let idx = rng.next_u64() % keys;
    format!("rec:{idx:05}").into_bytes()
}

/// The crafted keys the op mix draws from: four colliding pairs, and —
/// review of 2026-08-30, F-L07-01 (batch 23) — two colliding **triples**,
/// so a boot can find two cold slots of one hash beside one RAM sibling
/// and rebuild several tickets on one winner (a pair never yields more
/// than one).
fn crafted_keys(seed: u64) -> Vec<[u8; 48]> {
    let mut crafted: Vec<[u8; 48]> = Vec::with_capacity(14);
    for i in 0..4u64 {
        let (a, b) = forced_collision_pair(seed ^ i.wrapping_mul(P_TAG));
        crafted.push(a);
        crafted.push(b);
    }
    for i in 0..2u64 {
        crafted.extend(inf_store::forced_collision_triple(
            seed ^ 0x7C7C ^ (i + 4).wrapping_mul(P_TAG),
        ));
    }
    crafted
}

/// Tag spread for the crafted pairs (four unrelated pairs per seed).
const P_TAG: u64 = 0x9E37_79B9_7F4A_7C15;

/// Long records a spec variant's fill writes per life at least
/// (`Run::fill_replay_unit`), within `LONG_OPS_PAST_TARGET` ops past its
/// target: the class's engagement, each a chance for the boot to pad.
const LONG_RECORDS_PER_LIFE: u64 = 8;
const LONG_OPS_PAST_TARGET: u64 = 256;

/// Maintain rounds a parked live write waits through before the harness
/// calls it refused: each round demotes until it makes no progress, so
/// one frees the window above the budget; the rest are margin.
const PARK_ROUNDS_MAX: u32 = 4;

struct Run {
    /// The run's tier spec: the scenario's constants, or a variant's.
    spec: Spec,
    disk: SimDisk,
    shard: PathBuf,
    model: BTreeMap<Vec<u8>, Expect>,
    /// Encoded record-v1 tail since the last durable publish (the WAL's
    /// covered suffix; a publish truncates the checkpoint-covered
    /// prefix — the D7 rule made literal).
    tail: Vec<u8>,
    /// Retired-and-detached files awaiting unlink (the plane's
    /// pin-analog queue; some are deliberately left for the boot GC —
    /// the swap ↔ unlink crash window).
    pending_unlink: Vec<TierFileMeta>,
    /// Batch 23 (ADR-0093 A12): the cold address of one ticket the
    /// reconciler is made to leave open (its reads "fail") across the
    /// walk and the cut, so its winner is sealed and flushed below the
    /// walk watermark while the ticket is open.
    held_twin: Option<u64>,
    /// The held ticket's key and hash (diagnostics for the A12 row).
    held_key: Option<(Vec<u8>, u64)>,
    /// The record bytes of the images the last published checkpoint
    /// holds, and of the walk in progress: with the tail's, the bytes the
    /// next boot re-appends — the harness's own measure of the replay
    /// unit, independent of the table (ADR-0174 D6's control leg).
    published_image_bytes: u64,
    walk_image_bytes: u64,
    /// The refs the walk in progress emitted, and those of the published
    /// checkpoint: the two-crash row takes its keys from the latter.
    walk_refs: Vec<(u64, u64)>,
    published_refs: Vec<(u64, u64)>,
    /// The two-crash row's keys: (key, hash, the ref's address).
    two_crash: Vec<(Vec<u8>, u64, u64)>,
    report: RecoveryReport,
}

impl Run {
    fn maintain(&mut self, life: &mut Life) {
        loop {
            let sealed = life.table.seal_slice();
            let f = life.table.flush_slice(&mut life.flush).expect("flush slice");
            let released = life.table.release_slice();
            if sealed + released + f.appended_bytes + u64::from(f.gaps_crossed) == 0 {
                break;
            }
        }
        self.reclaim_blobs(life, "maintain");
    }

    /// The extent-reclaim slice (M4-S17, ADR-0061 D5; disposal per
    /// ADR-0096): candidates whose killing record's staging epoch is
    /// covered dispose here — deaths unlink, boot orphans quarantine
    /// (probe + rename, the plane's exact dispatch), second verdicts
    /// unlink the twin — with the early-free oracle armed (a model-live
    /// extent handed out is a violation, immediately).
    fn reclaim_blobs(&mut self, life: &mut Life, when: &str) {
        use inf_log::blob::{probe_extent_file, quarantine_extent_file, unlink_quarantined_file};
        let durable = life.table.wal_epoch();
        loop {
            let work = life.table.extent_reclaim_work(durable, 4);
            if work.is_empty() {
                break;
            }
            for candidate in work {
                let id = candidate.extent_id;
                if self.model.values().any(|e| e.extent == Some(id)) {
                    self.report
                        .violations
                        .push(format!("{when}: early free — extent {id} is model-live"));
                    life.table.extent_reclaim_done(id);
                    continue;
                }
                match candidate.origin {
                    inf_store::ReclaimOrigin::Death => {
                        unlink_extent_file(&self.disk, &self.shard, ExtentId(id))
                            .expect("sim unlink");
                        life.table.extent_reclaim_done(id);
                        self.report.blob_extents_reclaimed += 1;
                    }
                    inf_store::ReclaimOrigin::BootOrphan => {
                        let path = self
                            .shard
                            .join("cold")
                            .join(inf_log::blob::extent_file_name(ExtentId(id)));
                        match probe_extent_file(&self.disk, &path) {
                            Ok(header) if header.extent_id == ExtentId(id) => {
                                quarantine_extent_file(&self.disk, &self.shard, ExtentId(id))
                                    .expect("sim quarantine");
                                life.table.extent_reclaim_quarantined(id);
                            }
                            _ => {
                                unlink_extent_file(&self.disk, &self.shard, ExtentId(id))
                                    .expect("sim unlink");
                                life.table.extent_reclaim_done(id);
                                self.report.blob_extents_reclaimed += 1;
                            }
                        }
                    }
                    inf_store::ReclaimOrigin::Quarantined => {
                        unlink_quarantined_file(&self.disk, &self.shard, ExtentId(id))
                            .expect("sim unlink twin");
                        life.table.extent_reclaim_done(id);
                        self.report.blob_extents_reclaimed += 1;
                    }
                }
            }
        }
    }

    /// Stages one effect through the production `stage_wal` path (the
    /// reclaim-gate epoch), recycling the ring on admission refusal (the
    /// modeled tail is the durable record here, not the ring).
    fn stage(&mut self, life: &mut Life, effect: &MutationEffect<'_>) {
        if life.table.stage_wal(&mut life.ring, effect).is_err() {
            life.ring = StagingRing::new(StagingConfig::default());
            life.table.stage_wal(&mut life.ring, effect).expect("a fresh ring has room");
        }
    }

    /// A bounded burst of copy-forward slices (M4-S15, ADR-0059 D2):
    /// work request → catalog cold read → apply, with the cold floor
    /// asserted monotone. A stalled slice runs one maintain round (the
    /// refusal-aware admission's resolver) and continues.
    fn compact(&mut self, life: &mut Life, pressure: bool, rounds: u32, when: &str) {
        let floor_before = life.table.cold_floor();
        let mut budget = PAGE * 4;
        for _ in 0..rounds {
            match life.table.compaction_work(&life.flush, pressure, budget) {
                CompactionWork::Read { file_id, addr, len } => {
                    let Some(bytes) =
                        self.read_scan_chunk(&life.flush, file_id, addr.to_raw(), len)
                    else {
                        self.report
                            .violations
                            .push(format!("{when}: compaction read failed (file {file_id})"));
                        return;
                    };
                    let applied = life.table.compaction_apply(file_id, addr, &bytes);
                    self.report.relocations += u64::from(applied.relocated);
                    if applied.file_scanned {
                        self.report.files_scanned += 1;
                    }
                    // An oversized record re-reads at exactly its length
                    // (minimum-one-record progress, bounded at one).
                    budget = if applied.need > 0 { applied.need } else { PAGE * 4 };
                    if applied.stalled {
                        self.report.compaction_stalls += 1;
                        self.maintain(life);
                    }
                }
                CompactionWork::Idle => break,
            }
        }
        if life.table.cold_floor() < floor_before {
            self.report.violations.push(format!("{when}: the cold floor moved backwards"));
        }
    }

    /// Reads one scan chunk of a sealed catalog file (compaction
    /// candidates are sealed by eligibility, so the range path resolves).
    fn read_scan_chunk(
        &self,
        flush: &TierFlush<SimDisk>,
        file_id: u32,
        addr: u64,
        len: u64,
    ) -> Option<Vec<u8>> {
        debug_assert!(flush.sealed().iter().any(|m| m.id == file_id), "candidates are sealed");
        read_cold(&self.disk, flush, addr, usize::try_from(len).expect("chunk fits"))
    }

    /// The replay-above-window seed class (ADR-0174 D1): before the cut,
    /// tiered writes fill the tail until the record bytes the next boot
    /// re-appends — the published checkpoint's images and the tail's
    /// records — reach a multiple of the window drawn from the hostile row
    /// (window − 1 page, the window, window + 1 page, 3 ×, 16 ×), demoted
    /// live as a running node would. The mix: fresh keys with values drawn
    /// from the record-length row (a 1-byte key and value, a typical value,
    /// the inline maximum), rewrites of fill keys at distances under and
    /// over a window, deletes, and shadow writes over demoted keys. Counts
    /// the life when the unit exceeds the window. In a spec variant the
    /// window is the boot's, three writes in four are long
    /// ([`fill_op_long`](Self::fill_op_long)) and the fill writes
    /// [`LONG_RECORDS_PER_LIFE`] long records at least.
    fn fill_replay_unit(
        &mut self,
        life: &mut Life,
        rng: &mut SplitMix64,
        life_index: u64,
        fixed_target: Option<u64>,
    ) {
        let window = self.spec.window();
        let drawn = match rng.next_u64() % 8 {
            0 => window - PAGE,
            1 => window,
            2 | 3 => window + PAGE,
            4..=6 => 3 * window,
            _ => 16 * window,
        };
        let target = fixed_target.unwrap_or(drawn);
        // The held ticket's injected read error clears: its winner pins
        // release (ADR-0093 D3), and a window's worth of writes behind a
        // pin the reconciler can never lift would stall every writer, as
        // the plane's would until its stall timeout.
        self.held_twin = None;
        let mut unit = self.replay_unit_bytes();
        let mut fill: Vec<Vec<u8>> = Vec::new();
        let mut i = 0u64;
        // A variant's fill also writes its long records: a boot pads only
        // where one lands, and a unit already above its target may hold
        // none (the checkpoint's images and the phase's tail alone).
        let long_before = self.report.long_records_written;
        let long_owed = if self.spec.variant.is_some() { LONG_RECORDS_PER_LIFE } else { 0 };
        let mut past_target = 0u64;
        // Bound: each op appends a record or deletes one of the fill's
        // keys; one op in eight deletes, so the unit grows by at least a
        // record per eight ops until it reaches the target; past it, at
        // most `LONG_OPS_PAST_TARGET` ops for the long records owed.
        while unit < target
            || (self.report.long_records_written - long_before < long_owed
                && past_target < LONG_OPS_PAST_TARGET)
        {
            past_target += u64::from(unit >= target);
            let before = self.tail.len();
            let (key, op) = match self.spec.variant {
                None => fill_op(rng, &fill, life_index, i, self.spec.blob_threshold),
                Some(variant) => self.fill_op_long(life, rng, &fill, (life_index, i), variant),
            };
            if !fill.contains(&key) {
                fill.push(key.clone());
            }
            let violations = self.report.violations.len();
            self.apply_op(life, &key, op);
            if self.report.violations.len() > violations {
                // A live write found no room: the run is red already, and
                // a write that appends nothing would not end the fill.
                break;
            }
            unit += tail_record_bytes(&self.tail[before..]);
            i += 1;
            if i.is_multiple_of(64) {
                self.maintain(life);
            }
        }
        self.maintain(life);
        debug_assert_eq!(unit, self.replay_unit_bytes(), "the fill's own count");
        eprintln!(
            "inf-sim: m4-recovery life {life_index}: replay unit {unit} bytes, {} window(s) of \
             {window} (target {target}, {i} ops)",
            unit.div_ceil(window)
        );
        self.report.replay_unit_windows_max =
            self.report.replay_unit_windows_max.max(unit.div_ceil(window));
        if unit > window {
            self.report.replay_above_window_lives += 1;
        }
    }

    /// One op of a spec variant's fill (ADR-0174 D2 rule 6; the record's
    /// §6 second row): [`fill_op`]'s mix at the spec's threshold, with
    /// three writes in four long — a third each of the case's record
    /// (case (a): 2.9 pages; case (b): 3.9 pages), the longest the
    /// threshold and half the ring admit (in case (a) just above three
    /// pages, so the record spans four), and a length drawn between the
    /// case's shortest (case (a): 2.125 pages; case (b): the window less a
    /// page) and that longest — and, in case (b), one op in eight a fresh
    /// key of the longest length whose record is exactly half the ring.
    /// A write the live window cannot place after
    /// [`PARK_ROUNDS_MAX`] maintain rounds is written short instead: the
    /// live path would park on it until its stall timeout (at a need above
    /// the tail, FCR-STTIER-N4's shape) and acknowledge nothing.
    fn fill_op_long(
        &mut self,
        life: &mut Life,
        rng: &mut SplitMix64,
        fill: &[Vec<u8>],
        (life_index, i): (u64, u64),
        variant: SpecVariant,
    ) -> (Vec<u8>, Op) {
        let header = TieredTable::RECORD_HEADER_LEN as u64;
        let half_ring = self.spec.ring() / 2;
        let exact_half = variant == SpecVariant::Page && rng.next_u64().is_multiple_of(8);
        let (key, op) = if exact_half {
            let mut key = format!("half:{life_index:02}:{i:06}:").into_bytes();
            key.resize(inf_store::MAX_KEY_LEN, b'h');
            (key, Op::Set(Vec::new()))
        } else {
            fill_op(rng, fill, life_index, i, self.spec.blob_threshold)
        };
        let shadow = matches!(op, Op::SetShadow(_));
        let drawn = match op {
            Op::Set(value) | Op::SetShadow(value) => value,
            other @ (Op::Del | Op::SetBlob(_)) => return (key, other),
        };
        let key_len = key.len() as u64;
        let longest = (header + key_len + u64::from(self.spec.blob_threshold) - 1).min(half_ring);
        let (typical, shortest) = match variant {
            SpecVariant::RingTop => (29 * PAGE / 10, 2 * PAGE + PAGE / 8),
            SpecVariant::Page => (39 * PAGE / 10, self.spec.window() - PAGE + 1),
        };
        let len = if exact_half {
            half_ring
        } else if rng.next_u64() % 4 < 3 {
            match rng.next_u64() % 3 {
                0 => typical.min(longest),
                1 => longest,
                _ => shortest + rng.next_u64() % (longest - shortest + 1),
            }
        } else {
            header + key_len + drawn.len() as u64
        };
        let byte = (rng.next_u64() % 251) as u8;
        let value = if !self.placeable(life, usize::try_from(len).expect("half a ring")) {
            self.report.long_records_shortened += 1;
            vec![byte; 24 + (rng.next_u64() % 140) as usize]
        } else if len >= shortest {
            self.report.long_records_written += 1;
            vec![byte; usize::try_from(len - header - key_len).expect("half a ring")]
        } else {
            drawn
        };
        (key, if shadow { Op::SetShadow(value) } else { Op::Set(value) })
    }

    /// Whether the live window places `len` bytes within
    /// [`PARK_ROUNDS_MAX`] maintain rounds — [`park_for_room`]'s rounds,
    /// asked before the write is chosen, so a no is no violation: the
    /// generator writes something else.
    ///
    /// [`park_for_room`]: Self::park_for_room
    fn placeable(&mut self, life: &mut Life, len: usize) -> bool {
        for round in 0..PARK_ROUNDS_MAX {
            if matches!(life.table.space().room(len), Room::Fits) {
                return true;
            }
            self.park_round(life, round, len, "fill");
        }
        matches!(life.table.space().room(len), Room::Fits)
    }

    /// One round of the park on a full window: the reconciler from the
    /// second round on (a pinned twin holds release back, ADR-0093 D3),
    /// then demotion. A spec variant's live window holds two long records,
    /// so a write that still does not fit can wait on the partial-frame
    /// holdback alone: the barrier seal under backpressure (ADR-0056 D8)
    /// makes it claimable and demotion runs again. The scenario's own
    /// window, 257 pages, never waits on one frame.
    fn park_round(&mut self, life: &mut Life, round: u32, len: usize, when: &str) {
        if round > 0 {
            self.reconcile(life, 16, when);
        }
        self.maintain(life);
        if self.spec.variant.is_some()
            && !matches!(life.table.space().room(len), Room::Fits)
            && life.flush.append_cursor().is_some()
        {
            life.table.flush_barrier(&mut life.flush).expect("sim barrier seal");
            self.maintain(life);
        }
    }

    /// The plane's park on a full window, played here: a write the
    /// window cannot place waits for MAINTAIN's cycle — the reconciler,
    /// then demotion — and is then retried; the shape every live write
    /// meets after a boot that demoted, whose window holds the newest
    /// replayed records (ADR-0174 D5). The harness asks before it resolves,
    /// so nothing of the op is staged while it waits. Under a checkpoint
    /// walk release stops at the walk's watermark, so a write that still
    /// finds no room waits for the walk's end: the harness, which runs the
    /// walk's slices itself, drops the op unacknowledged (the model never
    /// saw it). Outside a walk, no room after [`PARK_ROUNDS_MAX`] rounds is
    /// a violation (the live path would answer its stall timeout). Returns
    /// whether the op may proceed.
    fn park_for_room(&mut self, life: &mut Life, len: usize) -> bool {
        for round in 0..PARK_ROUNDS_MAX {
            if matches!(life.table.space().room(len), Room::Fits) {
                return true;
            }
            self.report.writer_parks += 1;
            self.park_round(life, round, len, "park");
        }
        if matches!(life.table.space().room(len), Room::Fits) {
            return true;
        }
        // The held ticket's injected read error clears: its winner pins
        // release, and a full window behind a pin the reconciler can
        // never lift would stall every writer (the plane's, until its
        // stall timeout). The held row counts only a ticket open at the
        // cut, so this life's row reports nothing.
        if self.held_twin.take().is_some() {
            self.report.held_released_by_park += 1;
            // Rounds, not `reconcile_all`: under a pinned walk a verified
            // ticket settles only once the walk ends (ADR-0093 D5).
            for _ in 0..PARK_ROUNDS_MAX {
                self.reconcile(life, 16, "park");
            }
            self.maintain(life);
            if matches!(life.table.space().room(len), Room::Fits) {
                return true;
            }
        }
        if life.table.space().walk_watermark().is_some() {
            self.report.writes_parked_past_a_walk += 1;
            return false;
        }
        let space = life.table.space();
        self.report.violations.push(format!(
            "a live write of {len} bytes found no room after {PARK_ROUNDS_MAX} maintain rounds \
             ({:?}; head {} flushed {} tail {}, pin {:?}, {} tickets open)",
            space.room(len),
            space.head().to_raw(),
            space.flushed().to_raw(),
            space.tail().to_raw(),
            space.record_pin(),
            life.table.shadow_pending()
        ));
        false
    }

    /// The bytes the next boot re-appends: the published checkpoint's
    /// images and the tail's records (no bytes for a marker or a delete).
    fn replay_unit_bytes(&self) -> u64 {
        self.published_image_bytes + tail_record_bytes(&self.tail)
    }

    /// A boot's ADR-0174 D6 counters against the unit the harness knows
    /// it replayed — the control leg of the falsifier, both directions: a
    /// unit above the window must demote; one that fits with room for the
    /// page rounding and a ring-top hole must leave the zero set at zero.
    /// Folded into the report and the trace.
    fn note_boot(&mut self, counters: &ReplayCounters, unit: u64, life_index: u64) {
        let window = self.spec.window();
        let fits = unit + self.spec.fit_margin() <= window;
        if unit > window && counters.demote_steps == 0 {
            self.report.violations.push(format!(
                "life {life_index}: a replay unit of {unit} bytes above the {window}-byte window \
                 booted without a demote step ({counters:?})"
            ));
        }
        if fits && !counters.zero_set_is_zero() {
            self.report.violations.push(format!(
                "life {life_index}: a replay unit of {unit} bytes fits the {window}-byte window \
                 but the zero set moved ({counters:?})"
            ));
        }
        let r = &mut self.report;
        r.demoting_boots += u64::from(counters.demote_steps > 0);
        r.fitting_boots_checked += u64::from(fits);
        r.boot_replay.absorb(*counters);
        r.trace_hash = hash64(
            &[counters.demote_steps.to_le_bytes(), counters.tier_bytes.to_le_bytes()].concat(),
            r.trace_hash,
        );
    }

    /// The two-crash row's first half (ADR-0174 I10): up to eight keys the
    /// published checkpoint names by a ref, still slotted at that ref,
    /// take a shadow write — the record appends with no marker and the ref
    /// stays its twin — so the boot that demotes settles the ref against
    /// the record and chains it into the record's origins (R8).
    fn open_two_crash_rows(&mut self, life: &mut Life) {
        const ROWS_MAX: usize = 8;
        // A shadow write that finds no lone cold candidate falls back to a
        // plain one; at most twice as many writes as rows, each of a
        // 48-byte value and a key of at most 64 bytes, with its header.
        const WRITES_MAX: usize = 2 * ROWS_MAX;
        const ROW_BYTES_MAX: u64 = WRITES_MAX as u64 * 160;
        // The second boot must fit the raised window, or it would settle
        // the refs itself: a unit already past it cannot carry the row.
        let window = raised().mem_budget_bytes + raised().slice_bytes;
        if self.replay_unit_bytes() + ROW_BYTES_MAX + self.spec.fit_margin() > window {
            self.report.two_crash_unit_past_window += 1;
            return;
        }
        // The fill's open tickets settle first: their pinned bytes count
        // against the shadow pin cap a row's write is admitted under.
        self.reconcile_all(life, "two-crash rows");
        let refs = std::mem::take(&mut self.published_refs);
        // Bound: one cold read per ref of the published checkpoint, until
        // eight rows are open or sixteen writes were made.
        let mut writes = 0usize;
        for &(hash, addr) in &refs {
            if self.two_crash.len() == ROWS_MAX || writes == WRITES_MAX {
                break;
            }
            let Some(bytes) = read_cold_record(&self.disk, &life.flush, addr) else { continue };
            let key = TieredTable::decode_record(&bytes).key.to_vec();
            let at = LogicalAddr::from_raw(addr).expect("48-bit");
            let slotted =
                matches!(life.table.lookup(&key, hash, &[]), TieredLookup::Cold(a) if a == at);
            if key.starts_with(inf_store::COLLISION_KEY_PREFIX) || !slotted {
                continue;
            }
            self.apply_op(life, &key, Op::SetShadow(vec![0x2C; 48]));
            writes += 1;
            if life.table.shadow_tickets().any(|ticket| ticket.cold == at) {
                self.two_crash.push((key, hash, addr));
            }
        }
        self.published_refs = refs;
        self.report.two_crash_rows_opened = self.two_crash.len() as u64;
    }

    /// The two-crash row after the boot that demoted: a row whose ref the
    /// boot removed while no marker of the tail named it was settled by
    /// the boot (R7, R8) and stays a row; the rest leave it.
    fn settle_two_crash_rows(&mut self, table: &TieredTable) {
        let markers = tail_marker_addrs(&self.tail);
        self.two_crash.retain(|(_, hash, addr)| {
            let at = LogicalAddr::from_raw(*addr).expect("48-bit");
            !table.contains_pair(*hash, at) && !markers.contains(addr)
        });
        self.report.two_crash_rows_settled = self.two_crash.len() as u64;
    }

    /// The two-crash row (ADR-0174 I10): after the boot that settled the
    /// rows' refs, the window rises to its ring, each row's key is deleted
    /// live — its `DEL` stages a marker for every origin, the settled ref
    /// among them — and the power is cut before a checkpoint publishes.
    /// The second boot replays the same checkpoint and the whole tail
    /// inside the raised window, so it settles nothing and only the
    /// marker can remove the ref. The oracle: every row's ref is slotted
    /// once the checkpoint loads and gone once the tail replays, and its
    /// key misses. Engagement: settled rows and a second boot that fits,
    /// else `VACUOUS`.
    fn two_crash_coda(&mut self, mut life: Life, hasher: KeyHasher, seed: u64) {
        if self.two_crash.is_empty() && self.report.two_crash_unit_past_window > 0 {
            return;
        }
        if self.two_crash.is_empty() {
            self.report.violations.push(format!(
                "TWO-CRASH VACUOUS: {} rows opened, none settled by the boot that demoted",
                self.report.two_crash_rows_opened
            ));
            return;
        }
        life.table.set_demotion(raised()).expect("the raised window is the ring");
        let rows = std::mem::take(&mut self.two_crash);
        for (key, _, _) in &rows {
            if self.model.contains_key(key) {
                self.apply_op(&mut life, key, Op::Del);
            }
        }
        self.report.state.number(b"two-crash-cut", rows.len() as u64);
        self.disk.power_cut(seed ^ 0x2C2C_0000);
        drop(life);
        let booted = self.two_crash_boot(&rows, hasher);
        let (table, handed, counters) = match booted {
            Ok(done) => done,
            Err(err) => {
                self.report.violations.push(format!("two-crash boot 2: {err}"));
                return;
            }
        };
        let unit = self.replay_unit_bytes();
        let window = raised().mem_budget_bytes + raised().slice_bytes;
        if unit + self.spec.fit_margin() > window || !counters.zero_set_is_zero() {
            self.report.violations.push(format!(
                "TWO-CRASH VACUOUS: the second boot does not fit its {window}-byte window \
                 (unit {unit}, {counters:?})"
            ));
        }
        self.report.state.digest(table.simulation_digest());
        let life = Life {
            table,
            flush: handed.flush,
            ring: StagingRing::new(StagingConfig::default()),
            flush_lag: false,
        };
        for (key, hash, _) in &rows {
            let resurrected = match life.table.lookup(key, *hash, &[]) {
                TieredLookup::Miss => false,
                TieredLookup::Ram(_) => true,
                TieredLookup::Cold(addr) => {
                    read_cold_record(&self.disk, &life.flush, addr.to_raw()).is_some_and(|bytes| {
                        TieredTable::decode_record(&bytes).key == key.as_slice()
                    })
                }
            };
            if resurrected {
                self.report.violations.push(format!(
                    "TWO-CRASH VIOLATION: the deleted key {} serves after the second boot",
                    String::from_utf8_lossy(key)
                ));
            }
        }
        self.audit(&life, "two-crash boot 2");
    }

    /// The second boot of the two-crash row, through the raised window:
    /// each row's ref must be slotted after the checkpoint (engagement)
    /// and gone after the tail (a marker of the live `DEL` removed it).
    fn two_crash_boot(
        &mut self,
        rows: &[(Vec<u8>, u64, u64)],
        hasher: KeyHasher,
    ) -> Result<(TieredTable, HandedOver<SimDisk>, ReplayCounters), String> {
        let manifest = read_manifest(&self.disk, &self.shard)
            .map_err(|e| format!("manifest unreadable: {e}"))?
            .ok_or("published manifest lost")?;
        let tier = manifest.tier_ns(NS.0).cloned().ok_or("manifest lost its tier section")?;
        let recovered = recover_tiered_ns(
            self.disk.clone(),
            &tier,
            manifest.ckpt_id,
            flush_config(&self.shard),
            space_config(&self.spec, 0),
            raised(),
            1024,
            hasher,
        )
        .map_err(|e| format!("tier recovery: {e}"))?;
        let mut node = Booting::new(recovered.table, recovered.replay, hasher, &self.spec);
        let ick = self.shard.join(ick_file_name(manifest.ckpt_id));
        node.load_checkpoint(&self.disk, &ick, tier.flushed)?;
        node.table_mut().set_shadow_enabled(true);
        let pair = |table: &TieredTable, &(_, hash, addr): &(Vec<u8>, u64, u64)| {
            table.contains_pair(hash, LogicalAddr::from_raw(addr).expect("48-bit"))
        };
        let slotted: Vec<bool> = rows.iter().map(|row| pair(node.table(), row)).collect();
        node.replay_tail(&self.tail)?;
        for (row, &was) in rows.iter().zip(&slotted) {
            if !was {
                continue;
            }
            if pair(node.table(), row) {
                self.report.violations.push(format!(
                    "TWO-CRASH VIOLATION: the ref at {} of {} the first boot settled outlived the \
                     second boot's tail — no marker of the live DEL named it (I10)",
                    row.2,
                    String::from_utf8_lossy(&row.0)
                ));
            } else {
                self.report.two_crash_markers_removed += 1;
            }
        }
        if self.report.two_crash_markers_removed == 0 {
            self.report.violations.push(format!(
                "TWO-CRASH VACUOUS: none of {} settled refs was slotted after the second boot's \
                 checkpoint",
                rows.len()
            ));
        }
        node.finish(&mut self.report.shadow_settled_at_boot)
    }

    /// One live-path mutation, recorded into the modeled tail with its
    /// displacement marker (ADR-0057 D4 — unconditional for displacing
    /// mutations).
    fn apply_op(&mut self, life: &mut Life, key: &[u8], op: Op) {
        let record_len = TieredTable::RECORD_HEADER_LEN
            + key.len()
            + match &op {
                Op::Set(value) | Op::SetShadow(value) => value.len(),
                Op::SetBlob(_) => EXTENT_REF_LEN,
                Op::Del => 0,
            };
        if !matches!(op, Op::Del) && !self.park_for_room(life, record_len) {
            return;
        }
        let hash = life.table.hash_key(key);
        if key.starts_with(inf_store::COLLISION_KEY_PREFIX) {
            self.report.shadow_collide_ops += 1;
        }
        // The shadow path (ADR-0093 D2): probe → admit → insert →
        // register → the image alone into the tail (no marker). Any
        // other probe answer or a refusal is a plain SET.
        let op = match op {
            Op::SetShadow(value) => {
                let record_len = TieredTable::RECORD_HEADER_LEN + key.len() + value.len();
                match life.table.shadow_probe(key, hash) {
                    inf_store::ShadowProbe::One(cold)
                        if life.table.shadow_admit(hash, cold, record_len).is_ok() =>
                    {
                        let winner = life.table.insert(key, &value, hash).expect("fits");
                        life.table.register_shadow(hash, cold, winner);
                        self.stage(life, &MutationEffect::StringSet { ns: NS, key, value: &value });
                        RecordView::StringPostImage { ns: NS, key, value: &value }
                            .encode_into(&mut self.tail);
                        self.report.tail_records += 1;
                        self.report.shadow_opened += 1;
                        self.model.insert(key.to_vec(), Expect { value, extent: None });
                        return;
                    }
                    inf_store::ShadowProbe::RamHit(_)
                    | inf_store::ShadowProbe::Miss
                    | inf_store::ShadowProbe::NoCandidate
                    | inf_store::ShadowProbe::One(_)
                    | inf_store::ShadowProbe::Ticketed(_)
                    | inf_store::ShadowProbe::Many => Op::Set(value),
                }
            }
            other @ (Op::Set(_) | Op::SetBlob(_) | Op::Del) => other,
        };
        // The plane's resolve: RAM verifies in place; a cold candidate is
        // read and its full key compared, a mismatch (a fingerprint false
        // positive, or a crafted collision — ADR-0093 A7) excluded and
        // the probe retried.
        let mut exclude: Vec<LogicalAddr> = Vec::new();
        let displaced = loop {
            match life.table.lookup(key, hash, &exclude) {
                TieredLookup::Ram(addr) => {
                    let parts = life.table.record(addr);
                    break Some((addr, parts.encoded_len, parts.version));
                }
                TieredLookup::Cold(addr) => {
                    let bytes = read_cold_record(&self.disk, &life.flush, addr.to_raw())
                        .expect("cold record readable");
                    let parts = TieredTable::decode_record(&bytes);
                    if parts.key == key {
                        break Some((addr, parts.encoded_len, parts.version));
                    }
                    exclude.push(addr);
                }
                TieredLookup::Miss => break None,
            }
        };
        match op {
            Op::Set(value) => {
                match displaced {
                    Some((old, old_len, old_version)) => {
                        // Relocation-origin markers first (ADR-0059 D9):
                        // one ColdDisplace per address an un-superseded
                        // checkpoint may still ref this record by, then
                        // the ordinary marker for the live address.
                        for (origin, _) in life.table.take_displacement_origins(hash, old) {
                            RecordView::ColdDisplace { ns: NS, old_addr: origin }
                                .encode_into(&mut self.tail);
                        }
                        RecordView::ColdDisplace { ns: NS, old_addr: old.to_raw() }
                            .encode_into(&mut self.tail);
                        life.table
                            .update(key, &value, hash, old, old_len, old_version)
                            .expect("fits");
                    }
                    None => {
                        life.table.insert(key, &value, hash).expect("fits");
                    }
                }
                self.stage(life, &MutationEffect::StringSet { ns: NS, key, value: &value });
                RecordView::StringPostImage { ns: NS, key, value: &value }
                    .encode_into(&mut self.tail);
                self.report.tail_records += 1;
                self.model.insert(key.to_vec(), Expect { value, extent: None });
            }
            Op::SetShadow(_) => unreachable!("rewritten to Set above"),
            Op::SetBlob(value) => {
                // M4-S17 (ADR-0061 D3): extent bytes → fdatasync →
                // sealed token → only then the referencing record. The
                // cut physics are real — an unfsynced extent would tear,
                // and only the token proves it cannot be referenced.
                let extent_id = ExtentId(life.table.allocate_extent_id());
                let mut w = ExtentWriter::create(
                    &self.disk,
                    &self.shard,
                    extent_id,
                    0,
                    NS,
                    value.len() as u64,
                    TierIoMode::Buffered,
                )
                .expect("extent create");
                for chunk in value.chunks(29) {
                    w.append_chunk(chunk).expect("extent chunk");
                }
                let sealed = w.finish().expect("extent fsync");
                life.table.note_blob_bytes(sealed.device_bytes());
                if let Some((old, _, _)) = displaced {
                    for (origin, _) in life.table.take_displacement_origins(hash, old) {
                        RecordView::ColdDisplace { ns: NS, old_addr: origin }
                            .encode_into(&mut self.tail);
                    }
                    RecordView::ColdDisplace { ns: NS, old_addr: old.to_raw() }
                        .encode_into(&mut self.tail);
                }
                self.stage(
                    life,
                    &MutationEffect::StringSetExtent {
                        ns: NS,
                        key,
                        extent_id: sealed.extent_id().0,
                        offset: 0,
                        len: sealed.data_len(),
                    },
                );
                match displaced {
                    Some((old, old_len, old_version)) => {
                        life.table
                            .update_extent(key, hash, &sealed, old, old_len, old_version)
                            .expect("fits");
                    }
                    None => {
                        life.table.insert_extent(key, hash, &sealed).expect("fits");
                    }
                }
                RecordView::StringExtentRef {
                    ns: NS,
                    key,
                    extent_id: sealed.extent_id().0,
                    offset: 0,
                    len: sealed.data_len(),
                }
                .encode_into(&mut self.tail);
                self.report.tail_records += 1;
                self.report.blobs_written += 1;
                self.model.insert(key.to_vec(), Expect { value, extent: Some(extent_id.0) });
            }
            Op::Del => {
                if let Some((addr, len, _)) = displaced {
                    // ADR-0093 D3/A10: every ticket naming the winner is
                    // verified before its delete and each same-key twin
                    // takes the marker path (its origins, its own
                    // `ColdDisplace`, `delete`) — the plane's
                    // `delete_one` rule, played here.
                    for ticket in life.table.shadow_tickets_of_winner(addr) {
                        self.verify_twin_for_delete(life, ticket, "del");
                    }
                    for (origin, _) in life.table.take_displacement_origins(hash, addr) {
                        RecordView::ColdDisplace { ns: NS, old_addr: origin }
                            .encode_into(&mut self.tail);
                    }
                    RecordView::ColdDisplace { ns: NS, old_addr: addr.to_raw() }
                        .encode_into(&mut self.tail);
                    self.stage(life, &MutationEffect::Delete { ns: NS, key });
                    RecordView::Delete { ns: NS, key }.encode_into(&mut self.tail);
                    self.report.tail_records += 1;
                    life.table.delete(hash, addr, len);
                    self.model.remove(key);
                }
            }
        }
    }

    /// The cardinality oracle (ADR-0093 I5): with no ticket open, the
    /// keys the table counts are exactly the model's — an orphan slot
    /// (a twin that survived its key) or a lost key would show here
    /// before any read does.
    fn audit_cardinality(&mut self, life: &Life, when: &str) {
        if life.table.shadow_pending() != 0 {
            return;
        }
        if life.table.len() != self.model.len() {
            // Name the anomaly: every slot decoded (RAM directly, cold
            // through the catalog), counted per key, compared to the
            // model — the diagnosis a bare count cannot give.
            let mut slots: Vec<(u64, LogicalAddr)> = Vec::new();
            let mut cursor = 0u64;
            loop {
                cursor = life.table.scan_slots(cursor, 256, |hash, addr| slots.push((hash, addr)));
                if cursor == 0 {
                    break;
                }
            }
            let mut per_key: BTreeMap<Vec<u8>, Vec<String>> = BTreeMap::new();
            for (_, addr) in slots {
                let (key, class) = if life.table.space().resolve(addr) == inf_store::AddrClass::Cold
                {
                    let head = read_cold(
                        &self.disk,
                        &life.flush,
                        addr.to_raw(),
                        TieredTable::RECORD_HEADER_LEN,
                    );
                    let image = head
                        .map(|h| TieredTable::record_len_from_header(&h))
                        .and_then(|len| read_cold(&self.disk, &life.flush, addr.to_raw(), len));
                    match image {
                        Some(image) => (TieredTable::decode_record(&image).key.to_vec(), "cold"),
                        None => (b"<unreadable>".to_vec(), "cold"),
                    }
                } else {
                    (life.table.record(addr).key.to_vec(), "ram")
                };
                per_key.entry(key).or_default().push(format!("{class}@{}", addr.to_raw()));
            }
            let anomalies: Vec<String> = per_key
                .iter()
                .filter(|(key, at)| at.len() != 1 || !self.model.contains_key(*key))
                .map(|(key, at)| format!("{}: {at:?}", String::from_utf8_lossy(key)))
                .take(6)
                .collect();
            self.report.violations.push(format!(
                "{when}: cardinality — the table counts {} keys, the model {} — {anomalies:?}",
                life.table.len(),
                self.model.len()
            ));
        }
    }

    /// The oracle: every model key serves its exact bytes; every other
    /// key misses. Content only — versions are per-life (D3), addresses
    /// are per-life (§3.1).
    fn audit(&mut self, life: &Life, when: &str) {
        for (key, expect) in &self.model {
            let hash = life.table.hash_key(key);
            let mut exclude: Vec<LogicalAddr> = Vec::new();
            let got = loop {
                match life.table.lookup(key, hash, &exclude) {
                    TieredLookup::Ram(addr) => {
                        let parts = life.table.record(addr);
                        match parts.extent_ref() {
                            // M4-S17: the record carries a reference —
                            // the value serves through the CRC-verified
                            // extent reader (chunked, the cold-read
                            // shape), and the refcount map must agree.
                            Some(ext) => {
                                if life.table.extent_reference_at(addr)
                                    != Some((ext.extent_id, ext.len))
                                {
                                    break None; // map desync = a loss shape
                                }
                                break read_blob(&self.disk, &self.shard, ext);
                            }
                            None => break Some(parts.value.to_vec()),
                        }
                    }
                    TieredLookup::Cold(addr) => {
                        match read_cold_record(&self.disk, &life.flush, addr.to_raw()) {
                            Some(bytes) => {
                                let parts = TieredTable::decode_record(&bytes);
                                if parts.key == key.as_slice() {
                                    match parts.extent_ref() {
                                        Some(ext) => {
                                            break read_blob(&self.disk, &self.shard, ext);
                                        }
                                        None => break Some(parts.value.to_vec()),
                                    }
                                }
                                exclude.push(addr); // fingerprint false positive
                            }
                            None => break None,
                        }
                    }
                    TieredLookup::Miss => break None,
                }
            };
            self.report.keys_audited += 1;
            match got {
                Some(value) if value == expect.value => {
                    self.report.trace_hash = hash64(&value, self.report.trace_hash);
                }
                Some(_) => self
                    .report
                    .violations
                    .push(format!("{when}: wrong bytes for {}", String::from_utf8_lossy(key))),
                None => {
                    let shape = match life.table.lookup(key, hash, &[]) {
                        TieredLookup::Miss => "index miss".to_string(),
                        TieredLookup::Cold(addr) => {
                            format!("dangling cold ref at addr {}", addr.to_raw())
                        }
                        TieredLookup::Ram(_) => "ram (transient?)".to_string(),
                    };
                    self.report.violations.push(format!(
                        "{when}: never-none violated — {} lost ({shape})",
                        String::from_utf8_lossy(key)
                    ));
                }
            }
        }
    }
}

enum Op {
    Set(Vec<u8>),
    /// An out-of-line value (M4-S17): always at or above the threshold.
    SetBlob(Vec<u8>),
    /// A SET that takes the shadow path when the key's only exact
    /// candidate is cold (M4.5-S37); otherwise a plain `Set`.
    SetShadow(Vec<u8>),
    Del,
}

/// Streams one blob value through the chunked, CRC-verified extent
/// reader (M4-S17) — `None` on any read or CRC failure (a loss shape
/// the audit reports as never-none).
fn read_blob(disk: &SimDisk, shard: &Path, ext: ExtentRef) -> Option<Vec<u8>> {
    let mut reader =
        open_extent(disk, shard, ExtentId(ext.extent_id), TierIoMode::Buffered).ok()?;
    let len = usize::try_from(ext.len).expect("fits");
    let mut out = Vec::with_capacity(len);
    let mut offset = 0usize;
    while offset < len {
        let take = (len - offset).min(1000);
        match reader.read(offset as u64, take, &mut out) {
            Ok(Ok(())) => offset += take,
            Ok(Err(_)) | Err(_) => return None,
        }
    }
    Some(out)
}

/// The M4-S17 post-recovery refcount oracle (ADR-0061 D6): the
/// reference map equals the model's live blob set exactly — every live
/// blob key maps to its extent at count 1, and no extra reference
/// exists. Then the sweep: every listed-but-unreferenced extent
/// reclaims (orphans and stale alike), never a live one.
fn check_blob_refs(run: &mut Run, life: &mut Life, listed: &[u64], life_index: u64) {
    let mut model_live: Vec<u64> = run.model.values().filter_map(|e| e.extent).collect();
    model_live.sort_unstable();
    let mapped: Vec<u64> = life.table.extent_references().map(|(_, ext, _)| ext).collect();
    let mut mapped_sorted = mapped.clone();
    mapped_sorted.sort_unstable();
    if mapped_sorted != model_live {
        run.report.violations.push(format!(
            "life {life_index}: reference map {mapped_sorted:?} != model {model_live:?}"
        ));
    }
    for ext in &model_live {
        if life.table.extent_refcount(*ext) != 1 {
            run.report.violations.push(format!(
                "life {life_index}: extent {ext} refcount {} != 1",
                life.table.extent_refcount(*ext)
            ));
        }
    }
    let quarantined_before: Vec<u64> =
        inf_log::blob::list_quarantined_extent_ids(&run.disk, &run.shard)
            .expect("quarantine listing")
            .iter()
            .map(|i| i.0)
            .collect();
    let revive = life.table.extent_sweep_seed(listed, &quarantined_before);
    // ADR-0096 D3: the sim stages no upstream accounting omission, so a
    // revival means the sweep's verdict machinery itself desynced.
    if !revive.is_empty() {
        run.report.violations.push(format!(
            "life {life_index}: quarantined extents {revive:?} revived — the sweep \
             quarantined a referenced extent"
        ));
    }
    run.reclaim_blobs(life, &format!("life {life_index} boot sweep"));
    // Post-sweep, the directory is exactly the live set (zero leaks,
    // zero early frees — checked against the disk, not the accounting).
    let on_disk: Vec<u64> =
        list_extent_ids(&run.disk, &run.shard).expect("listing").iter().map(|i| i.0).collect();
    if on_disk != model_live {
        run.report.violations.push(format!(
            "life {life_index}: post-sweep directory {on_disk:?} != live set {model_live:?}"
        ));
    }
    // ADR-0096 D4 bound: every quarantine from a previous life resolved
    // this boot (second-verdict unlink — nothing lingers a second life).
    let quarantined_after: Vec<u64> =
        inf_log::blob::list_quarantined_extent_ids(&run.disk, &run.shard)
            .expect("quarantine listing")
            .iter()
            .map(|i| i.0)
            .collect();
    for id in &quarantined_before {
        if quarantined_after.contains(id) {
            run.report.violations.push(format!(
                "life {life_index}: quarantined extent {id} survived its second verdict"
            ));
        }
    }
    run.report.trace_hash = hash64(&(model_live.len() as u64).to_le_bytes(), run.report.trace_hash);
}

/// The M4-S14 post-recovery consistency oracle: enumerate the recovered
/// index through a pinned walk (every slot below the walk watermark emits
/// as a ref) and bucket the slots by catalogue file. A recovered file's
/// slot count must equal its bucket exactly, and its byte counters obey the
/// sound-direction rule (`dead ≤ len`; restored byte-exact means fully
/// dead). A file a demoting boot sealed (ADR-0174 D5) answers by bytes:
/// byte-exact, its live bytes equal to the lengths of the records its
/// slots name, read from the tier bytes — the dead-byte census. A boot
/// that wrote no tier file walks from the manifested watermark; one that
/// demoted, from at or above it.
fn check_live_set(
    table: &mut TieredTable,
    tier: &TierNsManifest,
    (disk, flush): (&SimDisk, &TierFlush<SimDisk>),
    walk_id: u64,
    report: &mut RecoveryReport,
    life_index: u64,
) {
    let catalogue = flush.sealed();
    let mut counts: BTreeMap<u32, u64> = BTreeMap::new();
    let mut live_bytes: BTreeMap<u32, u64> = BTreeMap::new();
    let w = table.begin_ckpt_walk(walk_id).to_raw();
    let boot_files = catalogue.iter().any(|f| tier.files.iter().all(|m| m.id != f.id));
    if (!boot_files && w != tier.flushed) || w < tier.flushed {
        report.violations.push(format!(
            "life {life_index}: new life walks from {w}, the manifested watermark is {} (boot \
             files: {boot_files})",
            tier.flushed
        ));
    }
    let mut cursor = 0u64;
    loop {
        let mut refs: Vec<u64> = Vec::new();
        cursor = table.ckpt_walk_slice(cursor, 256, |_, addr| refs.push(addr.to_raw()), |_| {});
        for addr in refs {
            let base = |f: &TierFileMeta| f.base.to_raw();
            let Some(file) =
                catalogue.iter().find(|f| addr >= base(f) && addr < base(f) + f.data_len)
            else {
                report
                    .violations
                    .push(format!("life {life_index}: slot {addr} outside every catalogue file"));
                continue;
            };
            *counts.entry(file.id).or_default() += 1;
            if tier.files.iter().all(|m| m.id != file.id) {
                let head = read_cold(disk, flush, addr, TieredTable::RECORD_HEADER_LEN);
                let len = head.map_or(0, |h| TieredTable::record_len_from_header(&h) as u64);
                *live_bytes.entry(file.id).or_default() += len;
            }
        }
        if cursor == 0 {
            break;
        }
    }
    // A ticket's winner is imaged by the walk even below its watermark
    // (ADR-0093 A12), so the slots above never name it. In a namespace
    // that demoted, the rebuild tickets two distinct keys with one hash
    // (ADR-0174 D3) and the winner can lie in a boot file: its bytes are
    // live there.
    let mut winners: Vec<u64> = table.shadow_tickets().map(|t| t.winner.to_raw()).collect();
    winners.sort_unstable();
    winners.dedup();
    for addr in winners {
        let in_file =
            |f: &&TierFileMeta| addr >= f.base.to_raw() && addr < f.base.to_raw() + f.data_len;
        let Some(file) = catalogue.iter().find(in_file) else { continue };
        if addr < w && tier.files.iter().all(|m| m.id != file.id) {
            let head = read_cold(disk, flush, addr, TieredTable::RECORD_HEADER_LEN);
            let len = head.map_or(0, |h| TieredTable::record_len_from_header(&h) as u64);
            *live_bytes.entry(file.id).or_default() += len;
        }
    }
    table.end_ckpt_walk();
    for f in table.live_set().files() {
        check_file(f, &counts, &live_bytes, report, life_index);
    }
}

/// One live-set file against the walk's buckets (see [`check_live_set`]).
fn check_file(
    f: &inf_store::FileLiveSet,
    counts: &BTreeMap<u32, u64>,
    live_bytes: &BTreeMap<u32, u64>,
    report: &mut RecoveryReport,
    life_index: u64,
) {
    let want = counts.get(&f.id).copied().unwrap_or(0);
    if f.dead_bytes > f.data_len {
        report.violations.push(format!(
            "life {life_index}: file {} dead {} exceeds its {} bytes",
            f.id, f.dead_bytes, f.data_len
        ));
    }
    if f.recovered {
        if f.live_count != want {
            report.violations.push(format!(
                "life {life_index}: file {} live count {} but the index holds {want}",
                f.id, f.live_count
            ));
        }
        if f.byte_exact && f.dead_bytes != f.data_len {
            report.violations.push(format!(
                "life {life_index}: file {} restored byte-exact without being fully dead",
                f.id
            ));
        }
    } else {
        let live = live_bytes.get(&f.id).copied().unwrap_or(0);
        if !f.byte_exact || f.data_len - f.dead_bytes.min(f.data_len) != live {
            report.violations.push(format!(
                "life {life_index}: boot file {} (byte-exact {}) has {} live bytes by its \
                 counters, {live} by its slots",
                f.id,
                f.byte_exact,
                f.data_len - f.dead_bytes.min(f.data_len)
            ));
        }
        report.boot_files_censused += 1;
    }
    report.trace_hash = hash64(
        &[u64::from(f.id).to_le_bytes(), f.live_count.to_le_bytes(), f.dead_bytes.to_le_bytes()]
            .concat(),
        report.trace_hash,
    );
}

/// The record bytes boot replay re-appends from a modeled tail: one
/// record per image or extent reference (header + key + value bytes),
/// nothing for a marker or a delete.
fn tail_record_bytes(tail: &[u8]) -> u64 {
    let mut rest = tail;
    let mut bytes = 0u64;
    while !rest.is_empty() {
        let (record, consumed) = decode_record(rest).expect("tail records decode");
        match record {
            RecordView::StringPostImage { key, value, .. } => {
                bytes += (TieredTable::RECORD_HEADER_LEN + key.len() + value.len()) as u64;
            }
            RecordView::StringExtentRef { key, .. } => {
                bytes += (TieredTable::RECORD_HEADER_LEN + key.len() + EXTENT_REF_LEN) as u64;
            }
            RecordView::Delete { .. }
            | RecordView::ColdDisplace { .. }
            | RecordView::ExpireAt { .. }
            | RecordView::NsOp { .. }
            | RecordView::CkptBegin { .. }
            | RecordView::DocDelta { .. }
            | RecordView::DocFull { .. } => {}
        }
        rest = &rest[consumed..];
    }
    bytes
}

/// One op of the replay-above-window fill: a fresh key (one in sixteen a
/// 1-byte key) or, once the fill has keys, a rewrite of a recent one (a
/// distance under a window), a rewrite or a shadow write of an early one
/// (over a window: demoted by now), or a delete. Values from the
/// record-length row: 1 byte, typical, and the longest under `threshold`.
fn fill_op(
    rng: &mut SplitMix64,
    fill: &[Vec<u8>],
    life_index: u64,
    i: u64,
    threshold: u32,
) -> (Vec<u8>, Op) {
    let value_len = match rng.next_u64() % 8 {
        0 => 1,
        1 => threshold as usize - 1,
        _ => 24 + (rng.next_u64() % 140) as usize,
    };
    let value = vec![(rng.next_u64() % 251) as u8; value_len];
    let count = fill.len() as u64;
    let recent =
        |rng: &mut SplitMix64| fill[(count - 1 - rng.next_u64() % count.min(32)) as usize].clone();
    let early = |rng: &mut SplitMix64| fill[(rng.next_u64() % count.div_ceil(4)) as usize].clone();
    match rng.next_u64() % 16 {
        0 | 1 if count > 0 => (early(rng), Op::Del),
        2 | 3 if count > 0 => (recent(rng), Op::Set(value)),
        4 | 5 if count > 64 => (early(rng), Op::SetShadow(value)),
        6 if count > 64 => (early(rng), Op::Set(value)),
        7 => (vec![b'!' + (rng.next_u64() % 64) as u8], Op::Set(vec![0x31])),
        _ => (format!("spill:{life_index:02}:{i:06}").into_bytes(), Op::Set(value)),
    }
}

fn seeded_op(rng: &mut SplitMix64, threshold: u32) -> Op {
    match rng.next_u64() % 8 {
        0 => Op::Del,
        // The blob leg (M4-S17): values at or above the threshold, small
        // enough that the extent lifecycle churns at DST scale.
        1 => {
            let len = threshold as usize + (rng.next_u64() % 300) as usize;
            Op::SetBlob(vec![(rng.next_u64() % 251) as u8; len])
        }
        // The shadow leg (M4.5-S37): a quarter of the inline SETs.
        2 | 3 => {
            let len = 24 + (rng.next_u64() % 140) as usize;
            Op::SetShadow(vec![(rng.next_u64() % 251) as u8; len])
        }
        _ => {
            let len = 24 + (rng.next_u64() % 140) as usize;
            Op::Set(vec![(rng.next_u64() % 251) as u8; len])
        }
    }
}

/// Runs the scenario once. Deterministic from `scenario.seed` (L7).
#[must_use]
pub fn run_recovery_scenario(scenario: &RecoveryScenario) -> RecoveryReport {
    let mut rng = SplitMix64::new(scenario.seed ^ 0x4EC0_7E4Fu64);
    // The table's key hasher (ADR-0094 D2): seed-derived, one value for
    // every life of the run (the checkpoint's refs are its outputs).
    let hasher = KeyHasher::from_seed(scenario.seed ^ 0x4B45_5948);
    let disk = SimDisk::new();
    let shard = PathBuf::from("node/shard-0");
    disk.create_dir_all(&shard).expect("shard dir");
    let spec = Spec::of(scenario.spec_variant);
    let mut run = Run {
        spec,
        disk: disk.clone(),
        shard: shard.clone(),
        model: BTreeMap::new(),
        tail: Vec::new(),
        pending_unlink: Vec::new(),
        held_twin: None,
        held_key: None,
        published_image_bytes: 0,
        walk_image_bytes: 0,
        walk_refs: Vec::new(),
        published_refs: Vec::new(),
        two_crash: Vec::new(),
        report: RecoveryReport { spec_variant: spec.variant, ..RecoveryReport::default() },
    };
    let mut life = Life {
        table: tiered_table(&spec, 0, hasher),
        flush: TierFlush::new(disk.clone(), flush_config(&shard), 0),
        ring: StagingRing::new(StagingConfig::default()),
        flush_lag: false,
    };
    let mut ckpt_id = 0u64;
    // ADR-0093 A7: four crafted colliding pairs per seed — two real keys
    // with one 64-bit hash each, routed by the shared hashtag — and two
    // triples (F-L07-01).
    let pairs = crafted_keys(scenario.seed);

    for life_index in 0..scenario.lives {
        run.report.lives += 1;
        life.flush_lag = life_index > 0 && rng.next_u64().is_multiple_of(3);
        if life.flush_lag {
            run.report.flush_lag_lives += 1;
        }
        // Phase A: mutations (into the tail — everything since the last
        // durable publish replays).
        for op_index in 0..scenario.ops_per_phase {
            if op_index == scenario.ops_per_phase / 2 && !life.flush_lag {
                run.hold_a_ticket(&mut life, &mut rng);
            }
            let key = seeded_key(&mut rng, scenario.keys, &pairs);
            let op = seeded_op(&mut rng, run.spec.blob_threshold);
            run.apply_op(&mut life, &key, op);
            if !life.flush_lag && rng.next_u64().is_multiple_of(32) {
                run.maintain(&mut life);
            }
            // The reconciler's cadence (ADR-0093 D4), seeded: most
            // tickets resolve in-life, some stay open into the walk and
            // the cut.
            if rng.next_u64().is_multiple_of(5) {
                run.reconcile(&mut life, 2, &format!("life {life_index} phase A"));
            }
            // The seeded AC1 cut (M4-S17): occasionally an extent
            // reaches durability and the "process dies" before its
            // referencing record exists — a durable orphan nothing can
            // ever resolve; the boot sweep must reclaim it.
            if rng.next_u64().is_multiple_of(48) {
                let orphan_id = ExtentId(life.table.allocate_extent_id());
                let len = run.spec.blob_threshold as usize + (rng.next_u64() % 64) as usize;
                let mut w = ExtentWriter::create(
                    &run.disk,
                    &run.shard,
                    orphan_id,
                    0,
                    NS,
                    len as u64,
                    TierIoMode::Buffered,
                )
                .expect("orphan create");
                w.append_chunk(&vec![(rng.next_u64() % 251) as u8; len]).expect("orphan bytes");
                let _token_lost = w.finish().expect("orphan fsync");
                run.report.blob_orphans_planted += 1;
            }
        }
        if !life.flush_lag {
            run.maintain(&mut life);
        }
        // Copy-forward burst before the walk (M4-S15): the dead-ratio
        // arm normally; the pressure arm on seeded lives. Relocated
        // bytes flush with the next maintain round.
        let pressure = rng.next_u64().is_multiple_of(5);
        if !life.flush_lag {
            run.compact(&mut life, pressure, 8, &format!("life {life_index} pre-walk"));
            run.maintain(&mut life);
            // Review of 2026-08-30, F-L07-01 / batch 23 (ADR-0093 A11):
            // a relocated cold slot becomes a same-key twin through the
            // shadow path and its winner is deleted with the ticket open
            // — the twin's own origins must ride the `DEL`'s markers, or
            // a checkpoint that began before the relocation resurrects
            // the deleted key at the next boot (the cut-before-publish
            // lives replay exactly that checkpoint).
            run.directed_twin_with_origins(&mut life, &mut rng);
        }

        // The fuzzy hybrid walk, slice-interleaved with mutations. The
        // tail prefix covered by this checkpoint is truncated only if
        // the publish lands (cut-before-publish keeps it — D7).
        // The class's last life carries the two-crash row: it publishes.
        let two_crash_life = scenario.replay_above_window
            && spec.variant.is_none()
            && life_index + 1 == scenario.lives;
        let cut_before_publish =
            life_index > 0 && rng.next_u64().is_multiple_of(4) && !two_crash_life;
        let covered = run.tail.len();
        run.walk_image_bytes = 0;
        run.walk_refs.clear();
        let w = life.table.begin_ckpt_walk(ckpt_id + 1).to_raw();
        let begin_lsn = Lsn::new(SegmentId(u32::try_from(life_index + 1).expect("small")), 64);
        let mut writer = SyncIckWriter::create_v2(
            disk.clone(),
            &shard,
            &CkptConfig::default(),
            0,
            ckpt_id + 1,
            begin_lsn,
            &[NS.0],
        )
        .expect("create ick");
        // Two passes, as the reactor writer walks (ADR-0174 R2: every ref
        // section of a namespace precedes every image section): a full
        // walk for refs, then one for images, each slice-interleaved with
        // the same mutation, maintain, reconcile and compaction rounds —
        // so a cold key overwritten between its ref in pass 0 and pass 1
        // reaching it has a ref, an image and a tail marker (ADR-0057 D4
        // rule 1's shape, now the ordinary one).
        for pass in 0..2u8 {
            let mut cursor = 0u64;
            loop {
                let cold_before = life.table.space().counters().cold_resolves;
                let mut refs: Vec<(u64, u64)> = Vec::new();
                let mut images: Vec<(Vec<u8>, Vec<u8>, Option<ExtentRef>)> = Vec::new();
                cursor = life.table.ckpt_walk_slice(
                    cursor,
                    48,
                    |hash, addr| {
                        if pass == 0 {
                            refs.push((hash, addr.to_raw()));
                        }
                    },
                    |parts| {
                        if pass == 1 {
                            let image = (parts.key.to_vec(), parts.value.to_vec());
                            images.push((image.0, image.1, parts.extent_ref()));
                        }
                    },
                );
                if life.table.space().counters().cold_resolves != cold_before {
                    run.report.violations.push("walker resolved a cold address".into());
                }
                for (hash, addr) in refs {
                    writer.append_ref(NS.0, w, hash, addr).expect("ref");
                    run.report.refs_emitted += 1;
                    run.walk_refs.push((hash, addr));
                }
                for (key, value, ext) in images {
                    let value_len = ext.map_or(value.len(), |_| EXTENT_REF_LEN);
                    run.walk_image_bytes +=
                        (TieredTable::RECORD_HEADER_LEN + key.len() + value_len) as u64;
                    match ext {
                        // M4-S17 (ADR-0061 D2): resident extent records
                        // image as tag-9 — the reference, never the value.
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
                            .append(&RecordView::StringPostImage {
                                ns: NS,
                                key: &key,
                                value: &value,
                            })
                            .expect("image"),
                    }
                    run.report.images_emitted += 1;
                }
                if cursor == 0 {
                    break;
                }
                for _ in 0..4 {
                    let key = seeded_key(&mut rng, scenario.keys, &pairs);
                    let op = seeded_op(&mut rng, run.spec.blob_threshold);
                    run.apply_op(&mut life, &key, op);
                }
                if !life.flush_lag && rng.next_u64().is_multiple_of(4) {
                    run.maintain(&mut life);
                }
                // Mid-walk reconciliation (ADR-0093 D5): a resolution under
                // a pinned walk records the walk's own id as the origin's
                // stamp, so the ref this walk may have emitted for the twin
                // is covered by the origin until the next checkpoint lands.
                if rng.next_u64().is_multiple_of(3) {
                    run.reconcile(&mut life, 1, &format!("life {life_index} mid-walk"));
                }
                // Mid-walk copy-forward attempt (ADR-0059 D9-1): the pin
                // pauses compaction — a mid-walk relocation would let this
                // walk emit a ref and an image for one key. The call
                // exercises the pause path; relocations must not move.
                if !life.flush_lag && rng.next_u64().is_multiple_of(3) {
                    let before = run.report.relocations;
                    run.compact(&mut life, false, 2, &format!("life {life_index} mid-walk"));
                    if run.report.relocations != before {
                        run.report
                            .violations
                            .push(format!("life {life_index}: compaction ran under a pinned walk"));
                    }
                }
            }
        }
        // Live-set emission (M4-S14, ADR-0058 D3): one 0x04 section per
        // namespace, after its record/ref emission — recovered files'
        // lower bounds carry forward, this life's files serialize exact.
        for f in life.table.live_set().files() {
            writer
                .append_live_set(NS.0, f.id, f.data_len, f.dead_bytes, f.byte_exact)
                .expect("live set");
            run.report.live_entries_emitted += 1;
        }
        // Blob-reference emission (M4-S17, ADR-0061 D6): the reference
        // map's cold entries — the identity a released record's death
        // decrements by after recovery.
        for (addr, extent_id, len) in life.table.extent_ckpt_entries().collect::<Vec<_>>() {
            writer.append_blob_ref(NS.0, addr, extent_id, len).expect("blob ref");
        }
        writer.finish().expect("finish ick");
        life.table.end_ckpt_walk();

        if !cut_before_publish {
            ckpt_id += 1;
            // Retirement (M4-S15, ADR-0059 D3): mark before the section
            // builds — the manifest under construction excludes retiring
            // files, and this walk provably emitted no ref into them.
            life.table.retire_scan(ckpt_id, &life.flush);
            let section = life.table.tier_manifest(NS.0, &life.flush);
            if section.flushed < w {
                run.report.violations.push("publication does not cover the walk watermark".into());
            }
            write_manifest(
                &disk,
                &shard,
                &Manifest {
                    ckpt_id,
                    begin_lsn,
                    segments: vec![begin_lsn.segment],
                    tiers: vec![section],
                    key_hash_id: hasher.identity(),
                },
            )
            .expect("manifest swap");
            // The swap landed: retiring files leave the table, detach
            // from the catalog, and unlink — except when the seed drives
            // the swap ↔ unlink crash window, leaving them on disk for
            // the boot GC to prove D6-1 covers it.
            let leave_for_boot_gc = rng.next_u64().is_multiple_of(3);
            for id in life.table.commit_retirement() {
                let Some(meta) = life.flush.detach_sealed(id) else { continue };
                run.report.files_retired += 1;
                if leave_for_boot_gc {
                    run.report.unlinks_left_to_boot_gc += 1;
                } else {
                    run.pending_unlink.push(meta);
                }
            }
            // The pin-analog drain: no reads are in flight between ops
            // in this harness, so queued unlinks execute here.
            for meta in run.pending_unlink.drain(..) {
                unlink_tier_file(&disk, &meta).expect("sim unlink");
                run.report.files_unlinked += 1;
            }
            // WAL truncation (D7): drop exactly the covered prefix.
            run.tail.drain(..covered);
            run.published_image_bytes = run.walk_image_bytes;
            run.published_refs = std::mem::take(&mut run.walk_refs);
            // In-life dangling oracle: every model key still serves with
            // the retired files gone (a slot naming a detached file
            // surfaces here as a read failure, before any crash).
            run.audit(&life, &format!("life {life_index} post-retirement"));
            // Post-publish tail ops.
            for _ in 0..scenario.ops_per_phase / 4 {
                let key = seeded_key(&mut rng, scenario.keys, &pairs);
                let op = seeded_op(&mut rng, run.spec.blob_threshold);
                run.apply_op(&mut life, &key, op);
            }
            if !life.flush_lag {
                run.maintain(&mut life);
            }
        } else {
            run.report.cut_before_publish += 1;
        }
        if scenario.replay_above_window || spec.variant.is_some() {
            // The two-crash life's unit lies just above the window, so its
            // boot demotes and the second boot fits the raised one.
            let fixed = two_crash_life.then_some(spec.window() + 16 * PAGE);
            run.fill_replay_unit(&mut life, &mut rng, life_index, fixed);
        }
        if two_crash_life {
            run.open_two_crash_rows(&mut life);
        }
        // `MEM-BUDGET` lowered to the boot's before the cut (a variant's;
        // the scenario's own spec is its live one): the ring stays, so the
        // next boot recovers at the lowered window over the same ring.
        life.table.set_demotion(spec.boot).expect("the boot's window is inside its ring");

        // Tickets deliberately left open across the cut (ADR-0093 D5):
        // recovery must re-form them from the checkpoint/tail.
        run.report.shadow_open_at_cut += life.table.shadow_pending() as u64;
        // The held ticket (A12) counts only if still open at the cut
        // (a DEL in the mix may have ended it legitimately).
        let held_at_cut =
            run.held_twin.filter(|c| life.table.shadow_tickets().any(|t| t.cold.to_raw() == *c));
        run.held_twin = None;
        // The cut: every un-fsynced byte tears (seeded physics).
        run.report.state.number(b"cut-life", life_index);
        run.report.state.digest(life.table.simulation_digest());
        run.report.state.disk(&disk);
        disk.power_cut(scenario.seed ^ (0xC07_0000 + life_index));
        run.report.state.disk(&disk);
        drop(life);

        // ---- recovery (ADR-0057 D6) ----
        let manifest = match read_manifest(&disk, &shard) {
            Ok(Some(manifest)) => manifest,
            Ok(None) => {
                run.report.violations.push("published manifest lost".into());
                run.report.state_hash = run.report.state.value();
                return run.report;
            }
            Err(e) => {
                run.report.violations.push(format!("manifest unreadable: {e}"));
                run.report.state_hash = run.report.state.value();
                return run.report;
            }
        };
        let Some(tier) = manifest.tier_ns(NS.0).cloned() else {
            run.report.violations.push("manifest lost its tier section".into());
            run.report.state_hash = run.report.state.value();
            return run.report;
        };
        let recovered = match recover_tiered_ns(
            disk.clone(),
            &tier,
            manifest.ckpt_id,
            flush_config(&shard),
            space_config(&spec, 0),
            spec.boot,
            1024,
            hasher,
        ) {
            Ok(recovered) => recovered,
            Err(e) => {
                run.report.violations.push(format!("tier recovery failed: {e}"));
                run.report.state_hash = run.report.state.value();
                return run.report;
            }
        };
        let extents_listed = recovered.extents_listed;
        let replay_unit = run.replay_unit_bytes();
        let mut node = Booting::new(recovered.table, recovered.replay, hasher, &spec);
        let booted = node
            .load_checkpoint(&disk, &shard.join(ick_file_name(manifest.ckpt_id)), tier.flushed)
            .and_then(|()| {
                run.report.state.number(b"checkpoint-loaded", life_index);
                run.report.state.digest(node.table().simulation_digest());
                node.table_mut().set_shadow_enabled(true);
                node.replay_tail(&run.tail)
            })
            .and_then(|()| node.finish(&mut run.report.shadow_settled_at_boot));
        let (mut table, handed, counters) = match booted {
            Ok(done) => done,
            Err(err) => {
                run.report.violations.push(format!("life {life_index}: boot: {err}"));
                run.report.state_hash = run.report.state.value();
                return run.report;
            }
        };
        run.note_boot(&counters, replay_unit, life_index);
        if two_crash_life {
            run.settle_two_crash_rows(&table);
        }
        run.report.state.number(b"recovered-life", life_index);
        run.report.state.digest(table.simulation_digest());
        // The M4-S14 oracle (ADR-0058 D4): by replay-complete, every
        // recovered file's slot count equals the index's ground truth,
        // and byte counters never over-count dead — asserted per life,
        // folded into the determinism trace. The ground-truth walk uses
        // the id the next real checkpoint would (monotone past boot).
        check_live_set(
            &mut table,
            &tier,
            (&disk, &handed.flush),
            manifest.ckpt_id + 1,
            &mut run.report,
            life_index,
        );
        table.set_blob_config(spec.blob());
        // A variant's `MEM-BUDGET` raised back to its ring for the live life.
        table.set_demotion(spec.live).expect("the live window is the ring");
        life = Life {
            table,
            flush: handed.flush,
            ring: StagingRing::new(StagingConfig::default()),
            flush_lag: false,
        };
        // The D5 rebuild: every pair the checkpoint and the tail restored
        // is a ticket again; the never-none audit runs with them open
        // (the winner serves), then they reconcile and the cardinality
        // oracle closes the life.
        run.report.shadow_reformed += life.table.shadow_pending() as u64;
        if let Some(cold) = held_at_cut {
            // ADR-0093 A12: the winner was sealed and flushed below the
            // walk watermark with the ticket open. If the twin came back
            // (its ref is in the manifest recovery used), a ticket must
            // name it — the walk imaged the winner; a twin an older
            // manifest never referenced (cut-before-publish) legitimately
            // comes back as nothing, and the key serves its image.
            let addr = LogicalAddr::from_raw(cold).expect("48-bit");
            let (key, hash) = run.held_key.clone().expect("a held ticket names its key");
            if life.table.shadow_tickets().any(|t| t.cold.to_raw() == cold) {
                run.report.shadow_held_reformed += 1;
            } else if !life.table.contains_pair(hash, addr) {
                run.report.shadow_held_not_restored += 1;
            } else {
                run.report.violations.push(format!(
                    "life {life_index}: HELD TICKET NOT RE-FORMED — the twin at {cold} of key \
                     {:?} was restored with no RAM winner to pair (lookup {:?}, pending {}, len \
                     {} vs model {}): the walk referenced the flushed winner (ADR-0093 A12)",
                    String::from_utf8_lossy(&key),
                    life.table.lookup(&key, hash, &[]),
                    life.table.shadow_pending(),
                    life.table.len(),
                    run.model.len()
                ));
            }
        }
        run.held_key = None;
        // Review of 2026-08-30, F-L07-01 / batch 23 (ADR-0093 A10): every
        // winner the rebuild left carrying several tickets is deleted
        // now, with them open — the plane's `delete_one` rule played
        // against a multi-ticket winner (pre-fix: the store's release
        // assert, a dead cell).
        run.delete_multi_ticket_winners(&mut life);
        run.audit(&life, &format!("life {life_index} (tickets open)"));
        run.audit_len_after_drain(&mut life, &format!("life {life_index} (tickets open)"));
        run.reconcile_all(&mut life, &format!("life {life_index} post-recovery"));
        run.audit_cardinality(&life, &format!("life {life_index}"));
        // The M4-S17 refcount reconciliation oracle + the boot sweep
        // (ADR-0061 D6): exact counts, orphans reclaimed, disk equals
        // the live set. After reconciliation: an open ticket's twin may
        // be a blob record whose extent stays referenced — legitimately
        // live until the twin is verified — while the model already
        // holds the key's inline winner (ADR-0093 D4: the death, and the
        // refcount decrement, happen at the verdict).
        check_blob_refs(&mut run, &mut life, &extents_listed, life_index);
        run.audit(&life, &format!("life {life_index}"));
        run.report.state.number(b"audited-life", life_index);
        run.report.state.digest(life.table.simulation_digest());
        run.report.state.disk(&disk);
        run.report.trace_hash = hash64(
            &[
                run.report.refs_emitted.to_le_bytes(),
                run.report.images_emitted.to_le_bytes(),
                run.report.tail_records.to_le_bytes(),
                run.report.relocations.to_le_bytes(),
                run.report.files_retired.to_le_bytes(),
                run.report.files_unlinked.to_le_bytes(),
                run.report.blobs_written.to_le_bytes(),
                run.report.blob_extents_reclaimed.to_le_bytes(),
                life.table.cold_floor().to_le_bytes(),
                run.report.shadow_opened.to_le_bytes(),
                run.report.shadow_reformed.to_le_bytes(),
                run.report.shadow_same_key.to_le_bytes(),
                run.report.shadow_collision.to_le_bytes(),
                run.report.shadow_settled_at_boot.to_le_bytes(),
                run.report.shadow_collide_ops.to_le_bytes(),
                run.report.shadow_multi_ticket_dels.to_le_bytes(),
                run.report.shadow_twin_origin_rows.to_le_bytes(),
                run.report.shadow_twin_origins_covered.to_le_bytes(),
                run.report.shadow_held_reformed.to_le_bytes(),
            ]
            .concat(),
            run.report.trace_hash,
        );
    }
    if scenario.replay_above_window && spec.variant.is_none() {
        run.two_crash_coda(life, hasher, scenario.seed);
    }
    // The class's engagement, per seed: a life above the window, a boot
    // that demoted, settle reads (the seal's, the end settle's and the
    // deletes', one D6 counter) and — once a unit reached three windows,
    // where the fill's deletes of its early keys meet demoted copies —
    // verified deletes. The two-crash row asserts its own above; blob
    // releases (R9) are not reached by this generator, and their
    // store-tier row is the evidence.
    let replay = run.report.boot_replay;
    let deletes_owed = run.report.replay_unit_windows_max >= 3;
    if scenario.replay_above_window
        && (run.report.replay_above_window_lives == 0
            || run.report.demoting_boots == 0
            || replay.settle_reads == 0
            || (deletes_owed && replay.deletes_verified == 0))
    {
        run.report.violations.push(format!(
            "REPLAY-ABOVE-WINDOW VACUOUS: {} lives above the window (largest {} windows), {} \
             boots demoted, {} settle reads, {} deletes verified",
            run.report.replay_above_window_lives,
            run.report.replay_unit_windows_max,
            run.report.demoting_boots,
            replay.settle_reads,
            replay.deletes_verified
        ));
    }
    // The spec-variant class's engagement, per seed (ADR-0174 D2 rule 6):
    // a life above the boot's window, a boot that demoted, and a pad
    // placed — the case the class exists to reach.
    if let Some(variant) = spec.variant
        && (run.report.replay_above_window_lives == 0
            || replay.demote_steps == 0
            || replay.pads_placed == 0)
    {
        run.report.violations.push(format!(
            "SPEC-VARIANT VACUOUS ({variant:?}): {} lives above the {}-byte window, {} demote \
             steps, {} pads placed, {} long records written ({} written short)",
            run.report.replay_above_window_lives,
            spec.window(),
            replay.demote_steps,
            replay.pads_placed,
            run.report.long_records_written,
            run.report.long_records_shortened
        ));
    }
    run.report.state_hash = run.report.state.value();
    run.report
}
