//! Blob-extent reference counting (M4-S17, ADR-0061 D4–D6) — the
//! cell-local bookkeeping for out-of-line values.
//!
//! `refcount(E)` ≡ the number of live records whose extent reference
//! names `E` ≡ the number of **reference-map** entries naming `E`. The
//! map (`record address → {extent id, value len}`, one entry per live
//! extent-carrying record) is the RAM-resident identity that lets a
//! *cold* record's death name its extent without a disk read (§3.3
//! forbids the read; the S02 hash sidecar solves the same problem for
//! keys). All state is per-cell, single-owner, no atomics (L1); the map
//! is a named L5 term bounded by `disk_budget / blob_threshold` entries.
//!
//! Lifecycle sites (ADR-0061 D4): births at `insert_extent`/
//! `update_extent`/`apply_extent_image`; deaths ride the `note_death`
//! routing (the S06/S14 choke point) on its unconditional side;
//! compaction **moves** the entry before the old address dies — the
//! count never dips or spikes across a relocation. Reclaim gates on the
//! killing record's WAL durability, not on checkpoints (deaths are
//! logged; ADR-0061 D5), then on the plane's in-flight read pins.

use std::collections::BTreeMap;
use std::collections::VecDeque;

/// Default out-of-line threshold: exactly the u24 inline ceiling, so the
/// default changes no existing value's path (ADR-0061 D1). Values at or
/// above it must take the extent path.
pub const BLOB_THRESHOLD_DEFAULT: u32 = 1 << 24;
/// Default hard cap on one value (everything has a limit; the S17
/// budget row is proven at exactly this size).
pub const BLOB_MAX_BYTES_DEFAULT: u64 = 1 << 30;
/// Default unlink candidates one MAINTAIN reclaim slice hands the plane
/// (M4-S18): each candidate is one syscall-class unlink, so the bound
/// keeps a slice's wall time flat regardless of backlog depth — the
/// backlog itself stays visible as `blob_reclaimable`. A plane budget,
/// not an `INF.NS` key (the `EvictBudget` class of bound).
pub const BLOB_RECLAIM_PER_SLICE_DEFAULT: usize = 8;

/// Per-namespace blob routing bounds (ADR-0061 D1). Construction
/// parameters until S19's `INF.NS` ADR ships the knob (`BLOB-THRESHOLD`
/// reserved; the plan's cut line allows the knob to slip to v0.4.1 —
/// the mechanism may not).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct BlobConfig {
    /// Values with `len >= threshold_bytes` store out of line.
    pub threshold_bytes: u32,
    /// Hard per-value cap — beyond it the write refuses typed.
    pub max_bytes: u64,
}

impl Default for BlobConfig {
    fn default() -> BlobConfig {
        BlobConfig { threshold_bytes: BLOB_THRESHOLD_DEFAULT, max_bytes: BLOB_MAX_BYTES_DEFAULT }
    }
}

impl BlobConfig {
    /// Panics on nonsense bounds (config validation is loud — the
    /// `CompactionConfig` posture).
    pub fn validate(&self) {
        assert!(self.threshold_bytes > 0, "a zero blob threshold routes every value out of line");
        assert!(
            self.max_bytes >= u64::from(self.threshold_bytes),
            "the value cap sits at or above the threshold"
        );
    }
}

/// One live extent's aggregate: how many live records reference it and
/// the value length its header declares (accounting; restore-checked).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
struct ExtentEntry {
    refs: u64,
    len: u64,
}

/// Why a reclaim candidate is eligible (ADR-0096 D1) — the plane's
/// disposal dispatches on it: a refcount-proven death unlinks; a
/// boot-listed orphan is probed and **quarantined** (renamed), never
/// unlinked in the life it booted in; a quarantined file re-listed by a
/// later boot — still unreferenced — unlinks its `.quarantine` twin.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ReclaimOrigin {
    /// Refcount reached zero in this life or replayed from the log
    /// (ADR-0061 D5) — the count is the proof; unlink as always.
    Death,
    /// Listed at boot, referenced by nothing after replay (ADR-0061 D6)
    /// — the verdict rests on one rebuilt map, so the disposal is
    /// probe-then-quarantine (ADR-0096 D2).
    BootOrphan,
    /// A `.quarantine` twin re-listed by this boot and still
    /// unreferenced — the second verdict (ADR-0096 D4).
    Quarantined,
}

/// One reclaim candidate handed to the plane: the extent id and the
/// provenance its disposal dispatches on (ADR-0096 D1).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct ReclaimCandidate {
    /// The extent to dispose of.
    pub extent_id: u64,
    /// Why it is eligible — selects unlink vs quarantine vs
    /// second-verdict unlink.
    pub origin: ReclaimOrigin,
}

/// A reclaim candidate: refcount hit zero; `stamp` is the WAL epoch its
/// killing record staged under (0 = durable by construction — replayed
/// or orphaned). `len` is the dead extent's declared value length — its
/// bytes stay on the device until the unlink completes, so the
/// disk-budget accounting (M4-S19, ADR-0062 D5) carries it through the
/// queue. Boot-sweep orphans carry 0 (the sweep lists names only — a
/// bounded under-count that heals at their unlink, disclosed).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
struct Reclaimable {
    extent_id: u64,
    len: u64,
    stamp: u64,
    origin: ReclaimOrigin,
}

/// Observable blob-extent state (`INFO tiering` + the §3.3 zero-assert
/// lists — memory-mode namespaces have no table, hence all-zero).
#[derive(Copy, Clone, Default, Debug, PartialEq, Eq)]
pub struct ExtentStats {
    /// Extents currently referenced by at least one live record.
    pub live: u64,
    /// Value bytes those extents hold (declared lengths).
    pub live_bytes: u64,
    /// Extents allocated this life (ids handed out).
    pub created: u64,
    /// Extents whose unlink completed.
    pub reclaimed: u64,
    /// Refcount-zero extents awaiting durability, pins, or unlink.
    pub reclaimable: u64,
    /// Reclaim work handed to the plane this life (slices).
    pub reclaim_slices: u64,
    /// Unlink failures deferred non-fatally (`blob_unlink_fail` — the
    /// candidate re-offers; the boot sweep re-drives after a crash).
    pub reclaim_deferred: u64,
    /// Boot orphans quarantined instead of unlinked (ADR-0096 D2 —
    /// header-valid files renamed to their `.quarantine` twin).
    pub quarantined: u64,
    /// Quarantined extents revived at boot because the replayed map
    /// references them (ADR-0096 D3 — a wrong orphan verdict healed;
    /// nonzero is the upstream-accounting falsifier signal).
    pub quarantine_revived: u64,
    /// Blob read-modify-write rewrites (ADR-0061 D7 — the doc-path cost
    /// counter, reserved at this seam; zero until doc-tiering wires).
    pub rmw_ops: u64,
    /// Device bytes the namespace's extents hold right now — live plus
    /// awaiting reclaim (M4-S19, ADR-0062 D5: a dead extent's bytes are
    /// on disk until its unlink completes, so the disk budget counts
    /// them). Boot-sweep orphans count 0 until unlinked (names-only
    /// listing — a bounded, disclosed under-count).
    pub disk_bytes: u64,
}

/// The per-namespace extent table: reference map + refcounts + the
/// stamped reclaim queue + the allocate-once id cursor.
#[derive(Default, Debug)]
pub struct ExtentRefs {
    /// record address (raw) → (extent id, value len) — ordered, so the
    /// checkpoint section emits in ascending address order for free.
    refs: BTreeMap<u64, (u64, u64)>,
    /// extent id → live aggregate. Ordered for deterministic iteration.
    extents: BTreeMap<u64, ExtentEntry>,
    /// Refcount-zero extents whose killing record has not staged yet —
    /// stamped with the WAL epoch at the next successful `stage_wal`
    /// (exact when the death's own effect stages next; conservative —
    /// deferral, never early release — under any other order). Carries
    /// `(extent id, value len)` — the len rides the whole queue for the
    /// D5 disk accounting.
    parked: Vec<(u64, u64)>,
    /// Stamped candidates, drained by `reclaim_work` under the plane's
    /// durable epoch.
    reclaimable: VecDeque<Reclaimable>,
    /// Candidates handed out and not yet confirmed or deferred, as
    /// `(extent id, value len, origin)`.
    in_reclaim: Vec<(u64, u64, ReclaimOrigin)>,
    /// Allocate-once cursor (ADR-0061 D1/D6): never reissued while any
    /// durable artifact can name the id.
    next_extent_id: u64,
    stats: ExtentStats,
}

impl ExtentRefs {
    pub fn new() -> ExtentRefs {
        ExtentRefs { next_extent_id: 1, ..ExtentRefs::default() }
    }

    /// Allocates the next extent id (the plane creates the file under
    /// it *before* the referencing record exists; a failed write
    /// quarantines the id — never reused, swept as an orphan).
    pub fn allocate_id(&mut self) -> u64 {
        let id = self.next_extent_id;
        self.next_extent_id += 1;
        self.stats.created += 1;
        id
    }

    /// Advances the cursor past an id observed from a durable artifact
    /// (0x05 restore, tag-9 image/tail replay, the directory listing).
    pub fn note_observed_id(&mut self, extent_id: u64) {
        self.next_extent_id = self.next_extent_id.max(extent_id + 1);
    }

    /// Registers a live record's reference (birth site). Idempotent on
    /// the exact same entry (the walker's at-least-once duplicates);
    /// a *different* entry at a registered address is a lifecycle bug.
    pub fn register(&mut self, addr: u64, extent_id: u64, len: u64) {
        debug_assert!(len > 0, "an extent reference names at least one byte");
        self.note_observed_id(extent_id);
        // Retract a pending park (found by the DST sweep, 329/3000 seeds):
        // replay legitimately dips a count to zero and revives it — a
        // mid-walk mutation is captured twice (imaged by the fuzzy walk
        // AND re-covered by the tail, the ADR-0057 D4 rules), and the
        // displace-then-reapply pairing transiently zeroes the extent's
        // count between the two. A park latched at the dip must revoke at
        // re-registration or the boot sweep reclaims a live extent — the
        // same at-least-once physics that makes `apply_ref` idempotent
        // (D4 rule 3), applied to the reclaim queue.
        self.parked.retain(|&(id, _)| id != extent_id);
        if let Some(at) = self.reclaimable.iter().position(|r| r.extent_id == extent_id) {
            self.reclaimable.remove(at);
        }
        debug_assert!(
            !self.in_reclaim.iter().any(|&(id, _, _)| id == extent_id),
            "a handed-out reclaim candidate re-registered (the plane may be unlinking it)"
        );
        match self.refs.insert(addr, (extent_id, len)) {
            None => {}
            Some(prev) => {
                assert_eq!(
                    prev,
                    (extent_id, len),
                    "an address re-registered with a different reference"
                );
                return; // exact duplicate — counted once
            }
        }
        let entry = self.extents.entry(extent_id).or_insert(ExtentEntry { refs: 0, len });
        debug_assert_eq!(entry.len, len, "one extent, one declared length");
        entry.refs += 1;
    }

    /// Moves a reference across a relocation (ADR-0061 D4): the count
    /// never changes — called **before** the old address's `note_death`,
    /// which then finds no entry and decrements nothing. Returns whether
    /// an entry moved (false for non-extent records — the caller checks
    /// the record tag first, this is the belt to that suspender).
    pub fn relocate(&mut self, old_addr: u64, new_addr: u64) -> bool {
        let Some(entry) = self.refs.remove(&old_addr) else {
            return false;
        };
        let prev = self.refs.insert(new_addr, entry);
        debug_assert!(prev.is_none(), "relocation target already registered");
        true
    }

    /// Death site (rides `TieredTable::note_death`, unconditional side):
    /// removes the address's reference if it names an extent; a count
    /// reaching zero parks the extent for the durability stamp. Returns
    /// the extent id that hit zero, if any (tests assert on it).
    pub fn note_death(&mut self, addr: u64) -> Option<u64> {
        let (extent_id, len) = self.refs.remove(&addr)?;
        let entry = self.extents.get_mut(&extent_id).expect("a mapped reference has an extent");
        debug_assert!(entry.refs > 0, "refcount underflow");
        entry.refs -= 1;
        if entry.refs > 0 {
            return None;
        }
        self.extents.remove(&extent_id);
        self.parked.push((extent_id, len));
        Some(extent_id)
    }

    /// Stamps every parked extent with `wal_epoch` (called by
    /// `stage_wal` after a successful staging — the killing record is
    /// at or before this epoch, so `epoch ≤ durable` implies the death
    /// is durable; ADR-0061 D5).
    pub fn stamp(&mut self, wal_epoch: u64) {
        for (extent_id, len) in self.parked.drain(..) {
            self.reclaimable.push_back(Reclaimable {
                extent_id,
                len,
                stamp: wal_epoch,
                origin: ReclaimOrigin::Death,
            });
        }
    }

    /// Seeds the boot sweep (ADR-0061 D6, disposal per ADR-0096): every
    /// listed extent id not referenced by any live record is an orphan,
    /// immediately *eligible* (stamp 0 — nothing durable names it, by
    /// the replay that just completed) — typed `BootOrphan`, so the
    /// plane quarantines rather than unlinks. A quarantined id still
    /// unreferenced is the second verdict (`Quarantined` — the plane
    /// unlinks its twin); a quarantined id the replayed map *does*
    /// reference is returned for revival (the caller renames it back
    /// and counts it — ADR-0096 D3). Advances the id cursor past
    /// everything listed and stamps any parked replay deaths at 0 (they
    /// were replayed *from* the log — durable by construction).
    pub fn sweep_seed(&mut self, listed: &[u64], quarantined: &[u64]) -> Vec<u64> {
        self.stamp(0);
        for &extent_id in listed {
            self.note_observed_id(extent_id);
            if self.sweep_candidate(extent_id) {
                // Orphans list by name only (D6 — no content reads at
                // boot), so their device bytes are unknown: len 0 is the
                // disclosed under-count that heals at unlink.
                self.reclaimable.push_back(Reclaimable {
                    extent_id,
                    len: 0,
                    stamp: 0,
                    origin: ReclaimOrigin::BootOrphan,
                });
            }
        }
        let mut revive = Vec::new();
        for &extent_id in quarantined {
            self.note_observed_id(extent_id);
            if self.extents.contains_key(&extent_id) {
                self.stats.quarantine_revived += 1;
                revive.push(extent_id);
            } else if self.sweep_candidate(extent_id) {
                self.reclaimable.push_back(Reclaimable {
                    extent_id,
                    len: 0,
                    stamp: 0,
                    origin: ReclaimOrigin::Quarantined,
                });
            }
        }
        revive
    }

    /// True when a listed id is neither referenced nor already queued —
    /// the sweep's candidacy test.
    fn sweep_candidate(&self, extent_id: u64) -> bool {
        let live = self.extents.contains_key(&extent_id);
        let queued = self.reclaimable.iter().any(|r| r.extent_id == extent_id)
            || self.in_reclaim.iter().any(|&(id, _, _)| id == extent_id);
        !live && !queued
    }

    /// Hands the plane up to `max` disposal candidates whose killing
    /// record is durable (`stamp ≤ durable_epoch`), each typed with its
    /// [`ReclaimOrigin`] (ADR-0096 D1). The plane composes the
    /// in-flight read pin check (pins are runtime state the store never
    /// sees — the ADR-0059 D3 fence), dispatches the disposal on the
    /// origin, and answers each candidate via
    /// [`reclaim_done`](Self::reclaim_done),
    /// [`reclaim_quarantined`](Self::reclaim_quarantined), or
    /// [`reclaim_deferred`](Self::reclaim_deferred).
    pub fn reclaim_work(&mut self, durable_epoch: u64, max: usize) -> Vec<ReclaimCandidate> {
        let mut out = Vec::new();
        let mut kept = VecDeque::new();
        while let Some(candidate) = self.reclaimable.pop_front() {
            if out.len() < max && candidate.stamp <= durable_epoch {
                out.push(ReclaimCandidate {
                    extent_id: candidate.extent_id,
                    origin: candidate.origin,
                });
                self.in_reclaim.push((candidate.extent_id, candidate.len, candidate.origin));
            } else {
                kept.push_back(candidate);
            }
        }
        self.reclaimable = kept;
        if !out.is_empty() {
            self.stats.reclaim_slices += 1;
        }
        out
    }

    /// Confirms one unlink (the file is gone; `statfs` sees the space).
    pub fn reclaim_done(&mut self, extent_id: u64) {
        let at = self.in_reclaim.iter().position(|&(id, _, _)| id == extent_id);
        let at = at.expect("reclaim_done for a candidate reclaim_work handed out");
        self.in_reclaim.swap_remove(at);
        self.stats.reclaimed += 1;
    }

    /// Confirms one boot-orphan quarantine (ADR-0096 D2): the file left
    /// the reachable namespace by rename, not unlink — counted apart
    /// from `reclaimed` because the bytes are still on the device until
    /// a later boot's second verdict.
    pub fn reclaim_quarantined(&mut self, extent_id: u64) {
        let at = self.in_reclaim.iter().position(|&(id, _, _)| id == extent_id);
        let at = at.expect("reclaim_quarantined for a candidate reclaim_work handed out");
        self.in_reclaim.swap_remove(at);
        self.stats.quarantined += 1;
    }

    /// Returns one candidate after a non-fatal disposal failure
    /// (`blob_unlink_fail`): counted, re-offered next round with its
    /// origin intact; the boot sweep re-drives it after any crash
    /// (idempotent by construction).
    pub fn reclaim_deferred(&mut self, extent_id: u64) {
        let at = self.in_reclaim.iter().position(|&(id, _, _)| id == extent_id);
        let at = at.expect("reclaim_deferred for a candidate reclaim_work handed out");
        let (_, len, origin) = self.in_reclaim.swap_remove(at);
        self.stats.reclaim_deferred += 1;
        self.reclaimable.push_back(Reclaimable { extent_id, len, stamp: 0, origin });
    }

    /// The reference at `addr`, if that record stores out of line.
    #[must_use]
    pub fn reference_at(&self, addr: u64) -> Option<(u64, u64)> {
        self.refs.get(&addr).copied()
    }

    /// Live refcount of one extent (0 = unreferenced).
    #[must_use]
    pub fn refcount(&self, extent_id: u64) -> u64 {
        self.extents.get(&extent_id).map_or(0, |e| e.refs)
    }

    /// Reference-map entries strictly below `watermark`, in ascending
    /// address order — the checkpoint 0x05 emission set (cold entries
    /// only; resident extent records ride tag-9 images).
    pub fn entries_below(&self, watermark: u64) -> impl Iterator<Item = (u64, u64, u64)> + '_ {
        self.refs.range(..watermark).map(|(&addr, &(extent_id, len))| (addr, extent_id, len))
    }

    /// [`entries_below`](Self::entries_below) resumed at `resume` — the
    /// multi-slice walk's cursor form (review of 2026-08-30, C4 /
    /// F-L03-01): the resume is an **address**, so a removal below it
    /// (foreground DEL/overwrite, compaction relocate) between two
    /// slices cannot shift what the next slice sees — the ordinal
    /// `.skip(n)` resume it replaces stepped over one live entry per
    /// below-cursor removal, and the boot sweep then unlinked the
    /// never-emitted extent.
    pub fn entries_from(
        &self,
        resume: u64,
        watermark: u64,
    ) -> impl Iterator<Item = (u64, u64, u64)> + '_ {
        self.refs.range(resume..watermark).map(|(&addr, &(extent_id, len))| (addr, extent_id, len))
    }

    /// Counts a blob read-modify-write rewrite (ADR-0061 D7 — reserved
    /// seam; doc-tiering wires the caller).
    pub fn note_rmw(&mut self) {
        self.stats.rmw_ops += 1;
    }

    /// Point-in-time observable state. `disk_bytes` walks the maps —
    /// O(extents), control plane, once per scrape/round (the L5 "counted
    /// term" rule beats a shadow counter that can drift).
    #[must_use]
    pub fn stats(&self) -> ExtentStats {
        use inf_log::blob::extent_device_bytes;
        let mut stats = self.stats;
        stats.live = self.extents.len() as u64;
        stats.live_bytes = self.extents.values().map(|e| e.len).sum();
        stats.reclaimable =
            (self.parked.len() + self.reclaimable.len() + self.in_reclaim.len()) as u64;
        stats.disk_bytes = self.extents.values().map(|e| extent_device_bytes(e.len)).sum::<u64>()
            + self.parked.iter().map(|&(_, len)| extent_device_bytes(len)).sum::<u64>()
            + self.reclaimable.iter().map(|r| extent_device_bytes(r.len)).sum::<u64>()
            + self.in_reclaim.iter().map(|&(_, len, _)| extent_device_bytes(len)).sum::<u64>();
        stats
    }

    /// Total live reference-map entries (== live extent-carrying
    /// records; the L5 bound observable).
    #[must_use]
    pub fn reference_count(&self) -> usize {
        self.refs.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refcount_follows_the_reference_map_exactly() {
        // Goal: births increment, relocations move without touching the
        // count, deaths decrement, zero parks.
        let mut x = ExtentRefs::new();
        let e = x.allocate_id();
        x.register(100, e, 4096);
        assert_eq!(x.refcount(e), 1);
        assert!(x.relocate(100, 900));
        assert_eq!(x.refcount(e), 1, "relocation never changes the count");
        assert_eq!(x.note_death(100), None, "the old address no longer maps");
        assert_eq!(x.refcount(e), 1);
        assert_eq!(x.note_death(900), Some(e), "the real death zeroes the count");
        assert_eq!(x.refcount(e), 0);
        // Parked until a stamp; reclaimable only under a durable epoch.
        assert!(x.reclaim_work(u64::MAX, 8).is_empty(), "unstamped deaths never reclaim");
        x.stamp(7);
        assert!(x.reclaim_work(6, 8).is_empty(), "not durable yet");
        assert_eq!(
            x.reclaim_work(7, 8),
            vec![ReclaimCandidate { extent_id: e, origin: ReclaimOrigin::Death }]
        );
        x.reclaim_done(e);
        assert_eq!(x.stats().reclaimed, 1);
    }

    #[test]
    fn deferred_unlinks_reoffer_and_orphans_sweep() {
        let mut x = ExtentRefs::new();
        let e = x.allocate_id();
        x.register(50, e, 10);
        x.note_death(50);
        x.stamp(1);
        let death = ReclaimCandidate { extent_id: e, origin: ReclaimOrigin::Death };
        assert_eq!(x.reclaim_work(1, 8), vec![death]);
        x.reclaim_deferred(e);
        assert_eq!(
            x.reclaim_work(1, 8),
            vec![death],
            "a deferred candidate re-offers with its origin intact"
        );
        x.reclaim_done(e);
        // Orphans: listed on disk, referenced by nothing — typed
        // `BootOrphan`, so the plane quarantines them (ADR-0096 D2).
        let mut y = ExtentRefs::new();
        y.register(10, 3, 100);
        assert!(y.sweep_seed(&[2, 3, 9], &[]).is_empty(), "nothing to revive");
        let mut orphans = y.reclaim_work(0, 8);
        orphans.sort_unstable_by_key(|c| c.extent_id);
        assert_eq!(
            orphans,
            vec![
                ReclaimCandidate { extent_id: 2, origin: ReclaimOrigin::BootOrphan },
                ReclaimCandidate { extent_id: 9, origin: ReclaimOrigin::BootOrphan },
            ],
            "live extents never sweep; orphans carry boot provenance"
        );
        assert_eq!(y.allocate_id(), 10, "the cursor advanced past everything listed");
    }

    /// ADR-0096 D3/D4: a quarantined id the replayed map references is
    /// revived (and counted — the upstream-omission falsifier signal);
    /// one still unreferenced is the second verdict, unlinked through
    /// its own typed candidate.
    #[test]
    fn quarantined_ids_revive_when_referenced_and_unlink_on_the_second_verdict() {
        let mut x = ExtentRefs::new();
        x.register(10, 3, 100);
        let revive = x.sweep_seed(&[9], &[3, 7]);
        assert_eq!(revive, vec![3], "the referenced quarantined id revives");
        assert_eq!(x.stats().quarantine_revived, 1);
        let mut work = x.reclaim_work(0, 8);
        work.sort_unstable_by_key(|c| c.extent_id);
        assert_eq!(
            work,
            vec![
                ReclaimCandidate { extent_id: 7, origin: ReclaimOrigin::Quarantined },
                ReclaimCandidate { extent_id: 9, origin: ReclaimOrigin::BootOrphan },
            ],
            "second verdict + fresh boot orphan, each typed"
        );
        // The plane's dispositions: the fresh orphan quarantines (rename
        // — bytes survive), the second verdict unlinks its twin.
        x.reclaim_quarantined(9);
        x.reclaim_done(7);
        assert_eq!(x.stats().quarantined, 1);
        assert_eq!(x.stats().reclaimed, 1);
        assert_eq!(x.refcount(3), 1, "the revived extent's count is untouched");
        assert_eq!(x.allocate_id(), 10, "listed and quarantined names both advance the cursor");
    }

    #[test]
    fn duplicate_registration_counts_once() {
        // The walker's at-least-once duplicate collapses; a different
        // reference at the same address is a bug and panics.
        let mut x = ExtentRefs::new();
        x.register(7, 1, 100);
        x.register(7, 1, 100);
        assert_eq!(x.refcount(1), 1);
        assert_eq!(x.note_death(7), Some(1));
        assert_eq!(x.refcount(1), 0);
    }

    #[test]
    fn entries_below_filters_and_orders() {
        let mut x = ExtentRefs::new();
        x.register(300, 3, 30);
        x.register(100, 1, 10);
        x.register(200, 2, 20);
        let got: Vec<_> = x.entries_below(250).collect();
        assert_eq!(got, vec![(100, 1, 10), (200, 2, 20)], "ascending, watermark-filtered");
    }

    /// Review of 2026-08-30 (C4 / F-L03-01, F-L14-02): the checkpoint's
    /// 0x05 walk resumes across MAINTAIN slices while foreground DEL /
    /// overwrite / compaction mutate the reference map, so the resume
    /// must be stable under removals on either side of the cursor.
    /// `entries_from` (the address-cursor resume `tier_walk_step` pass 3
    /// composes) is; the ordinal `.skip(n)` resume it replaced stepped
    /// over one live entry per below-cursor removal — this exact
    /// schedule against `entries_below(MAX).skip(2)` emitted
    /// `[100, 200, 400, 500]`, silently missing live entry 300, and the
    /// boot sweep then unlinked its extent (the recorded falsifier run).
    #[test]
    fn mid_walk_death_below_the_cursor_never_hides_an_entry() {
        let mut x = ExtentRefs::new();
        for addr in [100u64, 200, 300, 400, 500] {
            x.register(addr, addr / 100, 10);
        }
        // Slice 1: the walk emits the first two entries, exactly as
        // `tier_walk_step` pass 3 does; the cursor is the last emitted
        // address + 1, never an ordinal.
        let mut emitted: Vec<(u64, u64, u64)> = x.entries_from(0, u64::MAX).take(2).collect();
        let cursor = emitted.last().expect("two entries staged").0 + 1;
        // Between slices: a DEL kills an entry *below* the cursor and an
        // overwrite kills one above it.
        assert_eq!(x.note_death(100), Some(1));
        assert_eq!(x.note_death(400), Some(4));
        // Slice 2 resumes at the address; every surviving entry that
        // existed the whole walk must still be emitted.
        emitted.extend(x.entries_from(cursor, u64::MAX));
        let got: Vec<u64> = emitted.iter().map(|&(addr, _, _)| addr).collect();
        assert_eq!(
            got,
            vec![100, 200, 300, 500],
            "a still-live pre-watermark entry was never emitted into any 0x05 section"
        );
    }
}
