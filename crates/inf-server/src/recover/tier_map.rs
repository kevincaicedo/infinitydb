//! The start step's tier map (ADR-0174 D4, A1; ADR-0202): before the
//! checkpoint loads, every tombstoned namespace's leftover `cold/` is swept
//! (ADR-0100 D6) and every tiered namespace of the catalog is mapped — its
//! manifested files probed, an unsealed one resealed, its handles held and
//! its `cold/` listed — then built into the table and the boot pipeline
//! replay demotes through.
//!
//! Each runs under the step budget, in units at fixed prices: a boot file
//! operation (an open, a truncate, a removal, a listing's open and each
//! batch it reads), a tier block read (a probe's two, a reseal's header and
//! each frame it verifies), a barrier (a reseal's seal, a sweep's directory
//! sync) with the tier bytes it covers, and a RAM unit (a listed name
//! handled, a catalog file built). A step yields once its charge reaches
//! the budget, at the next unit, and the next step resumes at the walk's
//! cursor: an index into a list nothing appends to once the walk began,
//! the held listing — the filesystem keeps its position in the open
//! directory, valid across steps because while a cell recovers its recovery
//! is the directory's only writer and removes only names the stream already
//! returned (ADR-0202 D3) — and a reseal's held handle at its next frame. A
//! listed name removed twice is that name's own earlier removal, never a
//! refusal.

use std::io;

use inf_log::fs::{BatchEnd, DirStream, NameBatch, OpenedDir, Removal, SegmentFs, remove_listed};
use inf_log::manifest::TierNsManifest;
use inf_store::limits::{RECOVER_PROBE_CHARGE_BYTES, SETTLE_READ_CHARGE_BYTES};
use inf_store::{Keyspace, MapStep, MapWork, NsTierMap, TierSpec};

use super::hand_over::ResumeFrom;
use super::{
    Phase, RecoveringTierNs, Recovery, RecoveryProgress, ReplayReader, StepBudget, io_msg,
};
use crate::limits::{
    BOOT_FILE_OP_CHARGE_BYTES, REPLAY_BARRIER_CHARGE_BYTES, REPLAY_TIER_BYTE_CHARGE,
};

/// What follows the tier map's last unit: the start step's branch.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(super) enum MapThen {
    /// A MANIFEST: the checkpoint's open and its presize, then its load.
    Ckpt,
    /// A fresh cell: the hand-over of its machines, then its first segment.
    FreshCell,
    /// No MANIFEST: the whole retained log replays.
    Replay,
}

/// The tier map's walk over the namespaces, in order: every tombstoned
/// namespace's sweep, then every tiered namespace's map.
pub(super) struct TierMapWalk<F: SegmentFs> {
    /// The tombstoned namespaces to sweep, in sweep order.
    dropped: Vec<inf_log::NsId>,
    /// The catalog's tiered namespaces, each with its spec and the index of
    /// its section in the MANIFEST (`None`: the empty section).
    tiered: Vec<(inf_log::NsId, TierSpec, Option<usize>)>,
    stage: MapStage<F>,
    then: MapThen,
}

/// Where the walk stands.
enum MapStage<F: SegmentFs> {
    /// The sweep of `dropped[at]`.
    Dropped { at: usize, sweep: DropSweep<F> },
    /// `tiered[at]`'s map, not begun.
    Begin { at: usize },
    /// The map of `tiered[at]`.
    Live { at: usize, map: Box<NsTierMap<F>> },
}

/// A tombstoned namespace's sweep (ADR-0100 D6: the teardown an acked
/// `DROP` owed): its `cold/` listed a batch at a time, every listed name
/// removed, its directory synced once if a name was removed.
enum DropSweep<F: SegmentFs> {
    /// The listing, not opened yet.
    Open,
    /// The held listing, at `next` in its batch; `removed` counts the
    /// sweep's `Removed`.
    Listing {
        stream: F::DirStream,
        batch: Box<NameBatch>,
        next: usize,
        end: BatchEnd,
        removed: u64,
    },
}

impl<F: SegmentFs> TierMapWalk<F> {
    /// Whether nothing is to sweep or map: the start step then goes on in
    /// the same step, as a cell with no tiered namespace always did.
    pub(super) fn is_empty(&self) -> bool {
        self.dropped.is_empty() && self.tiered.is_empty()
    }

    /// What follows the walk.
    pub(super) fn then(&self) -> MapThen {
        self.then
    }

    /// Whether the walk stands between two namespaces.
    fn between_namespaces(&self) -> bool {
        matches!(
            self.stage,
            MapStage::Begin { .. } | MapStage::Dropped { sweep: DropSweep::Open, .. }
        )
    }
}

/// Charges one unit's work at the `limits` prices (ADR-0174 A1.2), the
/// prices the step judge reads the filesystem at. The planted canary of the
/// hand-over's prices charges nothing here either.
fn charge(budget: &mut StepBudget, work: MapWork) {
    let bytes = work
        .file_ops
        .saturating_mul(BOOT_FILE_OP_CHARGE_BYTES)
        .saturating_add(work.tier_reads.saturating_mul(SETTLE_READ_CHARGE_BYTES))
        .saturating_add(work.tier_bytes.saturating_mul(REPLAY_TIER_BYTE_CHARGE))
        .saturating_add(work.barriers.saturating_mul(REPLAY_BARRIER_CHARGE_BYTES))
        .saturating_add(work.ram_units.saturating_mul(RECOVER_PROBE_CHARGE_BYTES));
    budget.charge(bytes);
}

impl<F: SegmentFs + Clone> Recovery<F> {
    /// The tier map's walk over `ks`'s catalog (ADR-0174 D4; ADR-0100 D6):
    /// over a MANIFEST's tier sections a tombstoned id's residue is swept
    /// and an unknown id's is corruption; then every tombstoned namespace no
    /// section names is swept too — with or without a MANIFEST, as D4
    /// recovers a catalogued namespace with or without one (a `DROP` cut
    /// mid-teardown before the first checkpoint publishes leaves it); then
    /// every tiered namespace maps, through its manifest section or the
    /// empty one: `flushed` 0, no file, its dead-life tier files removed
    /// before any flush. Bound: the sections, the tombstones (at most the
    /// catalog's ceiling, 4,096) and the catalog's namespaces, once each.
    ///
    /// # Errors
    /// `InvalidData` for a section no catalog entry or tombstone explains.
    pub(super) fn tier_map_walk(&self, ks: &Keyspace, then: MapThen) -> io::Result<TierMapWalk<F>> {
        let sections = self.manifest.as_ref().map_or(&[][..], |m| m.tiers.as_slice());
        let mut dropped: Vec<inf_log::NsId> = Vec::new();
        for tier in sections {
            let ns = inf_log::NsId(tier.ns);
            if ks.ns_get_by_id(ns).and_then(|spec| spec.tier).is_some() {
                continue;
            }
            if !ks.ns_tombstoned(ns) {
                return Err(io_msg(format!(
                    "MANIFEST carries a tier section for ns {} the catalog does not know and no \
                     drop tombstone explains (corruption or a foreign META — ADR-0100 D6)",
                    tier.ns
                )));
            }
            dropped.push(ns);
        }
        let named = dropped.len();
        for &ns in ks.ns_tombstones() {
            if !dropped[..named].contains(&ns) {
                dropped.push(ns);
            }
        }
        // The sections strictly ascend by namespace (the decoder's rule).
        let tiered = ks
            .ns_iter()
            .filter_map(|spec| spec.tier.map(|tier| (spec.id, tier)))
            .map(|(ns, tier)| (ns, tier, sections.binary_search_by_key(&ns.0, |s| s.ns).ok()))
            .collect();
        let stage = if dropped.is_empty() {
            MapStage::Begin { at: 0 }
        } else {
            MapStage::Dropped { at: 0, sweep: DropSweep::Open }
        };
        Ok(TierMapWalk { dropped, tiered, stage, then })
    }

    /// One tier-map step: units of the walk under `budget`, until the
    /// budget is spent or every namespace is swept and mapped — then the
    /// start step's branch goes on.
    pub(super) fn step_tier_map(
        &mut self,
        ks: &mut Keyspace,
        mut walk: Box<TierMapWalk<F>>,
        budget: &mut StepBudget,
    ) -> io::Result<RecoveryProgress> {
        // Bound: each turn runs one unit (a reseal's frame reads at most
        // what the budget left buys, at least one), or moves to the next
        // namespace; a step with work to do completes a unit at any budget.
        loop {
            // The planted canary asks only between namespaces: a namespace's
            // files and its whole listing, every batch, run in one step.
            let ask = !cfg!(inf_canary_recover_map_whole) || walk.between_namespaces();
            if ask && budget.spent() {
                self.phase = Phase::TierMap { walk };
                return Ok(RecoveryProgress::Working);
            }
            let stage = std::mem::replace(&mut walk.stage, MapStage::Begin { at: usize::MAX });
            walk.stage = match stage {
                MapStage::Dropped { at, sweep } => self.sweep_unit(&walk, at, sweep, budget)?,
                MapStage::Begin { at } => {
                    let Some(&(ns, spec, section)) = walk.tiered.get(at) else {
                        self.phase = self.finish_tier_map(ks, walk.then)?;
                        return Ok(RecoveryProgress::Working);
                    };
                    let map = self.new_tier_map(ks, ns, &spec, section)?;
                    MapStage::Live { at, map: Box::new(map) }
                }
                MapStage::Live { at, map } => self.map_unit(ks, &walk, at, map, budget)?,
            };
        }
    }

    /// One unit of `dropped[at]`'s sweep: its listing opened, a batch read,
    /// a listed name removed, or — its listing at its end — its directory
    /// synced, one barrier, if a name was removed, and the sweep counted.
    /// An absent directory is nothing to do; a cut mid-sweep repeats it on
    /// the next boot, because the tombstone outlives the residue.
    fn sweep_unit(
        &mut self,
        walk: &TierMapWalk<F>,
        at: usize,
        sweep: DropSweep<F>,
        budget: &mut StepBudget,
    ) -> io::Result<MapStage<F>> {
        let ns = *walk.dropped.get(at).ok_or_else(|| io_msg("a sweep past its list".into()))?;
        let cold = self.shard_dir.join(format!("ns-{}", ns.0)).join("cold");
        let fs = self.fs.as_ref().ok_or_else(|| io_msg("a sweep with no filesystem".into()))?;
        let mut work = MapWork::default();
        let next = match sweep {
            DropSweep::Open => {
                work.file_ops += 1;
                match fs.open_listing(&cold)? {
                    OpenedDir::Absent => next_dropped(walk, at),
                    OpenedDir::Open(stream) => {
                        let batch = Box::new(NameBatch::new());
                        let end = BatchEnd::More;
                        let sweep = DropSweep::Listing { stream, batch, next: 0, end, removed: 0 };
                        MapStage::Dropped { at, sweep }
                    }
                }
            }
            DropSweep::Listing { mut stream, mut batch, next, end, mut removed } => {
                if let Some(name) = batch.name(next) {
                    work.file_ops += 1;
                    work.ram_units += 1;
                    let removal = remove_listed(fs, &cold.join(name))?;
                    // The planted canary counts a `Gone` as a removal of its own.
                    if removal == Removal::Removed || cfg!(inf_canary_recover_sweep_gone_counted) {
                        removed += 1;
                    }
                    let sweep = DropSweep::Listing { stream, batch, next: next + 1, end, removed };
                    MapStage::Dropped { at, sweep }
                } else if end == BatchEnd::More {
                    work.file_ops += 1;
                    let end = stream.next_batch(&mut batch)?;
                    MapStage::Dropped {
                        at,
                        sweep: DropSweep::Listing { stream, batch, next: 0, end, removed },
                    }
                } else {
                    drop(stream);
                    if removed > 0 {
                        work.barriers += 1;
                        fs.sync_dir(&cold)?;
                    }
                    self.stats.dropped_ns_swept += 1;
                    self.stats.dropped_files_swept += removed;
                    eprintln!(
                        "cell {}: swept dropped namespace {} residue \
                         ({removed} files) — ADR-0100 D6",
                        self.cell, ns.0
                    );
                    next_dropped(walk, at)
                }
            }
        };
        charge(budget, work);
        Ok(next)
    }

    /// The map of tiered namespace `ns` (ADR-0174 D4): its flush config, its
    /// ring and its manifest section `section` (the empty one for `None`).
    ///
    /// # Errors
    /// The spec's ring reservation unrepresentable.
    fn new_tier_map(
        &self,
        ks: &Keyspace,
        ns: inf_log::NsId,
        spec: &TierSpec,
        section: Option<usize>,
    ) -> io::Result<NsTierMap<F>> {
        let demote = spec.demotion_config();
        let reserve_bytes = demote
            .ring_reserve_bytes()
            .ok_or_else(|| io_msg("tier spec ring reservation unrepresentable".into()))?;
        let empty = TierNsManifest { ns: ns.0, flushed: 0, files: Vec::new() };
        let tier = self.section(section).unwrap_or(&empty);
        Ok(NsTierMap::new(
            self.fs().clone(),
            tier,
            self.manifest.as_ref().map_or(0, |m| m.ckpt_id),
            inf_log::TierFlushConfig {
                shard_dir: self.shard_dir.join(format!("ns-{}", ns.0)),
                cell: u32::from(self.cell),
                ns,
                mode: spec.tier_io_mode,
                file_capacity: inf_log::flush::TIER_FILE_CAPACITY_DEFAULT,
                slice_bytes: spec.maintain_slice_bytes,
                record_len_max: spec.record_len_max(),
            },
            inf_store::AddressSpaceConfig {
                reserve_bytes,
                page_bytes: inf_alloc::REGION_PAGE_BYTES,
                life_origin: inf_store::LogicalAddr::ZERO, // the section's, at the build
            },
            demote,
            1024,
            ks.hasher(),
            &self.census,
        ))
    }

    /// The MANIFEST's section at `index`.
    fn section(&self, index: Option<usize>) -> Option<&TierNsManifest> {
        index.and_then(|i| self.manifest.as_ref().and_then(|m| m.tiers.get(i)))
    }

    /// One unit of `tiered[at]`'s map, or — its last unit — its build: the
    /// recovered table installed in the keyspace, the boot machine kept for
    /// replay, holding the manifested files' creation-mode handles the
    /// hand-over at the end of replay returns (ADR-0054 D1).
    fn map_unit(
        &mut self,
        ks: &mut Keyspace,
        walk: &TierMapWalk<F>,
        at: usize,
        mut map: Box<NsTierMap<F>>,
        budget: &mut StepBudget,
    ) -> io::Result<MapStage<F>> {
        let &(ns, _, section) =
            walk.tiered.get(at).ok_or_else(|| io_msg("a map past its list".into()))?;
        // The planted canary verifies a reseal's every frame in one unit.
        let frames = if cfg!(inf_canary_recover_reseal_whole) {
            u64::MAX
        } else if map.resealing() {
            (budget.left() / SETTLE_READ_CHARGE_BYTES).max(1)
        } else {
            1
        };
        let empty = TierNsManifest { ns: ns.0, flushed: 0, files: Vec::new() };
        let tier = self.section(section).unwrap_or(&empty);
        let ended = match map.step(tier, frames)? {
            MapStep::Ran(work) => {
                charge(budget, work);
                return Ok(MapStage::Live { at, map });
            }
            MapStep::Mapped(ended) => ended,
        };
        let (recovered, work) = map.build(tier, ended)?;
        self.stats.units.tier_map_built =
            self.stats.units.tier_map_built.saturating_add(work.ram_units);
        charge(budget, work);
        ks.install_recovered_tiered(ns, recovered.table);
        let removed = &mut self.stats.tier_replay.dead_life_files_removed;
        *removed = removed.saturating_add(u64::from(recovered.stats.files_removed));
        self.recovering_tiers.tiers.push(RecoveringTierNs {
            ns,
            replay: recovered.replay,
            extents_listed: recovered.extents_listed,
            extents_quarantined: recovered.extents_quarantined,
            revive: inf_store::IdChunks::new(),
            lent: false,
        });
        Ok(MapStage::Begin { at: at + 1 })
    }

    /// The walk has run every unit: the start step's branch goes on — the
    /// checkpoint's open, a fresh cell's hand-over, or the log's replay.
    pub(super) fn finish_tier_map(
        &mut self,
        ks: &mut Keyspace,
        then: MapThen,
    ) -> io::Result<Phase<F>> {
        // The seam finds a record's machine by binary search.
        self.recovering_tiers.tiers.sort_unstable_by_key(|tier| tier.ns);
        Ok(match then {
            MapThen::Ckpt => self.open_checkpoint(ks)?,
            MapThen::FreshCell => self.begin_hand_over(ResumeFrom::FreshCell),
            MapThen::Replay => Phase::Replay { idx: 0, reader: ReplayReader::Unopened },
        })
    }
}

/// The stage after `dropped[at]`'s sweep: the next sweep, or the first map.
fn next_dropped<F: SegmentFs>(walk: &TierMapWalk<F>, at: usize) -> MapStage<F> {
    if at + 1 < walk.dropped.len() {
        MapStage::Dropped { at: at + 1, sweep: DropSweep::Open }
    } else {
        MapStage::Begin { at: 0 }
    }
}
