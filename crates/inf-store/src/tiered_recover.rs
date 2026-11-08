//! Tiered-namespace boot recovery (M4-S12, ADR-0057 D6): consume one
//! MANIFEST v2 tier section and hand back a serving-ready pair — the
//! table (new life at the manifested flushed watermark) and the flush
//! pipeline (catalog seeded, next file id reserved). The composition:
//!
//! 1. **Map tier files** ([`NsTierMap`], a unit at a time). Each sealed
//!    file verifies header identity + footer only (two block reads — frame
//!    CRCs verify lazily on read; an eager pass would read the whole cold
//!    tier at boot). The at-most-one unsealed file is resealed at its
//!    manifested length in three kinds of unit ([`TierReseal`]): its open
//!    (the header check and the truncate), its frame reads (each
//!    CRC-checked) and its seal (`Recovered`, one barrier) — ADR-0056 D5,
//!    idempotent across recovery crashes. Then `cold/` is listed a batch at
//!    a time: files on disk the manifest does not name are dead-life
//!    garbage, deleted before any new flush; extent names are collected.
//! 2. **Seed the new life** at `life_origin = flushed` — gapless and
//!    collision-free because no durable artifact names an address in
//!    `[flushed, old tail)`: only sub-`flushed` bytes flush (§3.1), refs
//!    sit below their walk watermark ≤ `flushed`, and image records
//!    carry no addresses at all.
//! 3. The caller then loads the checkpoint (images re-append at the new
//!    tail; ref sections apply through [`apply_ref_section`] with the
//!    manifested-watermark cross-check) and replays the WAL tail through
//!    the replay machine ([`TierReplay`]) and the table's `replay_*`
//!    entries (ADR-0174 D3): a record the window cannot hold demotes
//!    through the boot pipeline instead of failing the boot (D1), and a
//!    record sealed by that step is settled against its cold twins by
//!    an identity-checked read through a held handle (R7).
//!
//! Fail-stop philosophy: the manifest is the only authority — a named
//! file that is missing, mis-identified, or shorter than its manifested
//! range is `InvalidData`, never a silent skip (§8.4, ADR-0017 D5).

use std::ffi::OsStr;
use std::io;
use std::path::PathBuf;
use std::rc::Rc;

use inf_foundation::bounded::CapCensus;
use inf_foundation::{KeyHasher, LogicalAddr};
use inf_log::blob::{parse_extent_file_name, parse_quarantined_file_name};
use inf_log::ckpt::{IckBlobRefSection, IckLiveSetSection, IckRefSection};
use inf_log::flush::{BootFlush, TierFileMeta, TierFlush, TierFlushConfig};
use inf_log::fs::{BatchEnd, DirStream, NameBatch, OpenedDir, SegmentFs};
use inf_log::manifest::{TierFileRange, TierNsManifest};
use inf_log::tier::{
    SealReason, TIER_FOOTER_BYTES, TierIdentity, TierReseal, VerifiedTier, Verify,
    parse_tier_file_name, probe_tier_file, tier_file_name,
};

use crate::address_space::AddressSpaceConfig;
use crate::demote::DemotionConfig;
use crate::limits::ID_CHUNK_IDS;
use crate::tiered::TieredTable;
use crate::tiered::replay::TierReplay;

/// A boot list of `u64` ids in fixed chunks of [`ID_CHUNK_IDS`] (ADR-0174
/// A1): a push fills the last chunk or starts a new one, so no push copies
/// the list — an O(n) copy inside one unit, at a doubling, would be a unit
/// no budget bounds. Read by index: an index into a list nothing appends to
/// any more names one id across recovery steps.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct IdChunks {
    /// Every chunk but the last is full.
    chunks: Vec<Vec<u64>>,
    len: usize,
}

impl IdChunks {
    /// An empty list.
    #[must_use]
    pub fn new() -> IdChunks {
        IdChunks::default()
    }

    /// Appends `id`.
    pub fn push(&mut self, id: u64) {
        match self.chunks.last_mut() {
            Some(chunk) if chunk.len() < ID_CHUNK_IDS => chunk.push(id),
            Some(_) | None => {
                let mut chunk = Vec::with_capacity(ID_CHUNK_IDS);
                chunk.push(id);
                self.chunks.push(chunk);
            }
        }
        self.len += 1;
    }

    /// The ids listed.
    #[must_use]
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether no id is listed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The ids from index `at` to the end of its chunk, in list order:
    /// empty at or past `len()`, non-empty below it.
    #[must_use]
    pub fn run_from(&self, at: usize) -> &[u64] {
        let (chunk, offset) = (at / ID_CHUNK_IDS, at % ID_CHUNK_IDS);
        self.chunks.get(chunk).and_then(|ids| ids.get(offset..)).unwrap_or_default()
    }

    /// The ids from index `at` on, in list order.
    pub fn iter_from(&self, at: usize) -> impl Iterator<Item = u64> + '_ {
        let (chunk, offset) = (at / ID_CHUNK_IDS, at % ID_CHUNK_IDS);
        self.chunks.iter().skip(chunk).enumerate().flat_map(move |(i, ids)| {
            let from = if i == 0 { offset } else { 0 };
            ids.get(from..).unwrap_or_default().iter().copied()
        })
    }

    /// The ids in list order.
    pub fn iter(&self) -> impl Iterator<Item = u64> + '_ {
        self.iter_from(0)
    }

    /// Whether `id` is listed: a scan, for tests and oracles.
    #[must_use]
    pub fn contains(&self, id: &u64) -> bool {
        self.iter().any(|listed| listed == *id)
    }

    /// The ids as one vector, for tests and oracles: a copy.
    #[must_use]
    pub fn to_vec(&self) -> Vec<u64> {
        self.iter().collect()
    }
}

impl FromIterator<u64> for IdChunks {
    fn from_iter<I: IntoIterator<Item = u64>>(ids: I) -> IdChunks {
        let mut list = IdChunks::new();
        // Bound: the iterator's ids, one push each.
        for id in ids {
            list.push(id);
        }
        list
    }
}

/// Boot facts for the log line and the ledger (counts, not policy).
#[derive(Copy, Clone, Default, Debug, PartialEq, Eq)]
pub struct TierRecoverStats {
    /// Manifested files accepted on the sealed fast path.
    pub files_sealed: u32,
    /// Unsealed (or torn-seal) files re-sealed `Recovered` at their
    /// manifested length.
    pub files_resealed: u32,
    /// Un-manifested dead-life files deleted from the cold dir.
    pub files_removed: u32,
}

/// One recovered tiered namespace: the new-life table, its boot replay
/// machine over the seeded flush pipeline, and the boot facts.
pub struct RecoveredTier<F: SegmentFs> {
    /// New life at `life_origin = manifested flushed`; watermarks all at
    /// the origin (RAM starts cold — disclosed, L10). Live/dead counters
    /// boot *unreconciled* for the cold set (S14's lazy rebuild owns
    /// them).
    pub table: TieredTable,
    /// The replay machine (ADR-0174), `Seeded`:
    /// the boot pipeline — catalog seeded with the manifested files,
    /// `next_id` above every named id, the creation-mode handle of every
    /// manifested file held, the barrier claim rule — and the counters.
    /// Its `hand_over` is the plane's pipeline and the handles.
    pub replay: TierReplay<F>,
    /// Blob-extent ids present on disk, in the listing's order (names only
    /// — no content reads; M4-S17, ADR-0061 D6): no consumer reads an
    /// order, so none is sorted (on a real filesystem the order is its own,
    /// hash order on an ext4 htree). The caller hands this to
    /// [`TieredTable::extent_sweep_seed`] **after** checkpoint + tail
    /// replay complete: liveness is decided by the post-replay
    /// refcounts, never by the manifest — the tier-file "unmanifested ⇒
    /// garbage" rule deliberately does not extend to extents.
    pub extents_listed: IdChunks,
    /// `.quarantine`-named extent ids (ADR-0096 D3), in the listing's order
    /// — boot orphans a previous life renamed instead of unlinking. Handed
    /// to the same sweep seed; ids the replayed map references come back
    /// for revival, the rest await their second-verdict unlink.
    pub extents_quarantined: IdChunks,
    /// Boot facts.
    pub stats: TierRecoverStats,
}

/// What one unit of a namespace's tier map did, in the counts its prices
/// read (ADR-0174 A1): the recovery driver charges each at its `limits`
/// price, so a step's charge is the units it ran.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct MapWork {
    /// Boot file operations: an open, a truncate, a removal, a listing's
    /// open and each batch it reads.
    pub file_ops: u64,
    /// Tier block reads: a probe's header and footer, a reseal's header
    /// and each frame it verifies.
    pub tier_reads: u64,
    /// Tier bytes written: a reseal's footer.
    pub tier_bytes: u64,
    /// Barriers: a reseal's seal.
    pub barriers: u64,
    /// RAM units: a listed name handled, a catalog file built.
    pub ram_units: u64,
}

/// What one [`NsTierMap::step`] answered.
#[must_use]
pub enum MapStep {
    /// One unit ran.
    Ran(MapWork),
    /// Every unit but the build has run, the listing at its end: the
    /// witness [`NsTierMap::build`] takes.
    Mapped(MapEnded),
}

/// The witness that a tier map's listing reached its end: made only by
/// [`NsTierMap::step`]'s `Mapped` answer, taken by
/// [`NsTierMap::build`], so a build before the listing ended does not
/// compile.
#[must_use]
pub struct MapEnded(());

/// Where a namespace's tier map stands: its units, in their one order.
enum MapStage<F: SegmentFs> {
    /// The probe of manifested file `at` (its header and footer).
    Probe { at: usize },
    /// File `at` probed unsealed: its reseal's open.
    ResealOpen { at: usize },
    /// File `at`'s reseal, verifying its frames.
    Reseal { at: usize, reseal: TierReseal<F> },
    /// File `at`'s every frame verified: its seal.
    Seal { at: usize, verified: VerifiedTier<F> },
    /// File `at` checked, sealed for `reason`: its held handle.
    Handle { at: usize, reason: SealReason },
    /// The `cold/` listing, not opened yet.
    ListOpen,
    /// The listing: the held stream, its batch and the next name in it;
    /// `end` is how the batch's call ended.
    Listing { stream: F::DirStream, batch: Box<NameBatch>, next: usize, end: BatchEnd },
    /// Every unit but the build has run.
    Mapped,
    /// A unit failed: the boot refuses, and the map is never stepped again.
    Refused,
}

/// One tiered namespace's tier map (ADR-0174 D4, A1; ADR-0202): the one
/// home of what the map gathers — the catalog, the held handles, the two
/// id lists and the boot facts — and of the order of its units: each
/// manifested file probed, resealed if unsealed (its open, its frame reads,
/// its seal) and its handle opened, then `cold/` listed a batch at a time,
/// one name a unit, then the build. Both drivers step this one machine:
/// [`recover_tiered_ns`] whole, and the recovery driver of the server under
/// its step budget. Between two steps nothing but this map writes the
/// namespace's `cold/`, and the section it maps is the caller's immutable
/// manifest, so an index into its files names one file.
pub struct NsTierMap<F: SegmentFs> {
    fs: F,
    boot_ckpt_id: u64,
    flush_config: TierFlushConfig,
    space: AddressSpaceConfig,
    demote: DemotionConfig,
    initial_keys: usize,
    hasher: KeyHasher,
    census: Rc<CapCensus>,
    cold_dir: PathBuf,
    catalog: Vec<TierFileMeta>,
    handles: Vec<(u32, F::File)>,
    extents_listed: IdChunks,
    extents_quarantined: IdChunks,
    stats: TierRecoverStats,
    stage: MapStage<F>,
}

impl<F: SegmentFs> NsTierMap<F> {
    /// A map of `tier`'s namespace, at its first unit. The flush config's
    /// `{shard_dir, cell, ns, mode}` name the namespace; `space`'s
    /// `life_origin` is overridden with the manifested watermark at the
    /// build; `boot_ckpt_id` is the manifested checkpoint's id (the
    /// recovered files' initial unref stamp — M4-S15, ADR-0059 D3).
    // the recovery entry's seven inputs, the key hasher (ADR-0094) and the
    // cell's cap census, which the recovered pipeline's holders register on
    #[allow(clippy::too_many_arguments)]
    #[must_use]
    pub fn new(
        fs: F,
        tier: &TierNsManifest,
        boot_ckpt_id: u64,
        flush_config: TierFlushConfig,
        space: AddressSpaceConfig,
        demote: DemotionConfig,
        initial_keys: usize,
        hasher: KeyHasher,
        census: &Rc<CapCensus>,
    ) -> NsTierMap<F> {
        assert_eq!(flush_config.ns.0, tier.ns, "manifest section vs pipeline namespace");
        // The directory is not created here: a namespace that never flushed
        // has none, the pipeline creates it with its first tier file, and a
        // boot that fits writes nothing for the namespace (ADR-0174 D5) — a
        // full device refuses a mkdir. A section that names files needs it,
        // and their probes refuse typed when it is missing.
        let cold_dir = flush_config.shard_dir.join("cold");
        NsTierMap {
            fs,
            boot_ckpt_id,
            flush_config,
            space,
            demote,
            initial_keys,
            hasher,
            census: Rc::clone(census),
            cold_dir,
            catalog: Vec::with_capacity(tier.files.len()),
            handles: Vec::with_capacity(tier.files.len()),
            extents_listed: IdChunks::new(),
            extents_quarantined: IdChunks::new(),
            stats: TierRecoverStats::default(),
            stage: MapStage::Probe { at: 0 },
        }
    }

    /// The map's namespace.
    #[must_use]
    pub fn ns(&self) -> inf_log::NsId {
        self.flush_config.ns
    }

    /// Whether the next unit is a batch of a reseal's frame reads, which
    /// [`step`](Self::step) sizes by its `frames_max`.
    #[must_use]
    pub fn resealing(&self) -> bool {
        matches!(self.stage, MapStage::Reseal { .. })
    }

    /// The boot facts so far.
    #[must_use]
    pub fn stats(&self) -> TierRecoverStats {
        self.stats
    }

    /// Runs the next unit over `tier` — the same section every call — and
    /// answers what it did; a reseal's frame reads verify at most
    /// `frames_max` frames, at least one. `Mapped` once only the build is
    /// left.
    ///
    /// # Errors
    /// I/O failures; `InvalidData` when a named file is missing, its header
    /// identity mismatches, a sealed footer covers less than the manifested
    /// range or a retained frame fails its CRC. After an `Err` every call
    /// refuses.
    pub fn step(&mut self, tier: &TierNsManifest, frames_max: u64) -> io::Result<MapStep> {
        debug_assert_eq!(tier.ns, self.flush_config.ns.0, "one section, every step");
        let mut work = MapWork::default();
        // Bound: one unit with work, past the stage moves that do none
        // (a listing's end, the files' end): at most three moves.
        loop {
            let stage = std::mem::replace(&mut self.stage, MapStage::Refused);
            self.stage = self.run(stage, tier, frames_max, &mut work)?;
            if work != MapWork::default() {
                return Ok(MapStep::Ran(work));
            }
            if let MapStage::Mapped = self.stage {
                return Ok(MapStep::Mapped(MapEnded(())));
            }
        }
    }

    /// One stage's unit, or its move to the next stage; answers the stage
    /// that follows. An `Err` leaves the map `Refused`.
    fn run(
        &mut self,
        stage: MapStage<F>,
        tier: &TierNsManifest,
        frames_max: u64,
        work: &mut MapWork,
    ) -> io::Result<MapStage<F>> {
        match stage {
            MapStage::Probe { at } => match tier.files.get(at) {
                Some(range) => self.probe(range, at, work),
                None => Ok(MapStage::ListOpen),
            },
            MapStage::ResealOpen { at } => {
                let range = file_at(tier, at)?;
                let (_, expect) = self.file_of(range);
                work.file_ops += 2; // the open and the truncate
                work.tier_reads += 1;
                let reseal = TierReseal::open(
                    &self.fs,
                    &self.flush_config.shard_dir,
                    range.id,
                    expect,
                    range.durable_len,
                    self.flush_config.mode,
                )?;
                Ok(MapStage::Reseal { at, reseal })
            }
            MapStage::Reseal { at, reseal } => {
                let (verify, read) = reseal.verify(frames_max)?;
                work.tier_reads += read;
                Ok(match verify {
                    Verify::More(reseal) => MapStage::Reseal { at, reseal },
                    Verify::Verified(verified) => MapStage::Seal { at, verified },
                })
            }
            MapStage::Seal { at, verified } => {
                verified.seal()?;
                work.tier_bytes += TIER_FOOTER_BYTES as u64;
                work.barriers += 1;
                self.stats.files_resealed += 1;
                Ok(MapStage::Handle { at, reason: SealReason::Recovered })
            }
            MapStage::Handle { at, reason } => self.hold(tier, at, reason, work),
            MapStage::ListOpen => {
                work.file_ops += 1;
                Ok(match self.fs.open_listing(&self.cold_dir)? {
                    OpenedDir::Absent => MapStage::Mapped,
                    OpenedDir::Open(stream) => MapStage::Listing {
                        stream,
                        batch: Box::new(NameBatch::new()),
                        next: 0,
                        end: BatchEnd::More,
                    },
                })
            }
            MapStage::Listing { stream, batch, next, end } => {
                self.listing(tier, stream, batch, next, end, work)
            }
            MapStage::Mapped => Ok(MapStage::Mapped),
            MapStage::Refused => Err(io::Error::other("the tier map refused at an earlier unit")),
        }
    }

    /// The held-handle unit of manifested file `at` (ADR-0054 D1; ADR-0174
    /// D3, D5): one handle per manifested file in its creation mode, held
    /// by the boot pipeline — the settle read opens nothing, and the
    /// plane's cold-read table inherits them at the hand-over — and the
    /// file's catalog entry.
    fn hold(
        &mut self,
        tier: &TierNsManifest,
        at: usize,
        reason: SealReason,
        work: &mut MapWork,
    ) -> io::Result<MapStage<F>> {
        let range = file_at(tier, at)?;
        let (path, expect) = self.file_of(range);
        work.file_ops += 1;
        let handle = self.fs.open_tier(&path, self.flush_config.mode)?;
        self.catalog.push(TierFileMeta {
            id: range.id,
            base: expect.base,
            // The manifested prefix is the readable address range; a
            // sealed file's physical excess is inert (ADR-0057 D5).
            data_len: range.durable_len,
            reason,
            path,
        });
        self.handles.push((range.id, handle));
        Ok(MapStage::Probe { at: at + 1 })
    }

    /// A listing unit: the next name of the held batch handled, or the
    /// next batch read, or the listing's end.
    fn listing(
        &mut self,
        tier: &TierNsManifest,
        mut stream: F::DirStream,
        mut batch: Box<NameBatch>,
        next: usize,
        end: BatchEnd,
        work: &mut MapWork,
    ) -> io::Result<MapStage<F>> {
        if let Some(name) = batch.name(next) {
            work.ram_units += 1;
            self.list_name(tier, name, work)?;
            return Ok(MapStage::Listing { stream, batch, next: next + 1, end });
        }
        match end {
            BatchEnd::More => {
                work.file_ops += 1;
                let end = stream.next_batch(&mut batch)?;
                Ok(MapStage::Listing { stream, batch, next: 0, end })
            }
            BatchEnd::End => Ok(MapStage::Mapped),
        }
    }

    /// The probe unit of manifested file `at`: its header identity and its
    /// footer verdict, from two block reads.
    fn probe(
        &mut self,
        range: &TierFileRange,
        at: usize,
        work: &mut MapWork,
    ) -> io::Result<MapStage<F>> {
        let (path, expect) = self.file_of(range);
        work.file_ops += 1;
        work.tier_reads += 2;
        let (header, footer) = probe_tier_file(&self.fs, &path)?;
        if header.identity != expect {
            return Err(invalid(format!(
                "tier file {}: identity {:?} on disk, {expect:?} manifested",
                path.display(),
                header.identity
            )));
        }
        let Some(footer) = footer else { return Ok(MapStage::ResealOpen { at }) };
        if footer.data_len < range.durable_len {
            return Err(invalid(format!(
                "tier file {}: sealed at {} but the manifest claims {} durable bytes",
                path.display(),
                footer.data_len,
                range.durable_len
            )));
        }
        self.stats.files_sealed += 1;
        Ok(MapStage::Handle { at, reason: footer.reason })
    }

    /// One listed name handled (a RAM unit): an extent's id or a
    /// quarantined one collected (names only — ADR-0061 D6: liveness is the
    /// post-replay refcounts', so the tier-file rule never reaches an
    /// extent), and an unmanifested tier file — dead-life garbage —
    /// removed before any new flush references its address space (the S11
    /// recovery pre-flush rule with the manifest as the authority). A name
    /// that is not UTF-8 is foreign and kept, as its lossy form never
    /// parses (ADR-0202 D9); so is every other foreign name.
    fn list_name(
        &mut self,
        tier: &TierNsManifest,
        name: &OsStr,
        work: &mut MapWork,
    ) -> io::Result<()> {
        let Some(name) = name.to_str() else { return Ok(()) };
        if let Some(extent_id) = parse_extent_file_name(name) {
            self.extents_listed.push(extent_id.0);
            return Ok(());
        }
        if let Some(extent_id) = parse_quarantined_file_name(name) {
            self.extents_quarantined.push(extent_id.0);
            return Ok(());
        }
        let Some(id) = parse_tier_file_name(name) else { return Ok(()) };
        // The planted canary breaks ADR-0174 D4: a namespace recovered
        // through the empty section keeps its dead-life tier files.
        let empty_section = tier.flushed == 0 && tier.files.is_empty();
        if cfg!(inf_canary_replay_no_section_gc) && empty_section {
            return Ok(());
        }
        // The section's ids strictly ascend (the decoder's rule), so a
        // name's manifest search is a binary search.
        if tier.files.binary_search_by_key(&id, |file| file.id).is_err() {
            work.file_ops += 1;
            self.fs.remove_file(&self.cold_dir.join(name))?;
            self.stats.files_removed += 1;
        }
        Ok(())
    }

    /// Manifested file `range`'s path and the identity its header must
    /// carry.
    fn file_of(&self, range: &TierFileRange) -> (PathBuf, TierIdentity) {
        let expect = TierIdentity {
            cell: self.flush_config.cell,
            ns: self.flush_config.ns,
            base: LogicalAddr::from_raw(range.base).expect("manifest decode checked 48 bits"),
        };
        (self.cold_dir.join(tier_file_name(range.id)), expect)
    }

    /// The build, the map's last unit (one RAM unit a catalog file, at
    /// least one): the table at the new life's origin, the live set seeded
    /// from the catalog and the boot pipeline over the held handles, with
    /// `next_id` one above the highest named id.
    ///
    /// # Errors
    /// `OutOfMemory` when the ring reservation fails.
    pub fn build(
        self,
        tier: &TierNsManifest,
        _ended: MapEnded,
    ) -> io::Result<(RecoveredTier<F>, MapWork)> {
        let NsTierMap {
            fs,
            boot_ckpt_id,
            flush_config,
            space,
            demote,
            initial_keys,
            hasher,
            census,
            catalog,
            handles,
            extents_listed,
            extents_quarantined,
            stats,
            ..
        } = self;
        let work = MapWork { ram_units: (catalog.len() as u64).max(1), ..MapWork::default() };
        let next_id = tier.files.iter().map(|f| f.id + 1).max().unwrap_or(0);
        let mut table = TieredTable::new(
            AddressSpaceConfig {
                life_origin: LogicalAddr::from_raw(tier.flushed)
                    .expect("manifest decode checked 48 bits"),
                ..space
            },
            demote,
            initial_keys,
            hasher,
        )
        .ok_or_else(|| {
            io::Error::new(io::ErrorKind::OutOfMemory, "tier ring reservation failed")
        })?;
        // Live-set seeding (M4-S14, ADR-0058 D4): counts start at zero and
        // reconstruct through `replay_ref`/`replay_displace` as the checkpoint
        // and tail replay run; byte counters restore when the `.ick` 0x04
        // section arrives ([`apply_live_set_section`]).
        table.seed_recovered_files(&catalog, boot_ckpt_id);
        let flush = TierFlush::with_catalog(fs, flush_config, next_id, catalog, &census);
        let replay = TierReplay::new(BootFlush::new(flush, handles), &table);
        let recovered = RecoveredTier { table, replay, extents_listed, extents_quarantined, stats };
        Ok((recovered, work))
    }
}

/// Manifested file `at` of `tier`: a bounds check of the map's cursor.
fn file_at(tier: &TierNsManifest, at: usize) -> io::Result<&TierFileRange> {
    tier.files.get(at).ok_or_else(|| {
        invalid(format!("ns {}: the tier map's file {at} is past its section", tier.ns))
    })
}

/// Recovers one tiered namespace from its manifest section, whole: its
/// [`NsTierMap`] stepped to its end, then built (the store tier, the crash
/// matrix and the simulator; the server's recovery driver steps the same
/// map under its budget).
///
/// # Errors
/// As [`NsTierMap::step`] and [`NsTierMap::build`].
// the recovery entry's seven inputs, the key hasher (ADR-0094) and the
// cell's cap census, which the recovered pipeline's holders register on
#[allow(clippy::too_many_arguments)]
pub fn recover_tiered_ns<F: SegmentFs>(
    fs: F,
    tier: &TierNsManifest,
    boot_ckpt_id: u64,
    flush_config: TierFlushConfig,
    space: AddressSpaceConfig,
    demote: DemotionConfig,
    initial_keys: usize,
    hasher: KeyHasher,
    census: &Rc<CapCensus>,
) -> io::Result<RecoveredTier<F>> {
    let mut map = NsTierMap::new(
        fs,
        tier,
        boot_ckpt_id,
        flush_config,
        space,
        demote,
        initial_keys,
        hasher,
        census,
    );
    // Bound: one unit a call, over the section's files, their frames and
    // the directory's names.
    loop {
        match map.step(tier, u64::MAX)? {
            MapStep::Ran(_) => {}
            MapStep::Mapped(ended) => {
                return map.build(tier, ended).map(|(recovered, _)| recovered);
            }
        }
    }
}

/// Applies at most `max` entries of one validated `.ick` address-reference
/// section view, from its first (ADR-0057 D6 step 3), and answers how many.
/// At the section's entry (`first_index` 0) it first cross-checks the
/// section's walk watermark against the manifested flushed watermark — a
/// section claiming refs above it means the checkpoint and the manifest
/// are not one recovery unit (fail-stop) — and refuses a section for a
/// namespace whose table already holds a record of this life (ADR-0174 R2:
/// within a namespace every ref section precedes every image section, so a
/// ref can never name a key a replayed record of this life already settled
/// against). A resumed view skips both: refs append nothing, and between
/// two recovery steps nothing else changes the table. Each entry applies
/// idempotently.
///
/// # Errors
/// `InvalidData` on the watermark cross-check and on the section order.
pub fn apply_ref_entries(
    table: &mut TieredTable,
    section: &IckRefSection<'_>,
    manifested_flushed: u64,
    max: usize,
) -> io::Result<usize> {
    if section.first_index == 0 {
        check_ref_section(table, section.ns, section.walk_watermark, manifested_flushed)?;
    }
    Ok(replay_refs(table, section.iter().take(max)))
}

/// [`apply_ref_entries`] run to completion over a whole section.
///
/// # Errors
/// As [`apply_ref_entries`].
pub fn apply_ref_section(
    table: &mut TieredTable,
    section: &IckRefSection<'_>,
    manifested_flushed: u64,
) -> io::Result<()> {
    apply_ref_entries(table, section, manifested_flushed, usize::MAX).map(|_| ())
}

/// A ref section's checks and every entry over the section's facts
/// (reachable by a test without a decoded section).
#[cfg(test)]
fn apply_refs(
    table: &mut TieredTable,
    ns: u32,
    walk_watermark: u64,
    refs: impl Iterator<Item = (u64, u64)>,
    manifested_flushed: u64,
) -> io::Result<()> {
    check_ref_section(table, ns, walk_watermark, manifested_flushed)?;
    replay_refs(table, refs);
    Ok(())
}

/// The one place of a ref section's two checks.
fn check_ref_section(
    table: &TieredTable,
    ns: u32,
    walk_watermark: u64,
    manifested_flushed: u64,
) -> io::Result<()> {
    if walk_watermark > manifested_flushed {
        return Err(invalid(format!(
            "ick ref section (ns {ns}) walk watermark {walk_watermark} outruns the manifested \
             flushed {manifested_flushed}"
        )));
    }
    let (origin, tail) = (table.space().life_origin(), table.space().tail());
    if tail > origin {
        return Err(invalid(format!(
            "ick ref section (ns {ns}) after an image of its namespace: the table holds {} bytes \
             of this life above its origin {} (ADR-0174 R2)",
            tail.offset_from(origin),
            origin.to_raw()
        )));
    }
    Ok(())
}

/// Replays each ref; answers how many.
fn replay_refs(table: &mut TieredTable, refs: impl Iterator<Item = (u64, u64)>) -> usize {
    let mut applied = 0usize;
    for (hash, addr) in refs {
        table.replay_ref(hash, LogicalAddr::from_raw(addr).expect("reader checked 48 bits"));
        applied += 1;
    }
    applied
}

/// Applies at most `max` entries of one validated `.ick` live-set section
/// view, from its first (M4-S14, ADR-0058 D4/D5), and answers how many:
/// every entry restores under the D5 clamp rules — a length mismatch or an
/// unmanifested file id restores nothing, so the byte counters can only
/// ever under-count dead. Infallible by construction; the decoder already
/// audited shape, flags, and `dead ≤ len`.
pub fn apply_live_set_entries(
    table: &mut TieredTable,
    section: &IckLiveSetSection<'_>,
    max: usize,
) -> usize {
    let mut applied = 0usize;
    for entry in section.iter().take(max) {
        table.restore_live_entry(&entry);
        applied += 1;
    }
    applied
}

/// [`apply_live_set_entries`] run to completion over a whole section.
pub fn apply_live_set_section(table: &mut TieredTable, section: &IckLiveSetSection<'_>) {
    apply_live_set_entries(table, section, usize::MAX);
}

/// Applies at most `max` entries of one validated `.ick` blob-reference
/// section view, from its first (M4-S17, ADR-0061 D6), and answers how
/// many: each restores a reference-map cold entry, so the replayed tail's
/// displacements decrement the right extent, and advances the
/// allocate-once id cursor past its id. Pairs with the ref section (the
/// slots) — this section is bookkeeping, not index content. Infallible by
/// construction; the decoder already audited shape, order, and lengths.
pub fn apply_blob_ref_entries(
    table: &mut TieredTable,
    section: &IckBlobRefSection<'_>,
    max: usize,
) -> usize {
    let mut applied = 0usize;
    for entry in section.iter().take(max) {
        table.restore_extent_entry(entry.addr, entry.extent_id, entry.len);
        applied += 1;
    }
    applied
}

/// [`apply_blob_ref_entries`] run to completion over a whole section.
pub fn apply_blob_ref_section(table: &mut TieredTable, section: &IckBlobRefSection<'_>) {
    apply_blob_ref_entries(table, section, usize::MAX);
}

fn invalid(message: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests {
    use inf_foundation::KeyHasher;

    use super::*;

    /// A recovered table at origin 1 MiB with one manifested file below
    /// it (a ref must name a file's range).
    fn table() -> TieredTable {
        let demote = DemotionConfig::for_budget(1 << 20, 4 << 10);
        let mut table = TieredTable::new(
            AddressSpaceConfig {
                reserve_bytes: demote.ring_reserve_bytes().expect("valid budget"),
                page_bytes: 4 << 10,
                life_origin: LogicalAddr::from_raw(1 << 20).expect("48-bit"),
            },
            demote,
            64,
            KeyHasher::default(),
        )
        .expect("ring");
        table.seed_recovered_files(
            &[TierFileMeta {
                id: 0,
                base: LogicalAddr::ZERO,
                data_len: 1 << 20,
                reason: SealReason::Capacity,
                path: std::path::Path::new("shard-0/cold/tier-000000.itier").to_path_buf(),
            }],
            1,
        );
        table
    }

    /// The chunked id list (ADR-0174 A1): a push past a full chunk starts
    /// the next, an index names one id in list order across the chunk
    /// edge, a run ends at its chunk's end and is empty past the list, and
    /// the list reads back whole.
    #[test]
    fn an_id_list_reads_back_by_index_across_its_chunks() {
        let n = 2 * ID_CHUNK_IDS + 3;
        let list: IdChunks = (0..n as u64).map(|i| i * 7).collect();
        assert_eq!(list.len(), n);
        assert_eq!(list.to_vec(), (0..n as u64).map(|i| i * 7).collect::<Vec<_>>());
        assert_eq!(list.run_from(0).len(), ID_CHUNK_IDS, "a run ends at its chunk's end");
        assert_eq!(list.run_from(ID_CHUNK_IDS - 1), &[(ID_CHUNK_IDS as u64 - 1) * 7]);
        assert_eq!(list.run_from(ID_CHUNK_IDS)[0], ID_CHUNK_IDS as u64 * 7, "the next chunk");
        assert_eq!(list.run_from(n - 1), &[(n as u64 - 1) * 7], "the last id");
        assert!(list.run_from(n).is_empty() && list.run_from(usize::MAX).is_empty());
        let from: Vec<u64> = list.iter_from(ID_CHUNK_IDS - 2).take(4).collect();
        let want: Vec<u64> =
            (ID_CHUNK_IDS as u64 - 2..ID_CHUNK_IDS as u64 + 2).map(|i| i * 7).collect();
        assert_eq!(from, want, "a walk from an index crosses the chunk edge");
        assert!(list.contains(&(7 * 7)) && !list.contains(&1));
        assert!(IdChunks::new().is_empty() && IdChunks::new().run_from(0).is_empty());
    }

    /// A namespace's cold directory on `fs`: two sealed manifested files,
    /// an unsealed one of three frames, a dead life's file, two extent
    /// names, a quarantine twin and a foreign name; its section.
    fn mapped_unit(fs: &inf_log::fs::mem::MemFs) -> TierNsManifest {
        use inf_log::tier::TierWriter;
        let shard = std::path::Path::new("shard-0/ns-17");
        let data = inf_log::tier::TIER_FRAME_DATA as u64;
        let mut files = Vec::new();
        let mut base = 0u64;
        for (id, frames) in [(0u32, 1u64), (1, 2), (2, 3)] {
            let at = LogicalAddr::from_raw(base).expect("48 bits");
            let mode = inf_log::TierIoMode::Buffered;
            let mut writer =
                TierWriter::create(fs, shard, id, 0, inf_log::NsId(17), at, mode).expect("create");
            writer.append(at, &vec![id as u8; (frames * data) as usize]).expect("append");
            writer.sync().expect("sync");
            if id < 2 {
                writer.seal(SealReason::Capacity).expect("seal");
            }
            files.push(TierFileRange { id, base, durable_len: frames * data });
            base += frames * data;
        }
        let cold = shard.join("cold");
        for name in [
            "tier-000003.itier",
            "blob-000007.iblob",
            "blob-000002.iblob",
            "blob-000009.iblob.quarantine",
            "notes.txt",
        ] {
            fs.create_meta(&cold.join(name)).expect("a name");
        }
        TierNsManifest { ns: 17, flushed: base, files }
    }

    fn map_of(
        fs: &inf_log::fs::mem::MemFs,
        tier: &TierNsManifest,
    ) -> NsTierMap<inf_log::fs::mem::MemFs> {
        let demote = DemotionConfig::for_budget(1 << 20, 4 << 10);
        let census = inf_foundation::bounded::CapCensus::new(inf_foundation::CellId(0)).0;
        NsTierMap::new(
            fs.clone(),
            tier,
            1,
            TierFlushConfig {
                shard_dir: std::path::PathBuf::from("shard-0/ns-17"),
                cell: 0,
                ns: inf_log::NsId(17),
                mode: inf_log::TierIoMode::Buffered,
                file_capacity: 1 << 20,
                slice_bytes: 4 << 10,
                record_len_max: 16 << 20,
            },
            AddressSpaceConfig {
                reserve_bytes: demote.ring_reserve_bytes().expect("valid budget"),
                page_bytes: 4 << 10,
                life_origin: LogicalAddr::ZERO,
            },
            demote,
            64,
            KeyHasher::default(),
            &census,
        )
    }

    /// The tier map a unit a step (ADR-0174 A1): stepped one frame a reseal
    /// call, each step one unit — at most an open and two block reads, an
    /// open, a truncate and a read, one frame read, one seal's footer and
    /// barrier, or one listed name with its removal — it builds what the
    /// whole map builds: the catalog, the boot facts, the extent ids, and
    /// the directory left.
    #[test]
    fn a_tier_map_stepped_a_unit_at_a_time_builds_what_the_whole_map_builds() {
        let whole_fs = inf_log::fs::mem::MemFs::new();
        let tier = mapped_unit(&whole_fs);
        let mut whole = map_of(&whole_fs, &tier);
        let whole_ended = loop {
            match whole.step(&tier, u64::MAX).expect("a unit") {
                MapStep::Ran(_) => {}
                MapStep::Mapped(ended) => break ended,
            }
        };
        let (whole, _) = whole.build(&tier, whole_ended).expect("the whole build");
        let fs = inf_log::fs::mem::MemFs::new();
        let tier = mapped_unit(&fs);
        let mut map = map_of(&fs, &tier);
        let mut units = 0;
        let ended = loop {
            match map.step(&tier, 1).expect("a unit") {
                MapStep::Ran(work) => {
                    units += 1;
                    let ops = work.file_ops + work.tier_reads + work.barriers + work.ram_units;
                    assert!(work.tier_reads <= 2 && work.file_ops <= 2 && ops <= 3, "{work:?}");
                }
                MapStep::Mapped(ended) => break ended,
            }
        };
        let (stepped, build) = map.build(&tier, ended).expect("the stepped build");
        // 3 probes, the reseal's open, 3 frame reads and its seal, 3 handles,
        // the listing's open and one batch, and its 8 names.
        assert_eq!(units, 3 + 1 + 3 + 1 + 3 + 2 + 8, "one unit a step");
        assert_eq!(build.ram_units, 3, "the build: a RAM unit a catalog file");
        assert_eq!(stepped.stats, whole.stats);
        assert_eq!(stepped.stats.files_removed, 1, "the dead life's file");
        assert_eq!(stepped.stats.files_resealed, 1);
        assert_eq!(stepped.replay.sealed(), whole.replay.sealed(), "the catalog");
        assert_eq!(stepped.extents_listed, whole.extents_listed);
        assert_eq!(stepped.extents_quarantined, whole.extents_quarantined);
        let left = |fs: &inf_log::fs::mem::MemFs| {
            ["tier-000000.itier", "tier-000001.itier", "tier-000002.itier", "tier-000003.itier"]
                .map(|name| fs.contents(&std::path::Path::new("shard-0/ns-17/cold").join(name)))
        };
        assert_eq!(left(&fs), left(&whole_fs), "the directory left, byte for byte");
    }

    /// ADR-0174 R2, the reader half: a ref section for a namespace
    /// whose table already holds a record of this life is a typed boot
    /// refusal naming the namespace; the reverse order applies. Red
    /// before the law: the refs applied beside the image.
    #[test]
    fn a_ref_section_after_an_image_of_its_namespace_refuses_the_boot() {
        let flushed = 1u64 << 20;
        let hash = KeyHasher::default().hash(b"k");
        let refs = [(hash, 4096u64)];
        let mut t = table();
        apply_refs(&mut t, 41, flushed, refs.iter().copied(), flushed).expect("refs first");
        t.replay_upsert(&[], b"k", b"v", hash).expect("fits");
        assert_eq!(t.len(), 2, "two slots: the ref and this life's record (no rebuild yet)");
        let err = apply_refs(&mut t, 41, flushed, refs.iter().copied(), flushed)
            .expect_err("a ref section after an image of its namespace");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        let text = err.to_string();
        assert!(text.contains("ns 41") && text.contains("ADR-0174 R2"), "{text}");
        let mut fresh = table();
        fresh.replay_upsert(&[], b"k", b"v", hash).expect("fits");
        let err = apply_refs(&mut fresh, 41, flushed, refs.iter().copied(), flushed)
            .expect_err("the first ref section after an image refuses too");
        assert!(err.to_string().contains("ADR-0174 R2"));
        assert!(!fresh.contains_pair(hash, LogicalAddr::from_raw(4096).expect("48-bit")));
    }
}
