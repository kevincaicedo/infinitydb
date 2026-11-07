//! Boot-time per-cell log recovery (M2-S08/S11/S13/S14, ADR-0015 D7 +
//! ADR-0017 + ADR-0018): read `MANIFEST` (the atomic recovery-unit name),
//! load the checkpoint it names through the same `Keyspace::apply_record`
//! blind idempotent upsert as tail frames (ADR-0016 D1), replay the log
//! tail from `begin-LSN`, classify whatever lies beyond the last valid
//! frame (M2-S14), and reopen the tail segment for appending. Boot-time
//! blocking file I/O is the sanctioned exception to the cell denylist
//! (§3.3) and rides the injected `SegmentFs` seam for DST.
//!
//! Recovery-unit resolution (§8.4 — old or new, never neither, never
//! both-partial):
//! - No `MANIFEST` → replay the whole retained log (pre-checkpoint boots,
//!   fresh cells).
//! - `MANIFEST` present → it is the only authority: the named `.ick` must
//!   load (digest-verified) and the floor segment must exist — anything
//!   less is corruption and fail-stop, never a silent fallback to full
//!   replay (the segments below the floor may already be gone).
//! - Stale prefix segments (crash mid-truncation) and unnamed
//!   `.ick`/`.ick.new` orphans (crash between checkpoint publication and
//!   the MANIFEST swap) are garbage-collected here, before the cell
//!   serves.
//!
//! End-of-log policy (M2-S14, ADR-0018; ADR-0031 D4 as amended by
//! ADR-0087 D6): every segment's slack — the bytes beyond its last valid
//! frame — is scanned **right after that segment replays**. The first
//! slack holding a validating, self-located frame marks a **hole**: a
//! frame the writer queued below surviving data and the device lost. A
//! hole exists only if the barrier that would have covered those bytes
//! never completed, so the watermark never passed it and nothing at or
//! past it was acked — everything later is un-acked residue. The v2
//! stamps decide the one remaining question, whether the device is
//! lying: a later frame whose `covered_lsn` attests coverage at or past
//! the hole (or a v1 frame, which cannot attest) proves covered data was
//! lost — fail-stop with a named [`inf_log::LogCorruption`]. Otherwise
//! the tail *pointer* is truncated at the hole (bytes are never
//! rewritten), every later segment — probed for evidence, never replayed
//! — is removed to restore the pristine prealloc invariant, and the cell
//! continues under a fresh epoch. With no hole, the last data-bearing
//! segment's clean end is the resume point; non-validating remnants
//! behind it are the inert residue of an earlier torn-tail resume:
//! tolerated and counted, never fatal. A torn tail may never truncate
//! below the manifest's `begin-LSN`: everything the manifest names was
//! durable at publication, so a shorter log is disk lying.
//!
//! In the floor segment, records below `begin-LSN` are skipped: the
//! checkpoint already holds their final state, and replaying an older
//! post-image over it would regress entries whose last mutation preceded
//! `begin`. Recovery *throughput* gates bind at M2-S13/S22.
//!
//! M2-S15: recovery is a **resumable state machine** ([`Recovery`]) — the
//! plane drives it in bounded MAINTAIN steps so the cell answers
//! `-LOADING` while it replays (L6: boot is a budgeted task, not a
//! stop-the-world phase). [`open_cell_log`] is the same machine run to
//! completion in one call (tests, benches, MemFs tiers) — one code path,
//! so the S13 determinism sweep and the S14 taxonomy suite prove the
//! stepped machine too.

use std::io;
use std::path::PathBuf;

use inf_foundation::time::Nanos;
use inf_log::ckpt::{
    IckReader, IckReaderConfig, ick_file_name, parse_ick_file_name, read_ick_counts_probed,
};
use inf_log::fs::{SegmentFile, SegmentFs};
use inf_log::{
    BootDirs, FrameStamp, LogCorruption, Lsn, Manifest, ReadError, ReaderConfig, RegionEvidence,
    RegionScan, SegmentId, SegmentReader, SegmentRotor, SegmentScan, check_segment_len,
    create_cell_dirs, create_cell_dirs_deferred, read_manifest, scan_log_dir_from,
    scan_region_evidence, segment_file_name,
};
use inf_store::{Keyspace, ReplayOutcome, WallAnchor};

use crate::durable::DurableConfig;

mod frame_walk;
mod hand_over;
mod section_walk;
use frame_walk::cursor::FrameCursor;
use frame_walk::{WalkEnd, YieldKind};
use hand_over::{HandOverStage, ResumeFrom};

/// The recovery pipeline's phases as the outside sees them (M4.5-S39d):
/// what each [`Recovery::step`] is about to do, typed so the driver can
/// attribute the loop clock to it. Audit and probe share a phase — both
/// are the slack/evidence scan — exactly as the board's phase code does.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum RecoverPhase {
    /// Dirs, MANIFEST, directory scan, floor checks, checkpoint open.
    Start,
    /// Checkpoint sections streamed into the store.
    Ckpt,
    /// Tail frames replayed.
    Replay,
    /// Slack audit / evidence probe of a segment.
    Audit,
    /// Torn-tail resolution, begin guard, boot GC (stale files), rotor
    /// reopen.
    Finish,
    /// Nothing left: the next step reports completion.
    Complete,
}

impl RecoverPhase {
    /// The board's phase code (`CellRecoverySlot::phase_name`): 1 =
    /// start, 2 = checkpoint, 3 = replay, 4 = audit, 5 = finish, 6 =
    /// complete.
    #[must_use]
    pub const fn code(self) -> u8 {
        match self {
            RecoverPhase::Start => 1,
            RecoverPhase::Ckpt => 2,
            RecoverPhase::Replay => 3,
            RecoverPhase::Audit => 4,
            RecoverPhase::Finish => 5,
            RecoverPhase::Complete => 6,
        }
    }
}

/// Per-phase recovery accounting (M4.5-S39d, ADR-0090 A10): the bytes
/// each phase read and the time it took, so one boot's recovery figure
/// decomposes instead of being compared as a sum. Bytes are counted by
/// the machine itself; durations are credited by the driver from the
/// **injected loop clock** between consecutive steps
/// ([`Recovery::credit_phase_time`]) — cell code never reads an ambient
/// clock (L7). The synchronous tier (`open_cell_log`) has no clock and
/// leaves every duration zero. By construction every credited
/// nanosecond lands in exactly one phase, so `phase_ns` sums to
/// `total_ns` exactly — a property the e2e pins.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct RecoverPhases {
    pub start_ns: u64,
    /// Checkpoint bytes read (sections + header/footer) and load time.
    pub ckpt_bytes: u64,
    pub ckpt_ns: u64,
    /// Tail frame bytes read + validated, frames, and replay time.
    pub replay_bytes: u64,
    pub replay_frames: u64,
    pub replay_ns: u64,
    /// Slack/probe bytes the evidence scans read, the self-located and
    /// foreign-segment frames they CRC-validated, and scan time.
    pub audit_bytes: u64,
    pub audit_valid_frames: u64,
    pub audit_foreign_frames: u64,
    pub audit_ns: u64,
    /// Torn-tail resolution + boot GC + rotor reopen time (the stale
    /// files removed are `RecoverStats::stale_files_removed`).
    pub finish_ns: u64,
    /// First step to completion, loop-clock.
    pub total_ns: u64,
}

impl RecoverPhases {
    /// The phase that took longest (ties resolve in pipeline order) —
    /// the row's "dominating phase"; `None` before any time was credited.
    #[must_use]
    pub fn dominating(&self) -> Option<RecoverPhase> {
        let phases = [
            (RecoverPhase::Start, self.start_ns),
            (RecoverPhase::Ckpt, self.ckpt_ns),
            (RecoverPhase::Replay, self.replay_ns),
            (RecoverPhase::Audit, self.audit_ns),
            (RecoverPhase::Finish, self.finish_ns),
        ];
        let (phase, ns) =
            phases.iter().fold(phases[0], |best, &p| if p.1 > best.1 { p } else { best });
        (ns > 0).then_some(phase)
    }

    /// The durations in pipeline order (the sum-equals-total pin).
    #[must_use]
    pub const fn phase_ns(&self) -> [u64; 5] {
        [self.start_ns, self.ckpt_ns, self.replay_ns, self.audit_ns, self.finish_ns]
    }
}

/// What one cell's recovery did (log lines + `INFO persistence` inputs).
#[derive(Copy, Clone, Debug, Default)]
pub struct RecoverStats {
    /// M4.5-S39d: per-phase bytes + loop-clock durations.
    pub phases: RecoverPhases,
    pub segments: u64,
    pub frames: u64,
    pub records_applied: u64,
    pub records_skipped: u64,
    /// The subset of `records_skipped` whose namespace id no drop
    /// tombstone explains (ADR-0103 D4). After ADR-0103 a namespace is
    /// never servable before its definition is durable, so a nonzero
    /// count means a foreign log or corruption — the C14 verifier.
    pub records_skipped_unknown_ns: u64,
    /// Tail deltas already represented by a fuzzy checkpoint/full image
    /// (ADR-0043 R1). Separate from foreign-record skips.
    pub doc_deltas_skipped_stale: u64,
    /// Tail deltas whose key is absent because the fuzzy checkpoint
    /// captured a later delete/expiry (ADR-0043 R2).
    pub doc_deltas_skipped_missing: u64,
    /// Checkpoint-begin markers seen in the tail (counted so recovery
    /// output stays honest about them).
    pub markers: u64,
    /// Records loaded from the manifest-named checkpoint (M2-S11).
    pub ckpt_records: u64,
    /// Tail records below `begin-LSN` in the floor segment, skipped (the
    /// checkpoint supersedes them).
    pub records_pre_begin: u64,
    /// F-L03-03 (ADR-0028 A1): whether the checkpoint's presize counts
    /// came from the end-of-file footer probe (`Some(false)` = the
    /// dependent hop chain located the footer). `None` without a
    /// checkpoint.
    pub ckpt_footer_probe_hit: Option<bool>,
    /// Boot GC: stale below-floor segments + unnamed `.ick`/`.ick.new`
    /// orphans removed.
    pub stale_files_removed: u64,
    /// M2-S14: `Some` when a torn final write was truncated — the (segment,
    /// offset) the log resumes at. The dropped bytes were never covered by
    /// a completed fsync, so nothing acked is lost (§8.2); callers log it.
    pub torn_truncated_at: Option<Lsn>,
    /// Trailing segments removed with a torn tail (each verified to hold
    /// no validating frame before deletion).
    pub torn_segments_removed: u64,
    /// M4.5-S37 (ADR-0093 A4): rebuilt shadow slots the boot read and
    /// settled by their full key — the ambiguous (two RAM keys with one
    /// hash) and over-cap pairs, never the general index.
    pub shadow_settle_reads: u64,
    /// ADR-0096 D3: quarantined extents the replayed map references,
    /// renamed back before serving — a wrong orphan verdict healed.
    /// Nonzero is the upstream-accounting falsifier signal; logged.
    pub blob_revived: u64,
    /// ADR-0100 D6: tombstoned namespaces whose residue this boot swept
    /// (a `MANIFEST` tier section, an `ns-N/cold` directory, or both),
    /// the files unlinked doing it, and the checkpoint's tiered sections
    /// (ref / live-set / blob-ref) skipped for them.
    pub dropped_ns_swept: u64,
    pub dropped_files_swept: u64,
    pub dropped_sections_skipped: u64,
    /// Sealed segments with non-validating remnant bytes behind their data
    /// end — the inert residue of an earlier torn-tail resume (the reader
    /// never crosses a segment's data end, so they can never replay).
    /// Tolerated, but counted: never silent (§8.4).
    pub sealed_slack_remnants: u64,
    /// M2.5-S12 (ADR-0031 D4, ADR-0087 D6): validating frames beyond a
    /// hole that the stamp evidence proved un-covered (no surviving
    /// attestation reaches them) — discarded with the torn tail instead
    /// of refusing. The retired ADR-0021 D3 refusal class, counted, never
    /// silent. Since ADR-0087 D6 this includes frames in later segments
    /// behind a hole in a sealed one (probed, never replayed).
    pub beyond_frames_discarded: u64,
    /// Epoch-regressed frames ending replay early (discarded-life residue
    /// resurfacing at the data end — ADR-0031 D3/D5). Counted per boot.
    pub epoch_residue_stops: u64,
    /// Segment slacks holding **validating discarded-life residue** —
    /// frames of an earlier life left beyond a data end that a torn-tail
    /// resume truncated the pointer at and never rewrote (ADR-0031 D4's
    /// "bytes are never rewritten"), then sealed past by a later life
    /// that rotated away before overwriting them (the ADR-0086 D4
    /// class-upgrade rotation makes this routine). Proven residue by
    /// epoch — below the replayed prefix's, or below a life observed
    /// beyond it — never a hole: excluded from the attestation decision,
    /// replay continues past it. Counted per boot (ADR-0031 D5 as amended
    /// 2026-08-21).
    pub stale_residue_slacks: u64,
    /// M4.5-S39b (ADR-0090 D2 as amended): replay ended a segment's data
    /// at a **foreign-segment frame** — a decoded frame at its stored
    /// offset but stamped for another segment id, the residue a recycled
    /// file carries from its previous life. A classified end like a zero
    /// tail (replay continues in the next segment), never data, never a
    /// hole. Counted per boot.
    pub segment_residue_stops: u64,
    /// Segment slacks proven **recycled-life residue** by the audit: no
    /// self-located frame, ≥ 1 foreign-segment frame (garbage beside it
    /// allowed — a torn tail over residue is indistinguishable from
    /// residue and nothing acked can sit past a data end). Never torn,
    /// never a hole; trailing ones are removed as stale. Counted per boot.
    pub recycled_residue_slacks: u64,
    /// Boot replay of the tiered namespaces (ADR-0174 D6), per cell.
    pub tier_replay: TierReplayStats,
    /// The step facts (ADR-0174 A1.2), per cell: the largest work one step
    /// did and the liveness counters `INFO persistence` folds.
    pub step: StepFacts,
    /// The budget asks the steps made, per cell (not in `INFO`): one per
    /// call of `StepBudget::spent` or `StepBudget::left`. At `u64::MAX`, on
    /// a unit that demotes nothing, the replay and checkpoint phases ask
    /// once per frame and once per section they enter (an index-sidecar
    /// section's ask comes after it).
    pub budget_asks: u64,
    /// The units of work with no I/O of their own the steps ran, per cell
    /// (not in `INFO`).
    pub units: RecoverUnits,
}

/// The recovery units a cell's steps ran that do no I/O of their own,
/// counted by the engine as it does them and kept apart from the charge
/// (ADR-0174 A1): what a step judge prices at their `limits` price. Per
/// cell, as [`RecoverStats`].
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct RecoverUnits {
    /// Checkpoint entries applied: refs, live-set and blob-reference
    /// entries (a tombstoned namespace's skipped ones are not).
    pub ckpt_entries: u64,
    /// Listed addresses the end of the checkpoint walked, each released or
    /// found with no blob-reference entry (ADR-0174 R9).
    pub ckpt_end_releases: u64,
    /// Parked replay deaths the hand-over's sweep seed stamped durable.
    pub sweep_stamped: u64,
    /// Listed and quarantined extent ids the sweep seed seeded.
    pub sweep_seeded: u64,
    /// Probe groups ADR-0093's ticket rebuild scanned.
    pub rebuild_groups: u64,
}

/// The step facts of one cell's recovery (ADR-0174 A1.2), outside D6's
/// zero set: the gauge of the largest work one step did — what it charged
/// (the bytes its cursor passed and its priced boot work) plus the bytes of
/// the frame or section it entered and parked in, in budget bytes, never
/// time and never memory — and the liveness counters, the steps taken and
/// the yields inside a unit by kind. A fitting boot moves `steps` and can move the
/// frame and section kinds; its demote-step kind reads zero.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct StepFacts {
    /// The gauge: the largest work of one step, in budget bytes.
    pub work_bytes_max: u64,
    /// Steps that ran a phase.
    pub steps: u64,
    /// Steps that yielded inside a log frame.
    pub yields_in_frame: u64,
    /// Steps that yielded inside a checkpoint section.
    pub yields_in_section: u64,
    /// Steps that yielded inside a demote step's seal walk.
    pub yields_in_demote_step: u64,
}

/// [`StepFacts`]' fields in `INFO` order, the one order the recovery board
/// stores them in.
pub const STEP_FACT_FIELDS: usize = 5;

/// One `INFO persistence` line of the step facts' node fold: its name and
/// the field it prints and the recovery board restores.
pub type StepFactField = (&'static str, fn(&mut StepFacts) -> &mut u64);

impl StepFacts {
    /// The one table of the node fold's lines, in board order: the
    /// `recover_node_` prefix names the population, every cell's recovery
    /// folded — the gauge the largest over the cells, the counters summed.
    pub const FIELDS: [StepFactField; STEP_FACT_FIELDS] = [
        ("recover_node_step_work_bytes_max", |s| &mut s.work_bytes_max),
        ("recover_node_steps", |s| &mut s.steps),
        ("recover_node_yields_in_frame", |s| &mut s.yields_in_frame),
        ("recover_node_yields_in_section", |s| &mut s.yields_in_section),
        ("recover_node_yields_in_demote_step", |s| &mut s.yields_in_demote_step),
    ];

    /// The fields in [`FIELDS`](Self::FIELDS) order.
    #[must_use]
    pub fn to_array(self) -> [u64; STEP_FACT_FIELDS] {
        let mut facts = self;
        std::array::from_fn(|i| *(Self::FIELDS[i].1)(&mut facts))
    }

    /// The inverse of [`to_array`](Self::to_array).
    #[must_use]
    pub fn from_array(values: [u64; STEP_FACT_FIELDS]) -> StepFacts {
        let mut facts = StepFacts::default();
        for ((_, field), value) in Self::FIELDS.iter().zip(values) {
            *field(&mut facts) = value;
        }
        facts
    }

    /// The fold of two cells: the gauge the larger, the counters summed.
    #[must_use]
    pub fn fold(self, other: StepFacts) -> StepFacts {
        StepFacts {
            work_bytes_max: self.work_bytes_max.max(other.work_bytes_max),
            steps: self.steps.saturating_add(other.steps),
            yields_in_frame: self.yields_in_frame.saturating_add(other.yields_in_frame),
            yields_in_section: self.yields_in_section.saturating_add(other.yields_in_section),
            yields_in_demote_step: self
                .yields_in_demote_step
                .saturating_add(other.yields_in_demote_step),
        }
    }
}

/// The recovery step budget (ADR-0174 A1), built by [`Recovery::step`] from
/// its `budget_bytes` and lent to the phase it runs: the one holder of the
/// budget and of what the step has charged, so every yield test of
/// recovery is an ask of it, counted. A charge is budget bytes — the bytes
/// a cursor passed and boot work at fixed prices — never elapsed time and
/// never memory.
#[derive(Debug)]
pub struct StepBudget {
    budget: u64,
    charged: u64,
    asks: u64,
}

impl StepBudget {
    fn new(budget: u64) -> StepBudget {
        StepBudget { budget, charged: 0, asks: 0 }
    }

    /// Adds `bytes` to the step's charge (saturating: a yield test, never
    /// an account that balances).
    fn charge(&mut self, bytes: u64) {
        self.charged = self.charged.saturating_add(bytes);
    }

    /// An ask: whether the step is spent — its charge reached the budget
    /// and something was charged, so a step with work to do completes a
    /// unit at any budget, zero included.
    fn spent(&mut self) -> bool {
        self.asks = self.asks.saturating_add(1);
        self.charged > 0 && self.charged >= self.budget
    }

    /// An ask: the budget less the charge.
    fn left(&mut self) -> u64 {
        self.asks = self.asks.saturating_add(1);
        self.budget.saturating_sub(self.charged)
    }

    /// What the step has charged.
    fn charged(&self) -> u64 {
        self.charged
    }
}

/// Which recovery phase a step ran, as [`RECOVERY_UNITS`] rows it — finer
/// than the five board codes, which cannot tell the end settle from the
/// finish step.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum UnitPhase {
    Start,
    Ick,
    CkptEnd,
    Replay,
    Audit,
    Probe,
    Finish,
    Settle,
    HandOver,
    Resume,
    Complete,
}

/// The phases [`RECOVERY_UNITS`] rows.
pub const UNIT_PHASES: usize = 11;

impl UnitPhase {
    /// The phase's row in [`RECOVERY_UNITS`].
    #[must_use]
    pub const fn index(self) -> usize {
        match self {
            UnitPhase::Start => 0,
            UnitPhase::Ick => 1,
            UnitPhase::CkptEnd => 2,
            UnitPhase::Replay => 3,
            UnitPhase::Audit => 4,
            UnitPhase::Probe => 5,
            UnitPhase::Finish => 6,
            UnitPhase::Settle => 7,
            UnitPhase::HandOver => 8,
            UnitPhase::Resume => 9,
            UnitPhase::Complete => 10,
        }
    }
}

/// What a step of one phase runs between two asks of its budget.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum UnitEntry {
    /// A unit read and audited whole on entry: a log frame.
    Frame,
    /// A checkpoint section, read and audited whole on entry.
    Section,
    /// No entered unit.
    None,
}

/// One phase's row: its unit, the entry it reads whole, and whether the
/// step budget bounds it. A row that is not budgeted charges and parks
/// nothing, so the work gauge never reads it.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct RecoveryUnit {
    pub phase: UnitPhase,
    pub entry: UnitEntry,
    pub budgeted: bool,
    pub unit: &'static str,
}

/// The recovery units (ADR-0174 A1), one row per [`UnitPhase`], in its
/// order: the table the driver charges by and a step judge iterates. The
/// array is sized by the phase count and each row is pinned to its phase's
/// index, so a phase added without its row does not compile.
pub const RECOVERY_UNITS: [RecoveryUnit; UNIT_PHASES] = [
    RecoveryUnit {
        phase: UnitPhase::Start,
        entry: UnitEntry::None,
        budgeted: false,
        unit: "directories, MANIFEST, the segment scan and the tier map: one step",
    },
    RecoveryUnit {
        phase: UnitPhase::Ick,
        entry: UnitEntry::Section,
        budgeted: true,
        unit: "one record with its settle reads, one checkpoint entry, one demote step's flush, \
               or one index-sidecar section whole",
    },
    RecoveryUnit {
        phase: UnitPhase::CkptEnd,
        entry: UnitEntry::None,
        budgeted: true,
        unit: "one listed address the end of the checkpoint releases",
    },
    RecoveryUnit {
        phase: UnitPhase::Replay,
        entry: UnitEntry::Frame,
        budgeted: true,
        unit: "one record with its settle reads, or one demote step's flush",
    },
    RecoveryUnit {
        phase: UnitPhase::Audit,
        entry: UnitEntry::None,
        budgeted: false,
        unit: "one segment's slack scan",
    },
    RecoveryUnit {
        phase: UnitPhase::Probe,
        entry: UnitEntry::None,
        budgeted: false,
        unit: "one segment's evidence probe",
    },
    RecoveryUnit {
        phase: UnitPhase::Finish,
        entry: UnitEntry::None,
        budgeted: false,
        unit: "the lift decision and the unpaired-marker check",
    },
    RecoveryUnit {
        phase: UnitPhase::Settle,
        entry: UnitEntry::None,
        budgeted: true,
        unit: "one end-settled record with its exact-hash group's reads",
    },
    RecoveryUnit {
        phase: UnitPhase::HandOver,
        entry: UnitEntry::None,
        budgeted: true,
        unit: "one parked replay death stamped, one listed or quarantined extent id seeded, one \
               revive rename, one probe group the ticket rebuild scans or one slot it returns \
               settled, or one machine's hand-over with its drain's seal",
    },
    RecoveryUnit {
        phase: UnitPhase::Resume,
        entry: UnitEntry::None,
        budgeted: false,
        unit: "the sidecar commit, the log's resume point, the boot GC and the rotor, or a fresh \
               cell's first segment: one step",
    },
    RecoveryUnit {
        phase: UnitPhase::Complete,
        entry: UnitEntry::None,
        budgeted: false,
        unit: "nothing",
    },
];

const _: () = {
    let mut i = 0;
    while i < UNIT_PHASES {
        assert!(RECOVERY_UNITS[i].phase.index() == i, "each row sits at its phase's index");
        i += 1;
    }
};

/// What boot replay of the tiered namespaces did on one cell (ADR-0174
/// D6): the replay machines' counters summed over the cell's recovered
/// tiered namespaces, D4's dead-life tier files removed, and the largest
/// boot I/O charge one recovery step took. The zero set — every counter
/// but `counters.markers_skipped` — is zero on a boot that did not demote;
/// outside it are the markers skipped, `dead_life_files_removed`, and the
/// gauge, which also prices the settle reads of ADR-0093's rebuild on any
/// boot. `INFO persistence` renders the node fold of these
/// ([`TierReplayStats::fold`]).
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct TierReplayStats {
    /// The machines' counters, summed over the cell's namespaces.
    pub counters: inf_store::ReplayCounters,
    /// Tier files no manifest section named, removed before any flush
    /// (ADR-0174 D4) — outside the zero set.
    pub dead_life_files_removed: u64,
    /// The gauge: the largest boot I/O charge one recovery step took, in
    /// step-budget bytes (`boot_io_charge`).
    pub step_charge_bytes_max: u64,
}

/// [`TierReplayStats`]' fields in `INFO` order, the one order the
/// recovery board stores them in.
pub const TIER_REPLAY_FIELDS: usize = 13;

/// One `INFO persistence` line of the node fold: its name, and the field
/// of [`TierReplayStats`] it prints and the recovery board restores.
pub type TierReplayField = (&'static str, fn(&mut TierReplayStats) -> &mut u64);

impl TierReplayStats {
    /// The one table of the node fold's `INFO persistence` lines, in board
    /// order, each name beside the field it reads: the `recover_node_tier_`
    /// prefix names the population — every cell's recovered tiered
    /// namespaces, summed; the step-charge gauge is the largest over the
    /// cells.
    pub const FIELDS: [TierReplayField; TIER_REPLAY_FIELDS] = [
        ("recover_node_tier_demote_steps", |s| &mut s.counters.demote_steps),
        ("recover_node_tier_pads_placed", |s| &mut s.counters.pads_placed),
        ("recover_node_tier_bytes_written", |s| &mut s.counters.tier_bytes),
        ("recover_node_tier_barriers", |s| &mut s.counters.barriers),
        ("recover_node_tier_files_sealed", |s| &mut s.counters.files_sealed),
        ("recover_node_tier_settle_reads", |s| &mut s.counters.settle_reads),
        ("recover_node_tier_settled_same_key", |s| &mut s.counters.settled_same_key),
        ("recover_node_tier_settled_distinct", |s| &mut s.counters.settled_distinct),
        ("recover_node_tier_deletes_verified", |s| &mut s.counters.deletes_verified),
        ("recover_node_tier_blob_releases", |s| &mut s.counters.blob_releases),
        ("recover_node_tier_markers_skipped", |s| &mut s.counters.markers_skipped),
        ("recover_node_tier_dead_life_files_removed", |s| &mut s.dead_life_files_removed),
        ("recover_node_tier_step_charge_bytes_max", |s| &mut s.step_charge_bytes_max),
    ];

    /// The fields in [`FIELDS`](Self::FIELDS) order.
    #[must_use]
    pub fn to_array(self) -> [u64; TIER_REPLAY_FIELDS] {
        let mut stats = self;
        std::array::from_fn(|i| *(Self::FIELDS[i].1)(&mut stats))
    }

    /// The inverse of [`to_array`](Self::to_array).
    #[must_use]
    pub fn from_array(values: [u64; TIER_REPLAY_FIELDS]) -> TierReplayStats {
        let mut stats = TierReplayStats::default();
        for ((_, field), value) in Self::FIELDS.iter().zip(values) {
            *field(&mut stats) = value;
        }
        stats
    }

    /// The fold of two cells (or namespaces): counters sum, the gauge
    /// takes the larger.
    #[must_use]
    pub fn fold(mut self, other: TierReplayStats) -> TierReplayStats {
        self.counters.absorb(other.counters);
        self.dead_life_files_removed =
            self.dead_life_files_removed.saturating_add(other.dead_life_files_removed);
        self.step_charge_bytes_max = self.step_charge_bytes_max.max(other.step_charge_bytes_max);
        self
    }
}

/// The recovered manifest — hands the recovery-time floor + named
/// checkpoint to [`ServerPlane::enable_durable`](crate::ServerPlane) so
/// the truncation slice resumes where the last boot left off.
#[derive(Copy, Clone, Debug)]
pub struct RecoveredManifest {
    pub ckpt_id: u64,
    pub begin_lsn: Lsn,
}

/// Opens (or creates) cell `cell`'s log under the node data dir, loading
/// the manifest-named checkpoint (if any) and replaying the tail into
/// `ks`. Returns the rotor positioned at the tail, ready for
/// `begin_frame_deferred`, plus the recovered manifest seed.
///
/// `fs` is the injected filesystem seam (§3.3): `StdSegmentFs` on the
/// node, `MemFs` in the recovery test tiers, the sim disk at M2-S18.
///
/// # Errors
/// Scan errors, manifest/checkpoint corruption, interior log corruption
/// (M2-S14 — a validating frame beyond a corrupt region fail-stops; only
/// a torn *final* write is truncated and survived), read errors, store
/// apply failures, and rotor open failures — all fail-stop at boot (§8.4).
pub fn open_cell_log<F: SegmentFs + Clone>(
    fs: F,
    ks: &mut Keyspace,
    cell: u16,
    cfg: &DurableConfig,
    anchor: WallAnchor,
    now: Nanos,
) -> io::Result<(SegmentRotor<F>, RecoverStats, Option<RecoveredManifest>)> {
    let mut recovery = Recovery::new(fs, cell, cfg, anchor, now);
    while recovery.step(ks, u64::MAX)? == RecoveryProgress::Working {}
    Ok(recovery.finish())
}

use crate::readahead::{ReadAheadFile, ReadAheadFs};

/// A boot reader's file: the bare tier's file behind the boot-read
/// prefetch wrapper (ADR-0109). Only [`Recovery`]'s readers hold these;
/// the rotor, tier handles and barrier dirs stay `F::File`.
type BootFile<F> = ReadAheadFile<<F as SegmentFs>::File>;

/// Continuity verdict for one validating frame's stamp (ADR-0031 D3).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum StampVerdict {
    /// Continuity holds — apply the frame's records.
    Replay,
    /// Epoch regressed: discarded-life residue at the data end — replay
    /// ends here, the audit owns the rest.
    ResidueStop,
}

/// What one [`Recovery::step`] call reports.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum RecoveryProgress {
    /// More work remains — call `step` again.
    Working,
    /// Recovery is complete: call [`Recovery::finish`].
    Complete,
}

/// Recovery phase (each `step` performs one bounded unit of the phase).
enum Phase<File: SegmentFile> {
    /// Dirs, MANIFEST, scan, floor checks, checkpoint open + presize —
    /// bounded by directory-entry counts, one step.
    Start,
    /// Checkpoint sections streamed under the step budget, a step yielding
    /// at the next record or entry once its charge reaches the budget, inside
    /// a section the reader holds (ADR-0174 A1).
    Ick {
        reader: Box<IckReader<File>>,
    },
    /// The end of the checkpoint's releases (ADR-0174 R9), under the step
    /// budget: `at` the next listed address of the machine at index `tier`
    /// of the recovered tiered namespaces, whose lists closed with the
    /// checkpoint, so an index into one names one address.
    CkptEnd {
        tier: usize,
        at: usize,
    },
    /// Tail frames replayed under the step budget, one segment at a time,
    /// a step yielding at the next record once its charge reaches the
    /// budget (ADR-0174 A1).
    Replay {
        idx: usize,
        reader: ReplayReader<File>,
    },
    /// M2-S14 slack audit of segment `idx`, right after its replay
    /// (ADR-0087 D6); one segment per step.
    Audit {
        idx: usize,
    },
    /// A hole was found in an earlier segment: segment `idx` is scanned
    /// whole for stamp evidence and never replayed (ADR-0087 D6); one
    /// segment per step.
    Probe {
        idx: usize,
    },
    /// The lift decision; once declined, replay has ended: the unpaired-
    /// marker check, then each tiered machine's end of replay.
    Finish,
    /// The end-of-replay settle of every tiered namespace that demoted
    /// (ADR-0174 R10), one budgeted step at a time; then the hand-over.
    Settle,
    /// The hand-over (ADR-0174 R10's last half), each tiered namespace in
    /// turn under the step budget, a step yielding at the next unit once its
    /// charge reaches the budget: `stage` is where the next machine to hand
    /// over stands, `then` what follows the last one.
    HandOver {
        stage: HandOverStage,
        then: ResumeFrom,
    },
    /// The sidecar commit, then the torn-tail resolution, begin guard,
    /// boot GC and rotor reopen — or a fresh cell's first segment; one step.
    Resume {
        from: ResumeFrom,
    },
    Complete,
}

/// A replay segment's reader: not opened yet, or open with where the walk
/// stands in it — at a frame boundary, or inside the frame the reader holds,
/// at a cursor (the cursor lives only beside the reader that holds its
/// frame).
enum ReplayReader<File: SegmentFile> {
    Unopened,
    Open { reader: Box<SegmentReader<File>>, at: FramePos },
}

/// Where a replay step resumes in an open segment.
enum FramePos {
    /// At the next frame's entry.
    Boundary,
    /// Inside the held frame, at the cursor.
    InFrame(FrameCursor),
}

/// Boot recovery as a resumable state machine (M2-S15). Construction is
/// cheap; every I/O happens inside [`step`](Self::step), each call bounded
/// by `budget_bytes` in charged work plus one unit and the frame or section
/// it entered and parked in (ADR-0174 A1). The phases and their semantics are exactly
/// [`open_cell_log`]'s documented pipeline — that function *is* this
/// machine run to completion.
pub struct Recovery<F: SegmentFs> {
    /// The bare tier: dirs, the size probe, tier handles, the rotor —
    /// everything that outlives boot is opened here.
    fs: Option<F>,
    /// The boot readers' tier (ADR-0109): MANIFEST, checkpoint stream,
    /// segment replay and the slack audit open through the prefetch
    /// wrapper; it is constructed here and dies with this machine.
    boot_reads: ReadAheadFs<F>,
    cell: u16,
    cfg: DurableConfig,
    anchor: WallAnchor,
    now: Nanos,
    phase: Phase<BootFile<F>>,
    stats: RecoverStats,
    shard_dir: PathBuf,
    log_dir: PathBuf,
    ckpt_dir: PathBuf,
    manifest: Option<Manifest>,
    begin_lsn: Option<Lsn>,
    floor: SegmentId,
    scan: Option<SegmentScan>,
    stale: Vec<SegmentId>,
    segments: Vec<SegmentId>,
    /// Data extent per segment (file size — sparse prealloc makes this an
    /// upper bound; progress credits the slack when a segment completes).
    seg_sizes: Vec<u64>,
    ends: Vec<(u32, Option<ReadError>)>,
    /// The cell's cap census (ADR-0163 D2): a recovered tier pipeline's
    /// capped holders register their rows on it, so they stand on the
    /// plane's census once the pipeline is installed. A standalone
    /// recovery (tests, the offline open) builds its own.
    census: std::rc::Rc<inf_foundation::bounded::CapCensus>,
    residue: Vec<bool>,
    /// Per segment: its slack's validating frames are discarded-life
    /// residue (ADR-0031 D5 as amended) — counted as remnants, excluded
    /// from the hole/attestation decision.
    stale_slack: Vec<bool>,
    bytes_total: u64,
    bytes_done: u64,
    /// Bytes actually read+validated (frames, checkpoint blocks) — the
    /// throttle currency. `bytes_done` additionally credits skipped
    /// prealloc slack, which is bookkeeping, not I/O.
    bytes_consumed: u64,
    /// M2.5-S01: loop-resident boots defer every boot-metadata fsync off
    /// the ready path — dir creation and the fresh segment happen
    /// unsynced, and the open dir handles below become driver-ridden
    /// boot barriers at the head of the group-commit ledger. The
    /// synchronous tier ([`open_cell_log`]) keeps blocking syncs: it has
    /// no driver to ride.
    defer_boot_sync: bool,
    barrier_dirs: Option<BootDirs<F::File>>,
    /// Checkpoint bytes already credited to `bytes_done` (section blocks);
    /// the header/footer remainder is credited when the footer validates.
    ick_credited: u64,
    /// M2.5-S12 (ADR-0031 D3): the last v2 stamp replayed — the prefix
    /// continuity state (epoch monotone; seq +1 within an epoch, 1 at an
    /// epoch step; covered_lsn nondecreasing within an epoch).
    prev_stamp: Option<FrameStamp>,
    /// A v2 frame was replayed: a later v1 frame in the prefix is a named
    /// refusal (append order makes it unreachable — ADR-0031 D2).
    saw_v2: bool,
    /// Replay ended at an epoch-regressed frame (discarded-life residue —
    /// ADR-0031 D5): remaining segments are residue, never replayed.
    residue_stop: bool,
    /// Highest epoch observed in the valid prefix (the ADR-0031 D5
    /// derivation input, joined with the audit's beyond-frame evidence).
    max_prefix_epoch: u32,
    /// Stamp evidence per segment's slack (whole segment once probed),
    /// gathered by the audit/probe steps (ADR-0031 D4, ADR-0087 D6); the
    /// Finish phase aggregates it from the resume point on.
    evidence: Vec<RegionEvidence>,
    /// Index of the first segment whose slack held a validating frame —
    /// the hole (ADR-0087 D6). Every later segment is probed, never
    /// replayed.
    hole: Option<usize>,
    /// Index of the resume segment, fixed in the Finish phase: the hole's
    /// segment when it holds data, else the last data-bearing segment.
    last_data: usize,
    finished: Option<(SegmentRotor<F>, Option<RecoveredManifest>)>,
    /// Recovered tiered namespaces during replay (ADR-0174): each one's
    /// boot replay machine, holding its pipeline and the creation-mode
    /// handles, plus the extent sweep seed — every tiered namespace of the
    /// catalog (D4). Lent to `Keyspace::apply_record` as the replay seam;
    /// handed over at the end of replay into `recovered_tiers`.
    recovering_tiers: RecoveringTiers<F>,
    /// The machines the hand-over has not handed over yet, the next one last
    /// (namespace order reversed, so each leaves by a pop): moved out of the
    /// replay seam once replay has ended, so no record is lent one again.
    handing_over: Vec<RecoveringTierNs<F>>,
    /// The boot I/O the replay machines did inside the current step, in
    /// bytes of the recovery step budget (ADR-0174 R10; D6's gauge reads
    /// its largest): the barriers, the tier bytes, the settle reads, the
    /// bytes the seal and end settle walks passed. Drained where the step
    /// asks its budget — after a priced record, a demote step, a frame, a
    /// checkpoint section and a settle step — and charged to it there.
    step_charge: u64,
    /// The bytes of the frame or section the current step entered and parked
    /// in, not charged yet: the rest of the step's work (ADR-0174 A1.2). A
    /// unit whose price is not its bytes parks its bytes, never its price.
    step_parked_bytes: u64,
    /// The row of [`RECOVERY_UNITS`] the last step ran.
    last_unit: UnitPhase,
    /// The work the last step did ([`Recovery::step_work_bytes`]).
    step_work: u64,
    /// Recovered tiered namespaces' plane half (M4-S26, ADR-0057 D6):
    /// flush pipeline + open sealed-file handles + the extent sweep
    /// seed — installed into the plane's tier state at completion.
    recovered_tiers: Vec<RecoveredTierNs<F>>,
    /// Replay has ended: set once by `end_replay`, at the finish step
    /// that declines the lift; its debug assertion that replay ends once
    /// reads it.
    tier_replay_checked: bool,
    /// Sidecar boot loader (M4.5-S06, ADR-0078 D6): consumes tag-0x06
    /// sections during the ick phase, arms `CatchUp` for the tail, and
    /// commits loaded trees at end of replay. Taken (once) on entering
    /// the audit — the commit point.
    #[cfg(feature = "doc")]
    sidecar: Option<inf_store::SidecarLoader>,
}

/// One recovered tiered namespace while replay runs: its boot machine
/// (the pipeline under the barrier claim rule, the held handles, the
/// counters — ADR-0174 D5) and the extent sweep seed.
struct RecoveringTierNs<F: SegmentFs> {
    ns: inf_log::NsId,
    replay: inf_store::TierReplay<F>,
    extents_listed: Vec<u64>,
    extents_quarantined: Vec<u64>,
    /// The quarantined ids the replayed map references, in the listing's
    /// order: the hand-over's sweep seed appends them, its revive stage
    /// renames them back (ADR-0096 D3).
    revive: Vec<u64>,
    /// Lent through the seam since the last drain (its index is in
    /// [`RecoveringTiers::lent`] once).
    lent: bool,
}

/// The replay seam over the recovered pipelines (ADR-0174 D1): lends a
/// namespace's boot machine to the keyspace's tiered replay arms — one
/// binary search per record among the cell's tiered namespaces, kept in
/// namespace order. The machines keep the I/O they did; the driver
/// drains it with [`take_charge`](Self::take_charge) where it decides
/// whether to yield, from the machines lent since the last drain only.
struct RecoveringTiers<F: SegmentFs> {
    /// The machines, ordered by namespace id.
    tiers: Vec<RecoveringTierNs<F>>,
    /// Indices into `tiers` lent since the last drain, each once: a frame
    /// or section drains O(machines it lent), not O(tiered namespaces).
    lent: Vec<usize>,
}

impl<F: SegmentFs> inf_store::ReplaySpill for RecoveringTiers<F> {
    type Fs = F;

    fn replay_mut(&mut self, ns: inf_log::NsId) -> Option<&mut inf_store::TierReplay<F>> {
        let index = self.tiers.binary_search_by_key(&ns, |tier| tier.ns).ok()?;
        let tier = &mut self.tiers[index];
        if !tier.lent {
            tier.lent = true;
            self.lent.push(index);
        }
        Some(&mut tier.replay)
    }
}

impl<F: SegmentFs> RecoveringTiers<F> {
    /// The boot I/O the machines lent since the last drain did, in
    /// step-budget bytes (bound: the machines lent, each once).
    fn take_charge(&mut self) -> u64 {
        let mut charge = 0u64;
        for index in self.lent.drain(..) {
            let tier = &mut self.tiers[index];
            tier.lent = false;
            charge = charge.saturating_add(boot_io_charge(tier.replay.take_work()));
        }
        charge
    }
}

/// The recovery step budget's charge for a machine's drained boot I/O
/// (ADR-0174 R10; D6's gauge): tier bytes written at
/// [`REPLAY_TIER_BYTE_CHARGE`](crate::limits::REPLAY_TIER_BYTE_CHARGE),
/// barriers at [`REPLAY_BARRIER_CHARGE_BYTES`](crate::limits::REPLAY_BARRIER_CHARGE_BYTES),
/// and the settle reads and walked bytes at the store's prices, which its
/// end settle walk yields on
/// ([`settle_charge_bytes`](inf_store::ReplayWork::settle_charge_bytes)).
/// Saturating: a yield test and a gauge, never an account that balances.
fn boot_io_charge(work: inf_store::ReplayWork) -> u64 {
    use crate::limits::{REPLAY_BARRIER_CHARGE_BYTES, REPLAY_TIER_BYTE_CHARGE};
    if cfg!(inf_canary_replay_charge_dropped) {
        // The planted canary charges no boot I/O to the step budget.
        return 0;
    }
    work.tier_bytes
        .saturating_mul(REPLAY_TIER_BYTE_CHARGE)
        .saturating_add(work.barriers.saturating_mul(REPLAY_BARRIER_CHARGE_BYTES))
        .saturating_add(work.settle_charge_bytes())
}

/// One recovered tiered namespace's plane-side pieces (M4-S26), as the
/// boot machine handed them over (ADR-0174 R10).
pub(crate) struct RecoveredTierNs<F: SegmentFs> {
    pub ns: inf_log::NsId,
    pub flush: inf_log::TierFlush<F>,
    /// Creation-mode fds for every sealed catalogue file (ADR-0054: one
    /// fd, one mode) — the cold-read table inherits them.
    pub files: Vec<(u32, F::File)>,
}

impl<F: SegmentFs + Clone> Recovery<F> {
    /// Prepares recovery for cell `cell` under `cfg.data_dir`. No I/O
    /// happens here — the first [`step`](Self::step) does.
    #[must_use]
    pub fn new(fs: F, cell: u16, cfg: &DurableConfig, anchor: WallAnchor, now: Nanos) -> Self {
        let shard_dir = cfg.data_dir.join(format!("shard-{cell}"));
        let boot_reads = ReadAheadFs::new(fs.clone(), cfg.recover.boot_prefetch);
        Recovery {
            fs: Some(fs),
            boot_reads,
            cell,
            cfg: cfg.clone(),
            anchor,
            now,
            phase: Phase::Start,
            stats: RecoverStats::default(),
            log_dir: shard_dir.join("log"),
            ckpt_dir: shard_dir.join("ckpt"),
            shard_dir,
            manifest: None,
            begin_lsn: None,
            floor: SegmentId(0),
            scan: None,
            stale: Vec::new(),
            segments: Vec::new(),
            seg_sizes: Vec::new(),
            ends: Vec::new(),
            census: inf_foundation::bounded::CapCensus::new(inf_foundation::CellId(cell)).0,
            residue: Vec::new(),
            stale_slack: Vec::new(),
            bytes_total: 0,
            bytes_done: 0,
            bytes_consumed: 0,
            defer_boot_sync: false,
            barrier_dirs: None,
            ick_credited: 0,
            prev_stamp: None,
            saw_v2: false,
            residue_stop: false,
            max_prefix_epoch: 0,
            evidence: Vec::new(),
            hole: None,
            last_data: 0,
            finished: None,
            recovering_tiers: RecoveringTiers { tiers: Vec::new(), lent: Vec::new() },
            handing_over: Vec::new(),
            step_charge: 0,
            step_parked_bytes: 0,
            last_unit: UnitPhase::Start,
            step_work: 0,
            recovered_tiers: Vec::new(),
            tier_replay_checked: false,
            #[cfg(feature = "doc")]
            sidecar: Some(inf_store::SidecarLoader::default()),
        }
    }

    /// Switch this machine to deferred boot-metadata durability
    /// (M2.5-S01): no blocking fsync runs on the ready path; the caller
    /// takes [`take_boot_barrier_dirs`](Self::take_boot_barrier_dirs)
    /// after `Complete` and registers the barriers. Loop-resident boots
    /// only — a synchronous run-to-completion has no driver for barriers.
    #[must_use]
    pub fn deferred_boot_sync(mut self) -> Self {
        self.defer_boot_sync = true;
        self
    }

    /// Registers the recovered tier pipelines' capped holders on `census`,
    /// the cell's own (ADR-0163 D2), in place of the standalone census a
    /// recovery builds for itself.
    #[must_use]
    pub fn with_census(mut self, census: std::rc::Rc<inf_foundation::bounded::CapCensus>) -> Self {
        self.census = census;
        self
    }

    /// The boot directory handles whose driver-ridden fdatasyncs are the
    /// boot barriers (`None` unless
    /// [`deferred_boot_sync`](Self::deferred_boot_sync) was set). Take
    /// them after `Complete`, before [`finish`](Self::finish).
    pub fn take_boot_barrier_dirs(&mut self) -> Option<BootDirs<F::File>> {
        self.barrier_dirs.take()
    }

    /// Code of the phase the next [`step`](Self::step) will run — the
    /// RecoveryBoard's stuck-cell observable (M2.5-S01: published
    /// *before* the step so a stalled step is visible from outside).
    /// 1 = start, 2 = checkpoint, 3 = replay, 4 = audit, 5 = finish,
    /// 6 = complete.
    #[must_use]
    pub fn phase_code(&self) -> u8 {
        self.phase().code()
    }

    /// The phase the next [`step`](Self::step) runs (M4.5-S39d).
    #[must_use]
    pub fn phase(&self) -> RecoverPhase {
        match &self.phase {
            Phase::Start => RecoverPhase::Start,
            Phase::Ick { .. } | Phase::CkptEnd { .. } => RecoverPhase::Ckpt,
            Phase::Replay { .. } => RecoverPhase::Replay,
            Phase::Audit { .. } | Phase::Probe { .. } => RecoverPhase::Audit,
            // The end settle, the hand-over and the resume finish replay's
            // state: their time is the finish phase's (the board and `INFO`
            // decompose five phases).
            Phase::Finish | Phase::Settle | Phase::HandOver { .. } | Phase::Resume { .. } => {
                RecoverPhase::Finish
            }
            Phase::Complete => RecoverPhase::Complete,
        }
    }

    /// Credits `ns` of the driver's clock to `phase` (M4.5-S39d): the
    /// driver samples its injected clock around consecutive steps and
    /// attributes the delta to the phase the earlier step ran. Credits to
    /// `Complete` are the sample after the last real step — nothing ran,
    /// so they are dropped on the floor rather than invented as a phase.
    pub fn credit_phase_time(&mut self, phase: RecoverPhase, ns: u64) {
        let phases = &mut self.stats.phases;
        let slot = match phase {
            RecoverPhase::Start => &mut phases.start_ns,
            RecoverPhase::Ckpt => &mut phases.ckpt_ns,
            RecoverPhase::Replay => &mut phases.replay_ns,
            RecoverPhase::Audit => &mut phases.audit_ns,
            RecoverPhase::Finish => &mut phases.finish_ns,
            RecoverPhase::Complete => return,
        };
        *slot += ns;
        phases.total_ns += ns;
    }

    /// Progress numerator: bytes validated + applied so far (checkpoint
    /// blocks + replayed frames; a completed segment credits its slack).
    #[must_use]
    pub fn bytes_done(&self) -> u64 {
        self.bytes_done
    }

    /// Progress denominator: checkpoint file size + segment file extents
    /// (includes preallocated slack — an upper bound, disclosed).
    #[must_use]
    pub fn bytes_total(&self) -> u64 {
        self.bytes_total
    }

    /// Bytes actually read + validated so far (excludes the slack credits
    /// in [`bytes_done`](Self::bytes_done)) — what pacing should meter.
    #[must_use]
    pub fn bytes_consumed(&self) -> u64 {
        self.bytes_consumed
    }

    /// Segments fully replayed / total (0/0 before the scan phase runs).
    #[must_use]
    pub fn segments_progress(&self) -> (u64, u64) {
        let done = match &self.phase {
            Phase::Start | Phase::Ick { .. } | Phase::CkptEnd { .. } => 0,
            Phase::Replay { idx, .. } | Phase::Probe { idx } => *idx as u64,
            Phase::Audit { idx } => *idx as u64 + 1,
            Phase::Finish
            | Phase::Settle
            | Phase::HandOver { .. }
            | Phase::Resume { .. }
            | Phase::Complete => self.segments.len() as u64,
        };
        (done, self.segments.len() as u64)
    }

    /// Stats so far (complete once [`finish`](Self::finish) is reachable).
    #[must_use]
    pub fn stats(&self) -> &RecoverStats {
        &self.stats
    }

    /// The boot I/O charge the last [`step`](Self::step) took, in bytes
    /// of the recovery step budget (ADR-0174 R10): the tier bytes,
    /// barriers, settle reads and end-settle bytes its replay machines
    /// drained, at their prices. ADR-0174 D6's gauge is the largest of
    /// these.
    #[must_use]
    pub fn step_charge_bytes(&self) -> u64 {
        self.step_charge
    }

    /// The work the last [`step`](Self::step) did, in budget bytes
    /// (ADR-0174 A1.2): what it charged plus the bytes of the frame or
    /// section it entered and parked in. The step-facts gauge is the largest
    /// of these.
    #[must_use]
    pub fn step_work_bytes(&self) -> u64 {
        self.step_work
    }

    fn fs(&self) -> &F {
        self.fs.as_ref().expect("fs present until finish")
    }

    /// Folds one evidence scan's cost into the audit phase's accounting
    /// (M4.5-S39d). Cumulative: a probe later lifted into a replay still
    /// read its bytes.
    fn note_audit(&mut self, evidence: &RegionEvidence) {
        let phases = &mut self.stats.phases;
        phases.audit_bytes += evidence.bytes_read;
        phases.audit_valid_frames += evidence.valid_frames;
        phases.audit_foreign_frames += evidence.foreign_frames;
    }

    /// Runs one bounded recovery step: at most `budget_bytes` of charged
    /// work — the checkpoint and log bytes its cursor passed, boot I/O and
    /// RAM units at fixed prices — plus one unit and the frame or section it
    /// entered and parked in (ADR-0174 A1), or one audit/GC unit. Returns
    /// [`RecoveryProgress::Complete`] when [`finish`](Self::finish) may be
    /// called.
    ///
    /// # Errors
    /// Exactly [`open_cell_log`]'s fail-stop taxonomy; a failed machine
    /// must not be stepped again.
    pub fn step(&mut self, ks: &mut Keyspace, budget_bytes: u64) -> io::Result<RecoveryProgress> {
        self.step_charge = 0;
        self.step_parked_bytes = 0;
        let mut budget = StepBudget::new(budget_bytes);
        let unit = self.unit_phase();
        let budget_ref = &mut budget;
        let progress = match core::mem::replace(&mut self.phase, Phase::Complete) {
            Phase::Start => self.step_start(ks),
            Phase::Ick { reader } => self.step_ick(ks, reader, budget_ref),
            Phase::CkptEnd { tier, at } => self.step_ckpt_end(ks, tier, at, budget_ref),
            Phase::Replay { idx, reader } => self.step_replay(ks, idx, reader, budget_ref),
            Phase::Audit { idx } => self.step_audit(idx),
            // Replay is not over while a hole can still be lifted: the
            // end-of-replay checks run inside the finish step, once the
            // lift is declined — never at a probe, which precedes segments
            // a lift replays.
            Phase::Probe { idx } => self.step_probe(idx),
            Phase::Finish => self.step_finish(ks),
            Phase::Settle => self.step_settle(ks, budget_ref).map(|()| RecoveryProgress::Working),
            Phase::HandOver { stage, then } => self.step_hand_over(ks, stage, then, budget_ref),
            // The resume step reports `Working` and the *next* step
            // `Complete` (M4.5-S39d): the driver samples its clock around
            // every step, so its time is only attributable once a later
            // call exists — one extra polling iteration at boot.
            Phase::Resume { from } => {
                self.step_resume(ks, from).map(|()| RecoveryProgress::Working)
            }
            Phase::Complete => Ok(RecoveryProgress::Complete),
        };
        // ADR-0174 D6's gauge: the largest boot I/O charge one step took;
        // A1.2's: the largest work, its charge and the frame it parked in.
        let gauge = &mut self.stats.tier_replay.step_charge_bytes_max;
        *gauge = (*gauge).max(self.step_charge);
        let facts = &mut self.stats.step;
        let work = budget.charged().saturating_add(self.step_parked_bytes);
        facts.work_bytes_max = facts.work_bytes_max.max(work);
        self.step_work = work;
        if unit != UnitPhase::Complete {
            facts.steps += 1;
        }
        self.stats.budget_asks = self.stats.budget_asks.saturating_add(budget.asks);
        self.last_unit = unit;
        progress
    }

    /// The row of [`RECOVERY_UNITS`] the last [`step`](Self::step) ran —
    /// finer than [`phase`](Self::phase): the end settle and the finish
    /// step share a board code.
    #[must_use]
    pub fn unit_row(&self) -> &'static RecoveryUnit {
        &RECOVERY_UNITS[self.last_unit.index()]
    }

    /// The row of [`RECOVERY_UNITS`] the next [`step`](Self::step) runs.
    #[must_use]
    pub fn next_unit_row(&self) -> &'static RecoveryUnit {
        &RECOVERY_UNITS[self.unit_phase().index()]
    }

    /// The unit phase of the phase the next step runs.
    fn unit_phase(&self) -> UnitPhase {
        match &self.phase {
            Phase::Start => UnitPhase::Start,
            Phase::Ick { .. } => UnitPhase::Ick,
            Phase::CkptEnd { .. } => UnitPhase::CkptEnd,
            Phase::Replay { .. } => UnitPhase::Replay,
            Phase::Audit { .. } => UnitPhase::Audit,
            Phase::Probe { .. } => UnitPhase::Probe,
            Phase::Finish => UnitPhase::Finish,
            Phase::Settle => UnitPhase::Settle,
            Phase::HandOver { .. } => UnitPhase::HandOver,
            Phase::Resume { .. } => UnitPhase::Resume,
            Phase::Complete => UnitPhase::Complete,
        }
    }

    /// Drains the replay machines' boot I/O into this step's charge.
    fn drain_boot_io(&mut self) {
        let charge = self.recovering_tiers.take_charge();
        self.step_charge = self.step_charge.saturating_add(charge);
    }

    /// Drains the replay machines' boot I/O into this step's charge and
    /// into its budget.
    fn drain_into(&mut self, budget: &mut StepBudget) {
        let charge = self.recovering_tiers.take_charge();
        self.step_charge = self.step_charge.saturating_add(charge);
        budget.charge(charge);
    }

    /// The recovered rotor + stats + manifest seed.
    ///
    /// # Panics
    /// If recovery has not reported [`RecoveryProgress::Complete`].
    #[must_use]
    pub fn finish(self) -> (SegmentRotor<F>, RecoverStats, Option<RecoveredManifest>) {
        let (rotor, seed) = self.finished.expect("recovery complete before finish()");
        (rotor, self.stats, seed)
    }

    fn step_start(&mut self, ks: &mut Keyspace) -> io::Result<RecoveryProgress> {
        // M2.5-S01: on the loop-resident tier a blocking dir-fsync here
        // can stall the reactor for minutes behind foreign journal
        // writeback (the captured cell-2 wedge) — ready must not wait on
        // the device; the handles become ledger-fronted driver barriers.
        let dirs = if self.defer_boot_sync {
            let (dirs, handles) = create_cell_dirs_deferred(self.fs(), &self.shard_dir)?;
            self.barrier_dirs = Some(handles);
            dirs
        } else {
            create_cell_dirs(self.fs(), &self.shard_dir)?
        };
        debug_assert_eq!(dirs.log, self.log_dir);
        let manifest = read_manifest(&self.boot_reads, &self.shard_dir)?;
        // ADR-0094 D6: the manifest names the secret that placed its
        // checkpoint's refs; a boot holding another one refuses here,
        // before the checkpoint loads. `infinityd` pre-scans every shard
        // before any cell starts; this is the guard every boot passes —
        // the simulators' and the embedded one's included.
        if let Some(m) = &manifest {
            let secret = ks.hasher().identity();
            if m.key_hash_id != secret {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "cell {}: MANIFEST names key-hash id {} but the node's secret is {}: the \
                         secret was replaced after that checkpoint was placed (ADR-0094 D6) — \
                         every cold ref would be silently unreachable; restore the directory's \
                         original key-hash.toml (fail-stop)",
                        self.cell, m.key_hash_id, secret
                    ),
                ));
            }
        }

        // The manifest names the recovery unit; without one, the whole
        // retained log is the unit.
        self.floor = manifest.as_ref().map_or(SegmentId(0), Manifest::floor);
        let outcome =
            scan_log_dir_from(&self.boot_reads, &self.log_dir, self.floor).map_err(io_invalid)?;
        let scan = outcome.scan;
        self.stale = outcome.stale;
        self.segments = scan.segments().to_vec();
        for &segment in &self.segments {
            let file = self
                .fs()
                .open_read(&self.log_dir.join(segment_file_name(segment)))
                .map_err(io_invalid)?;
            // Before a checkpoint is loaded or a record replayed: a file
            // whose end no LSN can name is refused, never truncated to fit
            // (ADR-0018).
            let size = file.file_size().map_err(io_invalid)?;
            check_segment_len(segment, size).map_err(io_invalid)?;
            self.seg_sizes.push(size);
        }
        self.bytes_total = self.seg_sizes.iter().sum();

        let next = if let Some(manifest) = manifest {
            // The floor segment holds the begin marker and is never
            // truncated: its absence means the disk lost named state —
            // fail-stop.
            if scan.segments().first() != Some(&self.floor) {
                return Err(io_msg(format!(
                    "MANIFEST names ckpt {} with floor {} but the floor segment is missing \
                     (present: {:?})",
                    manifest.ckpt_id,
                    self.floor,
                    scan.segments()
                )));
            }
            if let (Some(last_listed), Some(tail)) = (manifest.segments.last(), scan.tail())
                && *last_listed > tail
            {
                return Err(io_msg(format!(
                    "MANIFEST lists segment {last_listed} but the log ends at {tail}"
                )));
            }
            // Tiered namespaces recover first (M4-S26; ADR-0174 D4): map
            // manifested files, seed the new life at the manifested
            // flushed watermark, and swap the recovered table in —
            // checkpoint entries and tail records then apply onto the
            // recovered life, never the fresh one.
            self.recover_tiers(ks, Some(&manifest))?;
            let ick_path = self.ckpt_dir.join(ick_file_name(manifest.ckpt_id));
            // Presize from the footer counts before streaming (M2-S13):
            // the bulk apply must not pay a doubling-rehash storm.
            let (counts, probe_hit) =
                read_ick_counts_probed(&self.boot_reads, &ick_path, IckReaderConfig::default())
                    .map_err(|err| io_msg(format!("checkpoint {}: {err:?}", ick_path.display())))?;
            self.stats.ckpt_footer_probe_hit = Some(probe_hit);
            for &(ns, entries) in &counts {
                ks.reserve_ns(inf_log::NsId(ns), entries);
            }
            let reader =
                IckReader::open(&self.boot_reads, &ick_path, IckReaderConfig::default())
                    .map_err(|err| io_msg(format!("checkpoint {}: {err:?}", ick_path.display())))?;
            let info = reader.info();
            if info.ckpt_id != manifest.ckpt_id
                || info.begin_lsn != manifest.begin_lsn
                || info.cell != self.cell
            {
                return Err(io_msg(format!(
                    "checkpoint header disagrees with MANIFEST: ick {{id {}, begin {}, cell \
                     {}}} vs manifest {{id {}, begin {}, cell {}}}",
                    info.ckpt_id,
                    info.begin_lsn,
                    info.cell,
                    manifest.ckpt_id,
                    manifest.begin_lsn,
                    self.cell
                )));
            }
            self.bytes_total += reader.file_size();
            self.begin_lsn = Some(manifest.begin_lsn);
            self.manifest = Some(manifest);
            Phase::Ick { reader: Box::new(reader) }
        } else if scan.is_empty() {
            // Fresh cell: nothing to replay, nothing to audit. Its tiered
            // namespaces still recover through the empty section (ADR-0174
            // D4: a dead life's tier files go before any flush) and hand
            // their machines over, under the step budget, before its first
            // segment is created.
            self.recover_tiers(ks, None)?;
            self.begin_hand_over();
            Phase::HandOver { stage: HandOverStage::Stamp, then: ResumeFrom::FreshCell }
        } else {
            // No MANIFEST: the whole retained log replays, into tiered
            // namespaces recovered through the empty section (D4).
            self.recover_tiers(ks, None)?;
            Phase::Replay { idx: 0, reader: ReplayReader::Unopened }
        };
        self.scan = Some(scan);
        self.phase = next;
        Ok(RecoveryProgress::Working)
    }

    /// The end of replay (ADR-0174 R10's first half), once the finish
    /// step declined a lift — after the last replayed record, which a
    /// lifted hole moves past the probed segments: the checks must see the
    /// displacement register and the index after every record a lift
    /// replays, never before. A non-empty displacement register means the log
    /// ended between a marker and its paired mutation — corrupt input by
    /// the ADR-0057 D4 same-frame rule (fail-stop, never a skip); then
    /// every tiered machine declares the end of replay, and one that
    /// demoted enters its end settle at `ro`.
    fn end_replay(&mut self, ks: &Keyspace) -> io::Result<()> {
        debug_assert!(!self.tier_replay_checked, "replay ends once");
        self.tier_replay_checked = true;
        if ks.displace_register_len() > 0 {
            return Err(io_msg(format!(
                "log ends with {} unpaired displacement marker(s) (ADR-0057 D4)",
                ks.displace_register_len()
            )));
        }
        for tier in &mut self.recovering_tiers.tiers {
            // A record left waiting for room was passed unapplied: a typed
            // refusal, a defect class no input reaches (ADR-0174 A1).
            if tier.replay.waits_for_room() {
                return Err(io_msg(format!(
                    "tiered ns {}: RoomMoved: replay ended with a record waiting for room",
                    tier.ns.0
                )));
            }
            if let Some(table) = ks.tiered_store(tier.ns) {
                tier.replay.end_of_replay(table);
            }
        }
        Ok(())
    }

    /// Recovers every tiered namespace the catalog holds (ADR-0174 D4):
    /// map + verify tier files, seed the new life at the manifested
    /// flushed watermark, install the recovered table, and build the boot
    /// machine that holds the creation-mode fds the plane's cold-read
    /// table inherits. A namespace no section names — no MANIFEST yet, or
    /// one published before the namespace was created — recovers through
    /// the empty section: `flushed` 0, no file, its dead-life tier files
    /// removed before any flush.
    fn recover_tiers(&mut self, ks: &mut Keyspace, manifest: Option<&Manifest>) -> io::Result<()> {
        self.sweep_dropped(ks, manifest)?;
        let tiered: Vec<(inf_log::NsId, inf_store::TierSpec)> =
            ks.ns_iter().filter_map(|spec| spec.tier.map(|tier| (spec.id, tier))).collect();
        // Bound: one recovery per tiered namespace of the catalog.
        for (ns, spec) in tiered {
            let named = manifest.and_then(|m| m.tier_ns(ns.0));
            let empty =
                inf_log::manifest::TierNsManifest { ns: ns.0, flushed: 0, files: Vec::new() };
            let section = named.unwrap_or(&empty);
            let ckpt_id = manifest.map_or(0, |m| m.ckpt_id);
            self.recover_tier_ns(ks, ns, &spec, section, ckpt_id)?;
        }
        // The seam finds a record's machine by binary search.
        self.recovering_tiers.tiers.sort_unstable_by_key(|tier| tier.ns);
        Ok(())
    }

    /// ADR-0100 D6: the catalog decides which namespaces exist. Over a
    /// MANIFEST's tier sections a tombstoned id's residue is swept and an
    /// unknown id's is corruption; then every tombstoned namespace's
    /// directory no section names is swept — with or without a MANIFEST,
    /// as D4 recovers a catalogued namespace with or without one (a DROP
    /// cut mid-teardown before the first checkpoint publishes leaves it).
    fn sweep_dropped(&mut self, ks: &Keyspace, manifest: Option<&Manifest>) -> io::Result<()> {
        let mut swept: Vec<inf_log::NsId> = Vec::new();
        for tier in manifest.map_or(&[][..], |m| m.tiers.as_slice()) {
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
            self.sweep_dropped_ns(ns)?;
            swept.push(ns);
        }
        let tombstones: Vec<inf_log::NsId> = ks.ns_tombstones().to_vec();
        for ns in tombstones {
            if !swept.contains(&ns) {
                self.sweep_dropped_ns(ns)?;
            }
        }
        Ok(())
    }

    /// One tiered namespace through `section` (its manifest section, or
    /// the empty one): the recovered table installed in the keyspace, the
    /// boot machine kept for replay.
    fn recover_tier_ns(
        &mut self,
        ks: &mut Keyspace,
        ns: inf_log::NsId,
        spec: &inf_store::TierSpec,
        section: &inf_log::manifest::TierNsManifest,
        ckpt_id: u64,
    ) -> io::Result<()> {
        let demote = spec.demotion_config();
        let reserve_bytes = demote
            .ring_reserve_bytes()
            .ok_or_else(|| io_msg("tier spec ring reservation unrepresentable".into()))?;
        let recovered = inf_store::recover_tiered_ns(
            self.fs().clone(),
            section,
            ckpt_id,
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
                life_origin: inf_store::LogicalAddr::ZERO, // overridden by the section
            },
            demote,
            1024,
            ks.hasher(),
            &self.census,
        )?;
        ks.install_recovered_tiered(ns, recovered.table);
        let removed = &mut self.stats.tier_replay.dead_life_files_removed;
        *removed = removed.saturating_add(u64::from(recovered.stats.files_removed));
        // The manifested files' creation-mode handles are the boot
        // machine's (ADR-0054 D1; opened by `recover_tiered_ns`), and the
        // hand-over at the end of replay returns them.
        self.recovering_tiers.tiers.push(RecoveringTierNs {
            ns,
            replay: recovered.replay,
            extents_listed: recovered.extents_listed,
            extents_quarantined: recovered.extents_quarantined,
            revive: Vec::new(),
            lent: false,
        });
        Ok(())
    }

    /// Unlinks a tombstoned namespace's `ns-N/cold` files — tier files and
    /// blob extents share that directory — before the cell serves
    /// (ADR-0100 D6: the teardown an acked `DROP` owed). Idempotent: an
    /// absent directory is nothing to do; a cut mid-sweep repeats it on
    /// the next boot because the tombstone outlives the residue.
    fn sweep_dropped_ns(&mut self, ns: inf_log::NsId) -> io::Result<()> {
        let cold = self.shard_dir.join(format!("ns-{}", ns.0)).join("cold");
        let names = match self.fs().list_dir(&cold) {
            Ok(names) => names,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(err) => return Err(err),
        };
        let mut files = 0u64;
        for name in names {
            self.fs().remove_file(&cold.join(&name))?;
            files += 1;
        }
        if files > 0 {
            self.fs().sync_dir(&cold)?;
        }
        self.stats.dropped_ns_swept += 1;
        self.stats.dropped_files_swept += files;
        eprintln!(
            "cell {}: swept dropped namespace {} residue ({files} files) — ADR-0100 D6",
            self.cell, ns.0
        );
        Ok(())
    }

    /// One end-of-replay settle step (ADR-0174 R10) over every tiered
    /// namespace that demoted, under what is left of the step budget once
    /// this step's charge is taken; once every machine's settle reached its
    /// tail, the hand-over follows.
    fn step_settle(&mut self, ks: &mut Keyspace, budget: &mut StepBudget) -> io::Result<()> {
        // Bound: one settle step per namespace still settling, each under
        // the budget left; a machine that did not demote answers `Done`.
        // A step yields only once it charged something (L6: progress is
        // explicit): at a budget it never spent — a zero budget — the
        // first settle walks one record (`settle_step`'s floor).
        for i in 0..self.recovering_tiers.tiers.len() {
            if budget.spent() {
                self.phase = Phase::Settle;
                return Ok(());
            }
            let left = budget.left();
            // The planted canaries break R10's budget: the end settle walks
            // its whole open span in one step, or settles at twice its budget.
            let left = if cfg!(inf_canary_replay_settle_unbudgeted) { u64::MAX } else { left };
            let left =
                if cfg!(inf_canary_replay_settle_doubled) { left.saturating_mul(2) } else { left };
            let tier = &mut self.recovering_tiers.tiers[i];
            let ns = tier.ns;
            let table = ks
                .tiered_store_mut(ns)
                .ok_or_else(|| io_msg(format!("ns {}: a boot machine with no table", ns.0)))?;
            let progress = tier
                .replay
                .settle_step(table, left)
                .map_err(|refusal| replay_refused(ns, &refusal))?;
            let charge = boot_io_charge(tier.replay.take_work());
            self.step_charge = self.step_charge.saturating_add(charge);
            budget.charge(charge);
            if progress == inf_store::SettleProgress::More {
                self.phase = Phase::Settle;
                return Ok(());
            }
        }
        self.begin_hand_over();
        self.phase = Phase::HandOver { stage: HandOverStage::Stamp, then: ResumeFrom::Log };
        Ok(())
    }

    /// The step after the hand-over (one unit, outside the step budget): a
    /// recovered log commits the sidecar loader and resumes at its resume
    /// point; a fresh cell creates its first segment.
    fn step_resume(&mut self, ks: &mut Keyspace, from: ResumeFrom) -> io::Result<()> {
        match from {
            ResumeFrom::Log => {
                self.finish_index_replay(ks);
                self.resume_log()
            }
            ResumeFrom::FreshCell => {
                let fs = self.fs.take().expect("fs present");
                let rotor = if self.defer_boot_sync {
                    SegmentRotor::create_fresh_deferred(fs, self.log_dir.clone(), self.cfg.geometry)
                } else {
                    SegmentRotor::create_fresh(fs, self.log_dir.clone(), self.cfg.geometry)
                };
                self.finished = Some((rotor.map_err(io_invalid)?, None));
                self.phase = Phase::Complete;
                Ok(())
            }
        }
    }

    /// End-of-replay sidecar commit (M4.5-S06, ADR-0078 D6), run once
    /// after the last replayed record: loaded trees are tail-caught-up — mark
    /// them converged + cell `Ready`, disarm replay maintenance, and
    /// log every per-index rebuild-vs-load decision (the was-ready
    /// downgrade loudly — L10). No-checkpoint boots commit an empty
    /// loader: every declaration records `rebuilt (no-sidecar)`.
    fn finish_index_replay(&mut self, ks: &mut Keyspace) {
        #[cfg(feature = "doc")]
        if let Some(sidecar) = self.sidecar.take() {
            for row in sidecar.commit_ready(ks) {
                match row.decision {
                    inf_store::SidecarBootDecision::Loaded { entries } => eprintln!(
                        "cell {}: index {}/{} sidecar loaded ({entries} entries; tail caught up)",
                        self.cell, row.ns.0, row.id.0
                    ),
                    inf_store::SidecarBootDecision::Rebuilt { reason } => {
                        let downgrade =
                            if row.was_ready { " — was serving before the crash" } else { "" };
                        eprintln!(
                            "cell {}: index {}/{} rebuilding ({}){downgrade}",
                            self.cell,
                            row.ns.0,
                            row.id.0,
                            reason.name()
                        );
                    }
                }
            }
        }
        #[cfg(not(feature = "doc"))]
        {
            let _ = ks;
        }
    }

    /// Hands the recovered tiered plane halves to the caller (the plane
    /// installs them into its tier state at completion — M4-S26).
    pub(crate) fn take_recovered_tiers(&mut self) -> Vec<RecoveredTierNs<F>> {
        debug_assert!(self.recovering_tiers.tiers.is_empty(), "every boot machine handed over");
        debug_assert!(self.handing_over.is_empty(), "every boot machine handed over");
        std::mem::take(&mut self.recovered_tiers)
    }

    fn step_replay(
        &mut self,
        ks: &mut Keyspace,
        idx: usize,
        reader: ReplayReader<BootFile<F>>,
        budget: &mut StepBudget,
    ) -> io::Result<RecoveryProgress> {
        // ADR-0031 D5: replay already ended at an epoch-regressed frame —
        // every later segment is discarded-life residue. Record an empty
        // end (offset 0) so the audit scans it whole as slack evidence.
        if self.residue_stop {
            self.stats.segments += 1;
            self.ends.push((0, None));
            self.bytes_done += self.seg_sizes[idx];
            self.phase = Phase::Audit { idx };
            return Ok(RecoveryProgress::Working);
        }
        let (mut reader, mut at) = match reader {
            ReplayReader::Open { reader, at } => (reader, at),
            ReplayReader::Unopened => {
                let reader = SegmentReader::open(
                    &self.boot_reads,
                    &self.log_dir,
                    self.segments[idx],
                    ReaderConfig::default(),
                )
                .map_err(io_invalid)?;
                (Box::new(reader), FramePos::Boundary)
            }
        };
        // The throttle and progress count a frame whole at its entry, so a
        // parked frame lies before the next step's start; the step budget
        // charges bytes as the cursor passes them (ADR-0174 A1).
        let start_offset = u64::from(reader.offset());
        // Bound: the frames of one segment; a step leaves at its budget.
        loop {
            let (cursor, resumed) = match at {
                FramePos::InFrame(cursor) => (cursor, true),
                FramePos::Boundary => match self.enter_frame(idx, &mut reader, start_offset)? {
                    Some(cursor) => (cursor, false),
                    None => return Ok(RecoveryProgress::Working),
                },
            };
            match self.walk_frame(ks, &reader, cursor, budget, resumed)? {
                WalkEnd::FrameEnd { ask_yield } => {
                    at = FramePos::Boundary;
                    if ask_yield {
                        self.credit_replayed(&reader, start_offset);
                        let reader = ReplayReader::Open { reader, at };
                        self.phase = Phase::Replay { idx, reader };
                        return Ok(RecoveryProgress::Working);
                    }
                }
                WalkEnd::Parked { cursor, kind, unpaid } => {
                    if !resumed {
                        self.step_parked_bytes = unpaid;
                    }
                    let facts = &mut self.stats.step;
                    match kind {
                        YieldKind::Frame => facts.yields_in_frame += 1,
                        YieldKind::DemoteStep => facts.yields_in_demote_step += 1,
                    }
                    self.credit_replayed(&reader, start_offset);
                    let at = FramePos::InFrame(cursor);
                    self.phase = Phase::Replay { idx, reader: ReplayReader::Open { reader, at } };
                    return Ok(RecoveryProgress::Working);
                }
            }
        }
    }

    /// The bytes this step's reader consumed, credited to progress and the
    /// throttle at the frame granularity they were read in.
    fn credit_replayed(&mut self, reader: &SegmentReader<BootFile<F>>, start_offset: u64) {
        let consumed = u64::from(reader.offset()) - start_offset;
        self.bytes_done += consumed;
        self.bytes_consumed += consumed;
    }

    /// Counts one consumed record's outcome.
    fn count_outcome(&mut self, ks: &Keyspace, outcome: ReplayOutcome) {
        match outcome {
            ReplayOutcome::Applied => self.stats.records_applied += 1,
            ReplayOutcome::SkippedUnknownNs(ns) => {
                self.stats.records_skipped += 1;
                // ADR-0103 D4: a tombstone explains a dropped namespace's
                // records; an id nothing explains is a foreign log or
                // corruption — counted apart, asserted zero by the DST,
                // never silent.
                if !ks.ns_tombstoned(ns) {
                    self.stats.records_skipped_unknown_ns += 1;
                }
            }
            ReplayOutcome::SkippedReserved => self.stats.records_skipped += 1,
            ReplayOutcome::SkippedMarker => self.stats.markers += 1,
            ReplayOutcome::SkippedDocDeltaStale => self.stats.doc_deltas_skipped_stale += 1,
            ReplayOutcome::SkippedDocDeltaMissing => self.stats.doc_deltas_skipped_missing += 1,
        }
    }

    /// M2.5-S12 (ADR-0031 D3): pre-apply continuity verdict for one
    /// validating frame's stamp against the prefix state. `Replay` admits
    /// the frame; `ResidueStop` ends replay at a discarded-life frame;
    /// malformed continuity between byte-adjacent validating frames is a
    /// named fail-stop (no honest writer emits it).
    fn classify_stamp(
        &mut self,
        stamp: Option<FrameStamp>,
        segment: SegmentId,
        frame_base: u32,
        first_lsn: Lsn,
    ) -> io::Result<StampVerdict> {
        let corruption = |detail: String| {
            io_msg(
                LogCorruption {
                    segment,
                    offset: frame_base,
                    evidence_segment: segment,
                    evidence_offset: frame_base,
                    detail,
                }
                .to_string(),
            )
        };
        let Some(stamp) = stamp else {
            if self.saw_v2 {
                return Err(corruption(
                    "a format-v1 frame follows a v2 frame in the replay prefix — append order \
                     makes this unreachable by any honest writer (ADR-0031 D2)"
                        .to_owned(),
                ));
            }
            return Ok(StampVerdict::Replay);
        };
        debug_assert!(
            stamp.covered_lsn <= first_lsn.to_u64(),
            "decode_frame admits covered_lsn <= first_lsn only (BadStamp)"
        );
        // Epochs only grow in append order, across segment rotations and
        // replay breaks alike: a lower-epoch frame anywhere later is
        // discarded-life residue (ADR-0031 D5).
        if stamp.epoch < self.max_prefix_epoch {
            return Ok(StampVerdict::ResidueStop);
        }
        // Cross-segment attestation (ADR-0031 D4): coverage is a prefix,
        // so a claim reaching into an earlier segment must lie within
        // that segment's surviving data — anything past it is proof the
        // disk lost covered bytes (a lie format v1 could not even see).
        // Claims below the manifest floor are pre-checkpoint history.
        let covered = Lsn::from_u64(stamp.covered_lsn);
        if covered.segment < segment
            && let Ok(idx) = self.segments.binary_search(&covered.segment)
            && covered.offset > self.ends[idx].0
        {
            return Err(corruption(format!(
                "frame attests fsync coverage up to {}, past segment {}'s surviving data end \
                 {:#x} — covered data was lost (ADR-0031 D4)",
                covered, covered.segment, self.ends[idx].0
            )));
        }
        if let Some(prev) = self.prev_stamp {
            if stamp.epoch == prev.epoch {
                if stamp.seq != prev.seq + 1 {
                    return Err(corruption(format!(
                        "frame seq {} follows seq {} within epoch {} — adjacent validating \
                         frames must be seq-contiguous (ADR-0031 D3)",
                        stamp.seq, prev.seq, stamp.epoch
                    )));
                }
                if stamp.covered_lsn < prev.covered_lsn {
                    return Err(corruption(format!(
                        "frame attestation regressed ({:#x} after {:#x}) within epoch {} — \
                         the watermark is monotone (ADR-0031 D3)",
                        stamp.covered_lsn, prev.covered_lsn, stamp.epoch
                    )));
                }
            } else if stamp.seq != 1 {
                return Err(corruption(format!(
                    "epoch stepped {} → {} but seq is {} — a new life stamps seq 1 \
                     (ADR-0031 D3)",
                    prev.epoch, stamp.epoch, stamp.seq
                )));
            }
        }
        self.saw_v2 = true;
        self.max_prefix_epoch = self.max_prefix_epoch.max(stamp.epoch);
        self.prev_stamp = Some(stamp);
        Ok(StampVerdict::Replay)
    }

    /// Slack audit of segment `idx`, immediately after its replay
    /// (ADR-0087 D6 — the order is the point: a later segment is never
    /// applied to the keyspace before this segment's slack is known
    /// clean). `scan_region_evidence` aggregates the v2 stamp facts in
    /// O(1) memory. A validating frame marks the hole: every later segment
    /// is probed for evidence instead of replayed, and the Finish phase
    /// decides refuse-or-truncate at the hole. Non-validating residue
    /// classifies by position there as before: torn final write at the
    /// resume point, tolerated inert remnants behind it.
    fn step_audit(&mut self, idx: usize) -> io::Result<RecoveryProgress> {
        let segment = self.segments[idx];
        let (end, failed) = (self.ends[idx].0, self.ends[idx].1.is_some());
        let evidence = scan_region_evidence(
            &self.boot_reads,
            &self.log_dir,
            segment,
            end,
            ReaderConfig::default(),
        )?;
        self.note_audit(&evidence);
        let residue = match evidence.summary() {
            RegionScan::ValidFrame { .. } | RegionScan::Garbage { .. } => true,
            RegionScan::AllZero => failed,
        };
        // ADR-0031 D5 as amended (2026-08-21): validating frames whose
        // epochs all sit below the replayed prefix's are discarded-life
        // residue — a recovery already resumed past them and a later life
        // wrote the prefix above them — never a hole of this life. (A hole
        // of the current life carries its own epoch; a lying device that
        // lost covered bytes of this life leaves this life's frames, or
        // attestations in later segments the cross-segment check refuses.)
        // The residue-stop that may have ended this segment's replay at
        // such a frame is lifted with it: the segments after it are the
        // live log, not residue.
        let stale_by_epoch = evidence.valid_frames > 0
            && !evidence.any_v1
            && evidence.max_epoch < self.max_prefix_epoch;
        if stale_by_epoch {
            self.stats.stale_residue_slacks += 1;
            self.residue_stop = false;
        }
        // ADR-0090 D2 as amended: a slack with no self-located frame and
        // at least one foreign-segment frame is recycled-life residue —
        // proven by the frames' own stamps, never a hole (there is no
        // frame of this life to be a hole *to*), never torn. Same
        // disposition as the epoch-proven class: excluded from the
        // attestation decision, trailing ones removed as stale.
        let recycled = evidence.is_recycled_residue();
        if recycled {
            self.stats.recycled_residue_slacks += 1;
        }
        // Proven residue is treated exactly like pre-zeroed slack for the
        // torn/trailing decisions: an empty recycled next segment stays
        // the tail (resume at 0, write-through from the first frame —
        // the file is fully allocated), a recycled tail resumes at its
        // data end, and nothing is reported torn. The reader's
        // foreign-segment stop makes the bytes behind the data end
        // unreachable on every later boot, which is the property zeros
        // gave (ADR-0090 D2/D3 as amended).
        let residue = residue && !recycled;
        self.residue.push(residue);
        self.stale_slack.push(stale_by_epoch);
        self.evidence.push(evidence);
        let next = idx + 1;
        self.phase = if evidence.valid_frames > 0 && !stale_by_epoch {
            self.hole = Some(idx);
            if next == self.segments.len() { Phase::Finish } else { Phase::Probe { idx: next } }
        } else if next == self.segments.len() {
            Phase::Finish
        } else {
            Phase::Replay { idx: next, reader: ReplayReader::Unopened }
        };
        Ok(RecoveryProgress::Working)
    }

    /// Evidence-only scan of segment `idx`, behind a hole (ADR-0087 D6):
    /// the whole file is residue by construction (nothing at or past the
    /// hole was acked), so it is never replayed — scanned for the stamp
    /// facts the Finish decision needs, then removed there. Records an
    /// empty data end and credits the segment's bytes to progress.
    fn step_probe(&mut self, idx: usize) -> io::Result<RecoveryProgress> {
        debug_assert!(self.hole.is_some_and(|hole| hole < idx), "probe only behind a hole");
        let segment = self.segments[idx];
        let evidence = scan_region_evidence(
            &self.boot_reads,
            &self.log_dir,
            segment,
            0,
            ReaderConfig::default(),
        )?;
        self.note_audit(&evidence);
        self.stats.segments += 1;
        self.ends.push((0, None));
        self.residue.push(!matches!(evidence.summary(), RegionScan::AllZero));
        self.stale_slack.push(false);
        self.evidence.push(evidence);
        self.bytes_done += self.seg_sizes[idx];
        let next = idx + 1;
        self.phase =
            if next == self.segments.len() { Phase::Finish } else { Phase::Probe { idx: next } };
        Ok(RecoveryProgress::Working)
    }

    fn step_finish(&mut self, ks: &mut Keyspace) -> io::Result<RecoveryProgress> {
        debug_assert_eq!(self.ends.len(), self.segments.len(), "every segment has an end");
        debug_assert_eq!(self.evidence.len(), self.segments.len(), "every segment was audited");
        // ADR-0031 D5 as amended (2026-08-21), the non-local half: a hole
        // whose slack frames all carry an epoch below one probed *beyond*
        // it is discarded-life residue the later life resumed past (the
        // prefix and the residue share a life, so the audit could not tell
        // locally; the later segments can). The probed segments are the
        // live log: drop their probe bookkeeping and replay them — nothing
        // of theirs was applied, so the replay order ADR-0087 D6 fixes is
        // kept (this segment's slack is now known to be residue).
        if let Some(hole) = self.hole {
            let later_max_epoch =
                self.evidence[hole + 1..].iter().map(|e| e.max_epoch).max().unwrap_or(0);
            let at_hole = &self.evidence[hole];
            if !at_hole.any_v1 && at_hole.max_epoch < later_max_epoch {
                self.stats.stale_residue_slacks += 1;
                self.stale_slack[hole] = true;
                self.hole = None;
                self.residue_stop = false;
                for idx in hole + 1..self.segments.len() {
                    self.stats.segments -= 1;
                    self.bytes_done -= self.seg_sizes[idx];
                }
                self.ends.truncate(hole + 1);
                self.residue.truncate(hole + 1);
                self.stale_slack.truncate(hole + 1);
                self.evidence.truncate(hole + 1);
                self.phase = Phase::Replay { idx: hole + 1, reader: ReplayReader::Unopened };
                return Ok(RecoveryProgress::Working);
            }
        }
        // No lift: the last replayed record is behind us. The
        // unpaired-marker check, then the end settle, the hand-over and the
        // sidecar commit — each sees the finished keyspace.
        self.end_replay(ks)?;
        self.phase = Phase::Settle;
        Ok(RecoveryProgress::Working)
    }

    /// The log's resume point (ADR-0087 D6, ADR-0031 D4): the resume
    /// segment and offset, whether residue lies behind it (torn, or a
    /// discarded life's), and the beyond-frame evidence's top epoch —
    /// refusing when a surviving frame attests coverage past the data end.
    fn resume_point(&mut self) -> io::Result<ResumePoint> {
        let ends = &self.ends;
        // The resume segment (ADR-0087 D6): the hole's, when it holds
        // data; else the last data-bearing segment before it (a hole at
        // offset 0 keeps nothing of its own — the previous data end is
        // the resume point and the segment is removed like any trailing
        // one). With no hole: the last data-bearing segment.
        let last_with_data = |upto: usize| (0..upto).rev().find(|&i| ends[i].0 > 0).unwrap_or(0);
        let last_data = match self.hole {
            Some(hole) if ends[hole].0 > 0 => hole,
            Some(hole) => last_with_data(hole),
            None => last_with_data(ends.len()),
        };
        self.last_data = last_data;
        // Residue beyond the resume point: torn (this life's, or
        // garbage) or stale (a discarded life's validating frames — a
        // remnant, removed with the trailing segments but never a
        // truncation of this life's data).
        let torn = (last_data..self.residue.len()).any(|i| self.residue[i] && !self.stale_slack[i]);
        let stale_trailing =
            (last_data..self.residue.len()).any(|i| self.residue[i] && self.stale_slack[i]);
        self.stats.sealed_slack_remnants =
            self.residue[..last_data].iter().filter(|&&r| r).count() as u64;
        let (resume_evidence, resume_evidence_anchor) = self.resume_evidence(last_data);

        let scan = self.scan.as_ref().expect("scan set in start");
        // The resume point: the last data-bearing segment's data end
        // whenever anything is truncated or removed behind it (torn, or
        // stale trailing residue — every later segment goes, so the tail
        // *is* that segment); otherwise the scan's tail at its own end (a
        // clean empty next segment resumes at 0 — pinned in
        // `recover_stamp.rs`: reading the removed segment's end here once
        // reopened the live segment at offset 0).
        let (resume_segment, resume_offset) = if torn || stale_trailing {
            (self.segments[last_data], ends[last_data].0)
        } else {
            (scan.tail().expect("non-empty scan"), ends.last().expect("non-empty scan").0)
        };
        let resume = Lsn::new(resume_segment, resume_offset);
        // ADR-0031 D4: validating frames beyond the data end are refused
        // when they cannot be proven un-covered — a v1 frame attests
        // nothing, and a surviving attestation at or past the data end is
        // proof the gap sat in covered territory. Otherwise they are the
        // legal remainder of a reorder hole in the un-covered suffix and
        // truncate with the torn tail (the retired ADR-0021 D3 refusal).
        if resume_evidence.valid_frames > 0 {
            let (evidence_segment, evidence_offset) =
                resume_evidence_anchor.expect("evidence anchor set with a valid frame");
            let corruption = |detail: String| {
                io_msg(
                    LogCorruption {
                        segment: resume_segment,
                        offset: resume_offset,
                        evidence_segment,
                        evidence_offset,
                        detail,
                    }
                    .to_string(),
                )
            };
            if resume_evidence.any_v1 {
                return Err(corruption(
                    "a format-v1 frame beyond the data end cannot attest whether the gap \
                     before it was fsync-covered (ADR-0031 D4)"
                        .to_owned(),
                ));
            }
            if resume_evidence.max_covered_lsn > resume.to_u64() {
                return Err(corruption(format!(
                    "a surviving frame attests fsync coverage up to {:#x}, past the valid \
                     data end {} — covered data was lost (ADR-0031 D4)",
                    resume_evidence.max_covered_lsn, resume
                )));
            }
            debug_assert!(torn, "beyond-frame evidence implies resume-region residue");
            self.stats.beyond_frames_discarded = resume_evidence.valid_frames;
        }
        Ok(ResumePoint {
            resume,
            torn,
            stale_trailing,
            beyond_max_epoch: resume_evidence.max_epoch,
        })
    }

    /// Evidence from the resume segment's slack and everything after it —
    /// the frames a truncation at the resume point would discard — with
    /// the first validating frame's position. Stale residue attests a
    /// discarded life's watermark, not this one's: excluded.
    fn resume_evidence(&self, last_data: usize) -> (RegionEvidence, Option<(SegmentId, u32)>) {
        let mut resume_evidence = RegionEvidence::default();
        let mut anchor = None;
        for (i, evidence) in self.evidence.iter().enumerate().skip(last_data) {
            if self.stale_slack[i] {
                continue;
            }
            if anchor.is_none()
                && let Some(offset) = evidence.first_valid
            {
                anchor = Some((self.segments[i], offset));
            }
            resume_evidence.absorb(evidence);
        }
        (resume_evidence, anchor)
    }

    /// The rest of the finish once replay's state is handed over: the
    /// begin guard, the torn-tail truncation, the boot GC and the rotor
    /// reopened at the resume point under the resumed life's epoch.
    fn resume_log(&mut self) -> io::Result<()> {
        let ResumePoint { resume, torn, stale_trailing, beyond_max_epoch } = self.resume_point()?;
        let last_data = self.last_data;
        // Everything the manifest names was durable at publication (the
        // watermark-≥-begin staging guard): a log ending below begin is
        // lost covered state, never a torn un-synced tail.
        if let Some(begin) = self.begin_lsn
            && resume < begin
        {
            return Err(io_msg(format!(
                "the log's valid data ends at {resume}, below the MANIFEST begin-LSN {begin}: \
                 fsync-covered bytes are missing — refusing to start"
            )));
        }
        if torn {
            self.stats.torn_truncated_at = Some(resume);
        }
        if torn || stale_trailing {
            // Every later segment is un-acked residue (frame-free, probed
            // and proven un-covered above, or a discarded life's) — remove
            // them so appends resume in the truncated segment with the
            // pristine-prealloc invariant restored.
            for &trailing in &self.segments[last_data + 1..] {
                self.fs().remove_file(&self.log_dir.join(segment_file_name(trailing)))?;
                self.stats.torn_segments_removed += 1;
            }
            if self.stats.torn_segments_removed > 0 {
                self.scan = Some(
                    scan_log_dir_from(&self.boot_reads, &self.log_dir, self.floor)
                        .map_err(io_invalid)?
                        .scan,
                );
            }
        }
        let tail_offset = resume.offset;

        // Boot GC: stale below-floor segments (crash mid-truncation) and
        // checkpoint-dir orphans (unnamed `.ick` from a crash before the
        // MANIFEST swap; `.ick.new` from a crashed walk). All are garbage
        // the named recovery unit fully covers; removal is safe in any
        // order and needs no dir-fsync (resurrection re-collects next
        // boot).
        for i in 0..self.stale.len() {
            let stale = self.stale[i];
            self.fs().remove_file(&self.log_dir.join(segment_file_name(stale)))?;
            self.stats.stale_files_removed += 1;
        }
        let named = self.manifest.as_ref().map(|m| m.ckpt_id);
        for name in self.fs().list_dir(&self.ckpt_dir)? {
            let stale_ick = parse_ick_file_name(&name).is_some_and(|id| Some(id) != named);
            if stale_ick || name.ends_with(".ick.new") {
                self.fs().remove_file(&self.ckpt_dir.join(name))?;
                self.stats.stale_files_removed += 1;
            }
        }

        let fs = self.fs.take().expect("fs present");
        let scan = self.scan.as_ref().expect("scan set");
        let mut rotor = SegmentRotor::open_existing(
            fs,
            self.log_dir.clone(),
            self.cfg.geometry,
            scan,
            tail_offset,
        )
        .map_err(io_invalid)?;
        // ADR-0031 D5: the resumed life's epoch tops every epoch observed
        // this boot — the valid prefix and every validating beyond-frame
        // (anything capable of resurrecting was durably whole, so the
        // audit saw it). Discarded-life residue that later resurfaces at
        // the new data end then fails the prefix's epoch-monotonicity rule
        // instead of replaying.
        // ADR-0090 D2 as amended: foreign-segment frames carry epochs at or
        // below the prefix's by append order (a recycled file was sealed
        // before every segment above the floor); folding them in makes the
        // "every epoch observed this boot" rule literal at the cost of one
        // max over the audited segments.
        let max_foreign_epoch =
            self.evidence.iter().map(|e| e.max_foreign_epoch).max().unwrap_or(0);
        let observed = self.max_prefix_epoch.max(beyond_max_epoch).max(max_foreign_epoch);
        let epoch = observed.checked_add(1).ok_or_else(|| {
            io_msg(format!("log epoch space exhausted at {observed} — refusing to start"))
        })?;
        rotor.set_resume_epoch(epoch);
        let seed = self
            .manifest
            .as_ref()
            .map(|m| RecoveredManifest { ckpt_id: m.ckpt_id, begin_lsn: m.begin_lsn });
        self.finished = Some((rotor, seed));
        self.phase = Phase::Complete;
        let phases = &mut self.stats.phases;
        phases.replay_bytes = self.bytes_consumed.saturating_sub(phases.ckpt_bytes);
        phases.replay_frames = self.stats.frames;
        Ok(())
    }
}

/// Where the log resumes after replay (the finish's evidence verdict).
struct ResumePoint {
    resume: Lsn,
    /// This life's residue lies behind the resume point.
    torn: bool,
    /// A discarded life's residue lies behind the resume point.
    stale_trailing: bool,
    /// The top epoch of the validating frames beyond the data end.
    beyond_max_epoch: u32,
}

/// The `open_cell_log` replay-failure message (kept byte-compatible with
/// the pre-S15 `ApplyError::Apply` surface the S13/S14 suites pin).
fn replay_apply_failed(at: Lsn, error: &io::Error) -> io::Error {
    io_msg(format!("replay apply failed at {at}: {error}"))
}

/// A tiered boot machine's typed refusal as the recovery fail-stop,
/// naming the namespace and the check (ADR-0174 D1).
fn replay_refused(ns: inf_log::NsId, refusal: &inf_store::ReplayRefusal) -> io::Error {
    io_msg(format!("tiered ns {}: boot replay refused: {refusal}", ns.0))
}

fn io_invalid(err: impl std::fmt::Debug) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("{err:?}"))
}

fn io_msg(message: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each `INFO persistence` line prints the field its name says: every
    /// field set by name to a distinct value, the expected lines written
    /// out by hand. A name paired with another field in
    /// [`TierReplayStats::FIELDS`] is red here, not a mislabelled line.
    #[test]
    fn every_tier_replay_field_prints_under_its_own_name() {
        let stats = TierReplayStats {
            counters: inf_store::ReplayCounters {
                demote_steps: 1,
                pads_placed: 2,
                tier_bytes: 3,
                barriers: 4,
                files_sealed: 5,
                settle_reads: 6,
                settled_same_key: 7,
                settled_distinct: 8,
                deletes_verified: 9,
                blob_releases: 10,
                markers_skipped: 11,
            },
            dead_life_files_removed: 12,
            step_charge_bytes_max: 13,
        };
        let names = TierReplayStats::FIELDS.iter().map(|(name, _)| *name);
        let printed: Vec<(&str, u64)> = names.zip(stats.to_array()).collect();
        let expected = [
            ("recover_node_tier_demote_steps", 1),
            ("recover_node_tier_pads_placed", 2),
            ("recover_node_tier_bytes_written", 3),
            ("recover_node_tier_barriers", 4),
            ("recover_node_tier_files_sealed", 5),
            ("recover_node_tier_settle_reads", 6),
            ("recover_node_tier_settled_same_key", 7),
            ("recover_node_tier_settled_distinct", 8),
            ("recover_node_tier_deletes_verified", 9),
            ("recover_node_tier_blob_releases", 10),
            ("recover_node_tier_markers_skipped", 11),
            ("recover_node_tier_dead_life_files_removed", 12),
            ("recover_node_tier_step_charge_bytes_max", 13),
        ];
        assert_eq!(printed, expected);
        assert_eq!(TierReplayStats::from_array(stats.to_array()), stats, "the board round-trips");
    }
}
