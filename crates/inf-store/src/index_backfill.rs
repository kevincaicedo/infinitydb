//! Backfill state machine (M4.5-S05, ADR-0077): `INF.IDX CREATE` on a
//! populated namespace builds its tree in budgeted MAINTAIN slices —
//! resumable across slices, crash-restartable, never blocking foreground.
//!
//! The walk is the store's own resize-stable reverse-binary enumeration
//! (the `SCAN`/checkpoint-walker guarantee: at-least-once emission across
//! doubling growth and tombstone rehashes), and the S04 maintenance hook
//! is **always on** for the whole build — so the walk inserts what it
//! sees, the hook fixes what changes, and idempotent entry ops
//! (insert-if-absent / remove-if-present, ADR-0072 D5) make every
//! interleaving converge without membership decisions (ADR-0077 D1; the
//! case analysis is written out in `tests/index_backfill_storm.rs`).
//!
//! The watermark (the cursor on a [`BackfillJob`]) exists **only** for
//! resuming the next slice: it is never consulted for entry membership
//! (rehash moves table positions mid-walk — the M1 SCAN lesson), it is
//! never persisted, and a crash restarts the build from zero (ADR-0077
//! D2 — correct because ops are idempotent and `ready` only follows a
//! completed walk; the S06 sidecar, not a watermark, answers expensive
//! re-walks).

use inf_foundation::time::Nanos;
use inf_log::NsId;

use crate::index_registry::{IndexId, IndexState};
use crate::keyspace::Keyspace;
#[cfg(feature = "doc")]
use crate::ns::FIRST_NAMED_NS_ID;
#[cfg(feature = "doc")]
use crate::record::TypeTag;
#[cfg(feature = "doc")]
use crate::store::{CellStore, next_rev_cursor, record_at};

/// Budget for one backfill MAINTAIN tick (ADR-0077 D3). Both axes bound
/// the tick: `max_docs` caps evaluated documents (the cost driver — the
/// foreground-p99.9 co-gate is won by this bound riding the Maintenance
/// deficit class), `max_steps` caps home-group visits (bounded even when
/// the table is large and sparse).
#[derive(Copy, Clone, Debug)]
pub struct BackfillBudget {
    pub max_docs: u32,
    pub max_steps: u32,
}

impl Default for BackfillBudget {
    fn default() -> BackfillBudget {
        BackfillBudget { max_docs: 256, max_steps: 4096 }
    }
}

/// What one [`Keyspace::idx_backfill_tick`] did (feeds the plane's
/// Maintenance charge-back and the INFO progress lines).
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct BackfillTickStats {
    /// Documents evaluated (re-emissions included — harmless no-ops).
    pub docs_scanned: u64,
    /// Fresh tree inserts (idempotent re-inserts are not counted).
    pub entries_inserted: u64,
    /// Expired records reaped on encounter (the `SCAN` reap semantics).
    pub reaped: u64,
    /// Home groups visited.
    pub steps: u32,
    /// Jobs whose walk completed this tick (cell state now `Ready`).
    pub completed: u32,
    /// Jobs parked degraded this tick (ADR-0077 D7 — typed, counted).
    pub parked: u32,
    /// Walking jobs remaining after the tick.
    pub active: u32,
}

/// One build's observable phase (ADR-0077 D8).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum BackfillPhase {
    /// Slices are consuming the walk.
    Walking,
    /// Degraded mid-build — no slices, no convergence, no publication;
    /// rebuild resets (ADR-0077 D4/D7).
    Parked,
    /// This cell completed and reported ready; the row retires when the
    /// catalog flips `ready` (the fleet caught up).
    Published,
}

impl BackfillPhase {
    pub fn name(self) -> &'static str {
        match self {
            BackfillPhase::Walking => "walking",
            BackfillPhase::Parked => "parked",
            BackfillPhase::Published => "published",
        }
    }
}

/// One build's progress row (`INF.IDX LIST` renders these at S10; INFO
/// renders the fold now).
#[derive(Clone, Debug)]
pub struct BackfillProgress {
    pub ns: NsId,
    pub id: IndexId,
    pub generation: u64,
    pub phase: BackfillPhase,
    pub docs_scanned: u64,
    pub entries_inserted: u64,
}

/// The INFO-facing fold of the machine (cumulative per boot, like the
/// S04 `idx_*` counter lines — the recorded RESETSTAT deviation).
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct BackfillInfo {
    pub walking: u32,
    pub parked: u32,
    pub published: u32,
    pub docs_scanned_total: u64,
    pub entries_inserted_total: u64,
}

/// One per-index build on this cell (ADR-0077 D3: one job per
/// `(ns, id, generation)` — two concurrent builds are two cursors).
/// Volatile by design: boot re-derives jobs from the seeded registry.
#[derive(Debug)]
pub(crate) struct BackfillJob {
    ns: NsId,
    id: IndexId,
    generation: u64,
    /// The resume-only watermark (module docs; ADR-0077 D2). Slices are
    /// doc-gated, so the slim build never reads it.
    #[cfg_attr(not(feature = "doc"), allow(dead_code))]
    cursor: u64,
    phase: BackfillPhase,
    docs_scanned: u64,
    entries_inserted: u64,
}

/// What one store slice did (internal to the tick).
#[cfg(feature = "doc")]
#[derive(Copy, Clone, Debug, Default)]
pub(crate) struct BackfillSliceOutcome {
    docs: u32,
    inserted: u32,
    reaped: u32,
    steps: u32,
    done: bool,
    tripped: bool,
}

#[cfg(feature = "doc")]
impl CellStore {
    /// One budgeted walk slice for the backfilling index `id` (ADR-0077
    /// D1): resize-stable home-group enumeration from `cursor`, reaping
    /// expired records on encounter (the `SCAN` semantics — keeps the
    /// ADR-0076 D4 physical-view invariant so post-convergence death
    /// removals always find their entries), evaluating each live
    /// document through the attach block's idempotent insert. Returns
    /// the next cursor and the slice outcome; `done` means the cursor
    /// wrapped (walk complete), `tripped` means the index degraded
    /// (ADR-0077 D7) and the caller parks the build.
    ///
    /// Any partial (non-`done`, non-`tripped`) return has visited at
    /// least one home group — the tick's budget loop terminates on that
    /// guarantee.
    pub(crate) fn idx_backfill_slice(
        &mut self,
        id: IndexId,
        cursor: u64,
        max_docs: u32,
        max_steps: u32,
        now: Nanos,
    ) -> (u64, BackfillSliceOutcome) {
        debug_assert!(!self.idx.bracket_open(), "backfill slices never run inside a bracket");
        let mask = self.index.group_count() as u64 - 1;
        let mut cursor = cursor & mask;
        let mut out = BackfillSliceOutcome::default();
        let mut batch: Vec<inf_alloc::ArenaAddr> = Vec::new();
        loop {
            batch.clear();
            {
                let arena = &self.arena;
                self.index.scan_home_group(
                    cursor as usize,
                    |addr| CellStore::hash_key(record_at(arena, addr).key()),
                    |addr| batch.push(addr),
                );
            }
            for &addr in &batch {
                let view = record_at(&self.arena, addr);
                if view.is_expired(now) {
                    let (hash, len) = (CellStore::hash_key(view.key()), view.encoded_len());
                    self.free_record(hash, addr, len);
                    self.note_reap_lazy();
                    out.reaped += 1;
                    continue;
                }
                if view.type_tag() != TypeTag::JsonDoc {
                    continue; // strings never enter an index (sparse).
                }
                let (hash, len) = (CellStore::hash_key(view.key()), view.encoded_len());
                let max_matches = self.cfg.doc_max_path_matches;
                let CellStore { arena, docs, idx, .. } = self;
                let Some(root) = crate::doc::doc_root_at(arena, docs, addr, len) else {
                    debug_assert!(false, "a JsonDoc record always has a root");
                    continue;
                };
                match idx.backfill_insert_doc(id, hash, root, max_matches) {
                    Ok(fresh) => {
                        out.docs += 1;
                        out.inserted += fresh;
                    }
                    Err(()) => {
                        out.tripped = true;
                        return (cursor, out);
                    }
                }
            }
            out.steps += 1;
            cursor = next_rev_cursor(cursor, mask);
            if cursor == 0 {
                out.done = true;
                return (0, out);
            }
            if out.docs >= max_docs || out.steps >= max_steps {
                return (cursor, out);
            }
        }
    }
}

impl Keyspace {
    /// One budgeted backfill MAINTAIN tick (ADR-0077 D3): sync jobs from
    /// the registry (create / rebuild-reset / drop / park), then spend
    /// the budget on walking jobs, rotating at tick end so no job
    /// starves. **Serving cells only** — callers must not tick while
    /// boot replay runs (replay maintains nothing by default, ADR-0076
    /// D7, so a walk over a half-replayed store would go stale
    /// silently; the plane gates on recovery completion).
    ///
    /// Completion (ADR-0077 D4): store materialized → converged flag
    /// armed → cell machine `Ready` — publication to the `IndexBoard`
    /// is the plane's half (it owns the cell id and the board).
    pub fn idx_backfill_tick(&mut self, now: Nanos, budget: BackfillBudget) -> BackfillTickStats {
        let mut stats = BackfillTickStats::default();
        self.idx_backfill_sync(&mut stats);
        #[cfg(feature = "doc")]
        {
            let mut docs_left = budget.max_docs;
            let mut steps_left = budget.max_steps;
            while docs_left > 0 && steps_left > 0 {
                let Some(pos) =
                    self.backfill.iter().position(|j| matches!(j.phase, BackfillPhase::Walking))
                else {
                    break;
                };
                let (ns, id, cursor) = {
                    let job = &self.backfill[pos];
                    (job.ns, job.id, job.cursor)
                };
                // Materialize the owning store: an index on an unwritten
                // namespace still converges (the bracket's own
                // materialization precedent).
                let store = if ns.0 < FIRST_NAMED_NS_ID {
                    self.db_mut(ns.0 as usize)
                } else {
                    match self.ns_store_mut(ns) {
                        Some(store) => store,
                        None => {
                            // The namespace vanished; the registry sync
                            // will retire the entry — drop the job now.
                            self.backfill.remove(pos);
                            continue;
                        }
                    }
                };
                let (next, out) = store.idx_backfill_slice(id, cursor, docs_left, steps_left, now);
                stats.docs_scanned += u64::from(out.docs);
                stats.entries_inserted += u64::from(out.inserted);
                stats.reaped += u64::from(out.reaped);
                stats.steps += out.steps;
                docs_left = docs_left.saturating_sub(out.docs);
                // Any partial return visited ≥ 1 group (the slice's
                // stated guarantee) — this is what terminates the loop.
                steps_left = steps_left.saturating_sub(out.steps.max(1));
                let job = &mut self.backfill[pos];
                job.cursor = next;
                job.docs_scanned += u64::from(out.docs);
                job.entries_inserted += u64::from(out.inserted);
                if out.tripped {
                    job.phase = BackfillPhase::Parked;
                    stats.parked += 1;
                    continue;
                }
                if out.done {
                    job.phase = BackfillPhase::Published;
                    self.idx_set_converged(ns, id, true);
                    self.idx_registry_mut()
                        .set_cell_state(id, IndexState::Ready)
                        .expect("Backfilling → Ready is an ADR-0075 D3 edge");
                    stats.completed += 1;
                }
            }
            // Tick-granularity round-robin (ADR-0077 D3): rotate so the
            // next tick starts from the next job.
            if self.backfill.len() > 1 {
                self.backfill.rotate_left(1);
            }
        }
        #[cfg(not(feature = "doc"))]
        let _ = (now, budget);
        stats.active =
            self.backfill.iter().filter(|j| matches!(j.phase, BackfillPhase::Walking)).count()
                as u32;
        self.idx_backfill_note_totals(&stats);
        stats
    }

    /// Reconciles the job list against the registry — the one place
    /// jobs are created, reset, retired, or parked (ADR-0077 D3/D4):
    ///
    /// - entry gone / catalog `dropping` / generation moved (rebuild) ⇒
    ///   the job is dropped (a rebuild's fresh job restarts at zero —
    ///   old-generation evaluations must never enter a new-generation
    ///   tree, ADR-0077 D2);
    /// - `published` jobs retire once the catalog flips `ready`;
    /// - cell state `declared`/`backfilling` without a job ⇒ a job is
    ///   created, flipping `declared → backfilling` at both scopes (the
    ///   ADR-0077 D6 local start edge);
    /// - the `degraded` veto parks a walking job (set by the hook
    ///   between slices — the walk half observes it here).
    fn idx_backfill_sync(&mut self, stats: &mut BackfillTickStats) {
        let missing: Vec<(NsId, IndexId, u64, bool)> = {
            let (backfill, indexes) = self.backfill_and_registry_mut();
            backfill.retain(|job| {
                let Some(spec) = indexes.get_by_id(job.id) else { return false };
                if spec.state == IndexState::Dropping || spec.generation != job.generation {
                    return false;
                }
                if matches!(job.phase, BackfillPhase::Published) {
                    return spec.state != IndexState::Ready;
                }
                true
            });
            indexes
                .iter()
                .filter(|spec| spec.state != IndexState::Dropping)
                .filter(|spec| {
                    matches!(
                        indexes.cell_state(spec.id),
                        Some(IndexState::Declared | IndexState::Backfilling)
                    )
                })
                .filter(|spec| backfill.iter().all(|j| j.id != spec.id))
                .map(|spec| {
                    let declared = indexes.cell_state(spec.id) == Some(IndexState::Declared);
                    (spec.ns, spec.id, spec.generation, declared)
                })
                .collect()
        };
        for (ns, id, generation, declared) in missing {
            if declared {
                self.idx_registry_mut()
                    .set_cell_state(id, IndexState::Backfilling)
                    .expect("Declared → Backfilling is an ADR-0075 D3 edge");
                if self
                    .idx_registry()
                    .get_by_id(id)
                    .is_some_and(|s| s.state == IndexState::Declared)
                {
                    self.idx_registry_mut()
                        .set_catalog_state(id, IndexState::Backfilling)
                        .expect("Declared → Backfilling is an ADR-0075 D3 edge");
                }
            }
            self.backfill.push(BackfillJob {
                ns,
                id,
                generation,
                cursor: 0,
                phase: BackfillPhase::Walking,
                docs_scanned: 0,
                entries_inserted: 0,
            });
        }
        // The degraded veto parks walking jobs (typed + counted at the
        // trip site; the park itself is visible in the phase).
        let parked: Vec<usize> = self
            .backfill
            .iter()
            .enumerate()
            .filter(|(_, j)| matches!(j.phase, BackfillPhase::Walking))
            .filter(|(_, j)| self.idx_degraded(j.ns, j.id) == Some(true))
            .map(|(at, _)| at)
            .collect();
        for at in parked {
            self.backfill[at].phase = BackfillPhase::Parked;
            stats.parked += 1;
        }
    }

    /// Progress rows for every live build on this cell (ADR-0077 D8).
    pub fn idx_backfill_progress(&self) -> Vec<BackfillProgress> {
        self.backfill
            .iter()
            .map(|job| BackfillProgress {
                ns: job.ns,
                id: job.id,
                generation: job.generation,
                phase: job.phase,
                docs_scanned: job.docs_scanned,
                entries_inserted: job.entries_inserted,
            })
            .collect()
    }

    /// The INFO fold (phase counts + cumulative walk totals).
    pub fn idx_backfill_info(&self) -> BackfillInfo {
        let mut info = self.backfill_totals();
        for job in &self.backfill {
            match job.phase {
                BackfillPhase::Walking => info.walking += 1,
                BackfillPhase::Parked => info.parked += 1,
                BackfillPhase::Published => info.published += 1,
            }
        }
        info
    }

    /// The board slot for `id`: its rank among live catalog ids on this
    /// cell (ADR-0077 D5 — derived, never stored; every cell computes
    /// the same rank from the same replicated declaration set, and boot
    /// re-derives it from the seeded catalog). Always < `INDEX_SLOTS`.
    pub fn idx_slot_of(&self, id: IndexId) -> Option<usize> {
        let mut ids: Vec<u32> = self.idx_registry().iter().map(|spec| spec.id.0).collect();
        ids.sort_unstable();
        ids.iter().position(|&candidate| candidate == id.0)
    }

    /// `(slot, generation)` for every index this cell has completed —
    /// the plane publishes these to the `IndexBoard` **every** MAINTAIN
    /// tick (ADR-0077 D4: republication makes D5's rank drift
    /// self-healing).
    pub fn idx_ready_reports(&self) -> Vec<(usize, u64)> {
        self.idx_registry()
            .iter()
            .filter(|spec| spec.state != IndexState::Dropping)
            .filter(|spec| self.idx_registry().cell_state(spec.id) == Some(IndexState::Ready))
            .filter_map(|spec| self.idx_slot_of(spec.id).map(|slot| (slot, spec.generation)))
            .collect()
    }

    /// `(id, slot, generation)` for every catalog entry still
    /// `backfilling` — the plane's ADR-0077 D6 flip candidates: observe
    /// `fleet_ready(slot, generation)` and transition the local catalog
    /// entry to `ready`.
    pub fn idx_fleet_candidates(&self) -> Vec<(IndexId, usize, u64)> {
        self.idx_registry()
            .iter()
            .filter(|spec| spec.state == IndexState::Backfilling)
            .filter_map(|spec| {
                self.idx_slot_of(spec.id).map(|slot| (spec.id, slot, spec.generation))
            })
            .collect()
    }
}
