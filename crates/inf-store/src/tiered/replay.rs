//! Boot replay of one tiered namespace on one cell (ADR-0174): the replay
//! state machine, the demote step that makes room when the RAM window
//! fills (D2), the settle walk that keeps a sealed record from leaving a
//! same-key cold slot behind (D3 R7), the end-of-replay cursor (R10), the
//! end-of-checkpoint blob release (R9) and the hand-over to the plane.
//! Every replay append and delete enters through the table's `replay_*`
//! entries, which hold the demote machinery: no tiered replay append
//! exists outside them (D1); a machine reaches them only through
//! `Keyspace::apply_record`'s [`ReplaySpill`] seam, the lent forms being
//! crate-private; the window's refusal is a [`Room`], never an error variant,
//! and every boot settle answers from a [`ColdKey`] — a key that hashes
//! to the slot's hash — read through the file's held creation-mode handle
//! (D3).
//!
//! The machine is one [`TierReplay`] per recovered namespace, owned by
//! the recovery driver and lent to the keyspace's replay arms through the
//! [`ReplaySpill`] seam; the table stays where it serves from. State ×
//! event is the record's table: `Seeded` (recovered, `tail = origin`) →
//! `Fitting` (this-life records, nothing sealed) → `Spilling` (a demote
//! step began) → `Settling` (replay ended while spilling; the cursor walks
//! `[ro, tail)`) → handed over. A typed [`ReplayRefusal`] drops the
//! machine: the boot's refusal, terminal for this boot.

use super::*;

use inf_log::flush::{BootFlush, HandedOver, SeamFlush, SettleReadError, SettleWindow};

use crate::address_space::{Room, WindowFull};
use crate::limits::{REPLAY_ROOM_ASKS_MAX, SETTLE_READ_CHARGE_BYTES, SETTLE_WALK_BYTE_CHARGE};
use crate::record::{ColdKey, ColdKeyError};
use crate::tiered::shadow::SettleCase;
use crate::tiered_recover::IdChunks;

/// Where the machine stands. Handed over is the machine consumed by
/// [`TierReplay::hand_over`]; refused, the error that dropped it.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum ReplayState {
    /// Recovered through its manifest section (or the empty one): files
    /// mapped, pipeline built, `tail = origin`.
    Seeded,
    /// This-life records; `ro = origin`: nothing sealed, no boot file.
    Fitting,
    /// At least one demote step began (entered before the step's first
    /// read or write).
    Spilling,
    /// Replay ended while `Spilling`; `cursor` is the end-of-replay
    /// settle's position in `[ro, tail]`.
    Settling { cursor: LogicalAddr },
}

/// The end of the checkpoint's release list (ADR-0174 R9, A1.3): 8 B per
/// ref a boot settle removed while the checkpoint loaded — before the 0x05
/// entry that would release its blob reference could exist — in settle
/// order, with no cap of its own: it is bounded by the origin map it
/// shadows. Open while the checkpoint loads, closed at its end (walked by
/// index, nothing appends), released once walked.
#[derive(Debug)]
enum ReleaseList {
    Loading(IdChunks),
    Releasing(IdChunks),
    Released,
}

/// The machine's phase, for the boot line and the oracles.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ReplayPhase {
    Seeded,
    Fitting,
    Spilling,
    Settling,
}

/// Replay's own counters (ADR-0174 D6), per namespace per cell; the
/// recovery driver folds them per cell. Every field but `markers_skipped`
/// is the zero set: zero on a boot that did not demote.
#[derive(Copy, Clone, Default, Debug, PartialEq, Eq)]
pub struct ReplayCounters {
    /// Demote steps (D2).
    pub demote_steps: u64,
    /// Pads placed (D2 rule 6).
    pub pads_placed: u64,
    /// Record bytes replay appended to tier files.
    pub tier_bytes: u64,
    /// Barriers of the demote step and the hand-over drain.
    pub barriers: u64,
    /// Files the boot pipeline sealed (not the `Recovered` reseal).
    pub files_sealed: u64,
    /// Settle reads of a replayed `DEL` (R6) and of the seal and end
    /// settles (R7, R10) — not the rebuild's.
    pub settle_reads: u64,
    /// Slots those reads settled as the same key.
    pub settled_same_key: u64,
    /// Slots those reads kept as a distinct key.
    pub settled_distinct: u64,
    /// Cold slots a replayed `DEL` verified and removed (R6).
    pub deletes_verified: u64,
    /// Blob references released by a replay settle (R8, R9).
    pub blob_releases: u64,
    /// Markers naming an address at or above the life origin (R4) —
    /// outside the zero set: a fitting boot counts them too.
    pub markers_skipped: u64,
}

/// Boot I/O the machine did since its owner last drained it: what the
/// recovery driver charges to its step budget — the settle reads and the
/// bytes the seal and end settle walks passed at
/// [`settle_charge_bytes`](Self::settle_charge_bytes), the prices' one
/// home, which both walks yield on too; the tier bytes and barriers at the
/// driver's prices.
#[derive(Copy, Clone, Default, Debug, PartialEq, Eq)]
pub struct ReplayWork {
    /// Record bytes appended to tier files.
    pub tier_bytes: u64,
    /// Barriers made.
    pub barriers: u64,
    /// Settle reads made.
    pub settle_reads: u64,
    /// Bytes walked by a demote step's seal walk and the end-of-replay
    /// settle.
    pub walked_bytes: u64,
}

impl ReplayWork {
    /// Nothing to charge.
    #[must_use]
    pub fn is_zero(self) -> bool {
        self == ReplayWork::default()
    }

    /// The settle reads and the walked bytes at their prices,
    /// [`SETTLE_READ_CHARGE_BYTES`] and [`SETTLE_WALK_BYTE_CHARGE`]: what
    /// the end settle walk yields on and what the recovery driver charges
    /// for them. Saturating: a yield test, never an account.
    #[must_use]
    pub fn settle_charge_bytes(self) -> u64 {
        self.walked_bytes
            .saturating_mul(SETTLE_WALK_BYTE_CHARGE)
            .saturating_add(self.settle_reads.saturating_mul(SETTLE_READ_CHARGE_BYTES))
    }
}

impl ReplayCounters {
    /// Adds `other` field by field — the recovery driver's fold of a
    /// cell's namespaces (ADR-0174 D6). Saturating: counters, not money.
    pub fn absorb(&mut self, other: ReplayCounters) {
        let ReplayCounters {
            demote_steps,
            pads_placed,
            tier_bytes,
            barriers,
            files_sealed,
            settle_reads,
            settled_same_key,
            settled_distinct,
            deletes_verified,
            blob_releases,
            markers_skipped,
        } = other;
        self.demote_steps = self.demote_steps.saturating_add(demote_steps);
        self.pads_placed = self.pads_placed.saturating_add(pads_placed);
        self.tier_bytes = self.tier_bytes.saturating_add(tier_bytes);
        self.barriers = self.barriers.saturating_add(barriers);
        self.files_sealed = self.files_sealed.saturating_add(files_sealed);
        self.settle_reads = self.settle_reads.saturating_add(settle_reads);
        self.settled_same_key = self.settled_same_key.saturating_add(settled_same_key);
        self.settled_distinct = self.settled_distinct.saturating_add(settled_distinct);
        self.deletes_verified = self.deletes_verified.saturating_add(deletes_verified);
        self.blob_releases = self.blob_releases.saturating_add(blob_releases);
        self.markers_skipped = self.markers_skipped.saturating_add(markers_skipped);
    }

    /// Whether every counter of the zero set is zero — a boot that did
    /// not demote (D6); `markers_skipped` is outside the set.
    #[must_use]
    pub fn zero_set_is_zero(&self) -> bool {
        ReplayCounters { markers_skipped: 0, ..*self } == ReplayCounters::default()
    }
}

/// The replay seam: lends a namespace's boot machine to the keyspace's
/// tiered replay arms, one lookup per record. The machine accumulates
/// the boot I/O it does ([`ReplayWork`]); the seam's owner drains it with
/// [`TierReplay::take_work`] where it asks its step budget — after a
/// record whose answer was priced, after a demote step, at the end of a
/// unit — so a record that does no priced work pays nothing for the
/// charge. `None` is a namespace with no machine:
/// its replay places what fits and refuses typed where a demote would be
/// needed (ADR-0174 D4 gives every tiered namespace a machine).
pub trait ReplaySpill {
    /// The pipelines' filesystem.
    type Fs: SegmentFs;
    /// The namespace's machine, if it has one.
    fn replay_mut(&mut self, ns: inf_log::NsId) -> Option<&mut TierReplay<Self::Fs>>;
}

/// A seam with no machine behind it — keyspaces whose tiered namespaces
/// have no boot pipeline (memory-only keyspaces, audits, the routing
/// tests): every tiered replay places what fits.
#[derive(Copy, Clone, Debug, Default)]
pub struct NoSpill;

impl ReplaySpill for NoSpill {
    type Fs = inf_log::fs::StdSegmentFs;

    fn replay_mut(&mut self, _ns: inf_log::NsId) -> Option<&mut TierReplay<Self::Fs>> {
        None
    }
}

/// A typed boot refusal from replay: the recovery fail-stop class, naming
/// the check. Nothing of the refusing record was applied; a refusal from
/// inside a demote step leaves the table exact (the boundary and the
/// cursor never passed an unread record).
#[derive(Debug)]
pub enum ReplayRefusal {
    /// A length refusal `append` makes, before room (D1), naming the
    /// bound: the key, the value, the blob threshold, half the ring, the
    /// blob maximum or the admission cost.
    TooLarge(LengthRefusal),
    /// `room` answered `End`: the 48-bit end of the space.
    End { len: usize },
    /// A fifth room ask for one record.
    RoomAsks { len: usize, asks: u32 },
    /// The namespace has no boot pipeline and the record needs room.
    NoPipeline { need: Room },
    /// `pad_tail` refused the pad `room` answered (the pair check).
    PadRefused { to: LogicalAddr },
    /// The demote step's release could not reach its target.
    DemoteStalled { head: LogicalAddr, target: LogicalAddr },
    /// The boot pipeline's flush, seal or create failed (the demote
    /// step, the hand-over drain): a short write, the device full, a
    /// barrier, a create refused. `unplaced_bytes` are the sealed bytes
    /// no barrier covers yet; `handles_held` the catalogue handles the
    /// pipeline holds.
    Flush { cause: TierFlushError, unplaced_bytes: u64, handles_held: usize },
    /// A settle read failed (R6, R7, R10).
    SettleRead { addr: LogicalAddr, cause: SettleReadError },
    /// Settle bytes do not parse into a verified record of the slot's
    /// hash — never "distinct".
    SettleIdentity { addr: LogicalAddr, cause: ColdKeyError },
    /// A same-key settle would exceed the survivor's origin room.
    OriginRoom { winner: LogicalAddr, cold: LogicalAddr },
    /// A settle step before the end of replay was declared.
    ReplayNotEnded,
    /// A record or a `DEL` after the end of replay was declared.
    ReplayEnded,
    /// A hand-over with RAM records unsettled (R10).
    Unsettled { cursor: Option<LogicalAddr>, tail: LogicalAddr },
    /// The store refused the placement after `room` answered `Fits` —
    /// disk admission is open at boot and the window was just made, so
    /// this names a defect, typed.
    Store(OpError),
    /// A record waiting for room found its demote step's facts changed
    /// between two steps: `room` answering other than the target while the
    /// head is below it, the boundary moved under the settled span, another
    /// record presented, or a demote step with no record waiting (ADR-0174
    /// A1). A defect class, as `RoomAsks` is: no input reaches it.
    RoomMoved { why: &'static str },
}

impl core::fmt::Display for ReplayRefusal {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            ReplayRefusal::TooLarge(refusal) => write!(f, "{refusal}"),
            ReplayRefusal::End { len } => {
                write!(f, "no room for {len} bytes: the 48-bit end of the address space")
            }
            ReplayRefusal::RoomAsks { len, asks } => write!(
                f,
                "room asked {asks} times for {len} bytes \
                 (ADR-0174 D2 rule 1 bounds {REPLAY_ROOM_ASKS_MAX})"
            ),
            ReplayRefusal::NoPipeline { need } => write!(
                f,
                "the namespace has no boot pipeline and the record needs {need:?} (ADR-0174 D4)"
            ),
            ReplayRefusal::PadRefused { to } => {
                write!(f, "the pad to {} exceeds the window", to.to_raw())
            }
            ReplayRefusal::DemoteStalled { head, target } => write!(
                f,
                "the demote step left the head at {} below its target {}",
                head.to_raw(),
                target.to_raw()
            ),
            ReplayRefusal::Flush { cause, unplaced_bytes, handles_held } => write!(
                f,
                "boot tier flush: {cause} ({unplaced_bytes} bytes unplaced, \
                 {handles_held} tier-file handles held)"
            ),
            ReplayRefusal::SettleRead { addr, cause } => {
                write!(f, "settle read of the slot at {}: {cause}", addr.to_raw())
            }
            ReplayRefusal::SettleIdentity { addr, cause } => write!(
                f,
                "the slot at {} fails the identity check: {cause} (ADR-0174 D3)",
                addr.to_raw()
            ),
            ReplayRefusal::OriginRoom { winner, cold } => write!(
                f,
                "the cold record at {} is a same-key twin of {} with no origin room \
                 (RELOC_ORIGIN_CAP)",
                cold.to_raw(),
                winner.to_raw()
            ),
            ReplayRefusal::ReplayNotEnded => {
                write!(f, "a settle step before the end of replay was declared")
            }
            ReplayRefusal::ReplayEnded => {
                write!(f, "a replayed record after the end of replay was declared")
            }
            ReplayRefusal::Unsettled { cursor, tail } => write!(
                f,
                "hand-over with RAM records unsettled (cursor {:?}, tail {}) — ADR-0174 R10",
                cursor.map(LogicalAddr::to_raw),
                tail.to_raw()
            ),
            ReplayRefusal::Store(e) => write!(f, "the store refused a fitted placement: {e:?}"),
            ReplayRefusal::RoomMoved { why } => {
                write!(f, "a record waiting for room found its demote step moved: {why}")
            }
        }
    }
}

impl std::error::Error for ReplayRefusal {}

/// What a marker did (R3, R4).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Displaced {
    /// The exact pair below the origin was slotted and is removed.
    Removed,
    /// The pair below the origin was not slotted — a legal interleaving.
    Absent,
    /// The marker names an address at or above the life origin: a
    /// crashed-life address, nothing in this life (ADR-0174 R4).
    AboveOrigin,
}

/// The settle walk's receipt (R7): the boundary advances only to the end
/// of a span whose every live record was settled against its cold twins,
/// so no RAM record below it has a same-key cold slot. `from` is where the
/// walk began: a seal advances the boundary on a span only from the
/// boundary itself, so a walk the step budget stopped resumes on its own
/// receipt, never on a bare address (ADR-0174 A1.3). Only
/// [`TierReplay::settle_walk`] makes one or lengthens one.
#[must_use = "a settled span is what the boundary may advance to"]
#[derive(Debug)]
pub struct SettledSpan {
    from: LogicalAddr,
    end: LogicalAddr,
}

/// Where a settle walk starts: at an address, or at the end of a span an
/// earlier walk of the same seal settled, which it lengthens.
#[derive(Debug)]
enum WalkStart {
    At(LogicalAddr),
    Extend(SettledSpan),
}

/// Where a settle walk goes: a demote step's seal walk to a target
/// address under the budget left, or one end-of-replay step to a budget.
#[derive(Copy, Clone, Debug)]
enum WalkTo {
    /// To the first record start at or above `stop` (D2 rule 2), or until
    /// its charge reaches `budget_bytes` (ADR-0174 A1.3).
    Seal { stop: u64, budget_bytes: u64 },
    /// Until the step's charge reaches `budget_bytes` (R10).
    Settle { budget_bytes: u64 },
}

/// How a settle walk ended.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum WalkEnd {
    /// At the tail: every record from the walk's start is settled.
    Tail,
    /// A seal walk at its stop, short of the tail.
    AtStop,
    /// The charge reached the budget after a record, short of the tail.
    Spent,
}

/// A record waiting for room (ADR-0174 D2, A1.3): the target `room`
/// answered, the room asks the record made, counted across suspensions
/// (rule 1), the record's length, and the span the seal walk has settled
/// so far, the walk's own receipt.
#[derive(Debug)]
struct PendingRoom {
    target: LogicalAddr,
    asks: u32,
    len: usize,
    settled: Option<SettledSpan>,
}

/// What one [`TierReplay::demote_step`] reports.
#[must_use = "a demote step that answers More has not made the room"]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum DemoteProgress {
    /// The seal walk stopped at the budget left; call again.
    More,
    /// The head reached the target: present the record again.
    Room,
}

/// What the room question answered a record (D2 rule 1).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum RoomVerdict {
    /// The record fits: place it.
    Fits,
    /// A demote step is under way for it: the record changed nothing.
    NeedsRoom,
}

/// What a placement changes on the machine once its record is placed: no
/// more, the first record's `Seeded` → `Fitting`, or the waiting record's
/// pending room cleared — read once, from the state, before the room
/// question.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum PlaceEntry {
    Plain,
    Seeded,
    Waiting,
}

/// What a replayed upsert did: placed at an address, or waiting for room
/// with nothing of it applied.
#[must_use = "a record that needs room was not placed"]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum Placement {
    Placed(LogicalAddr),
    NeedsRoom,
}

/// Whether a replayed `DEL` read cold slots (R6): the boot work its answer
/// prices.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum DeleteReads {
    None,
    Read,
}

/// What one end-of-replay settle step reports (R10).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum SettleProgress {
    /// The cursor stopped at the budget; call again.
    More,
    /// The cursor is at the tail: every RAM record is settled.
    Done,
}

/// One recovered tiered namespace's boot replay machine.
pub struct TierReplay<F: SegmentFs> {
    state: ReplayState,
    flush: BootFlush<F>,
    /// `MAINTAIN-SLICE` in whole commit pages, at least one (D2 rule 2).
    lead: u64,
    counters: ReplayCounters,
    /// I/O since the last [`take_work`](Self::take_work).
    work: ReplayWork,
    /// Scratch: the exact-hash cold slots of the record being settled.
    twins: Vec<LogicalAddr>,
    /// Scratch: a `DEL`'s verified same-key cold slots, `(address, len)`.
    doomed: Vec<(LogicalAddr, u32)>,
    /// The settled refs' addresses the end of the checkpoint releases
    /// (ADR-0174 R9, A1.3), in settle order.
    releases: ReleaseList,
    /// Scratch: the key of the record being settled (its RAM bytes may
    /// move under the settle).
    key: Vec<u8>,
    /// The record waiting for room, while a demote step is under way. Set
    /// only by [`begin_demote`](Self::begin_demote), which enters
    /// `Spilling`, so a machine in any other state holds none and its
    /// replay entries read it only in `Spilling`.
    pending: Option<PendingRoom>,
}

impl<F: SegmentFs> TierReplay<F> {
    /// A machine over a recovered boot pipeline, `Seeded`, for `table`:
    /// `lead` is the table's `MAINTAIN-SLICE` in whole commit pages of its
    /// own space, read here, so no caller hands the machine a page or a
    /// slice the table it demotes does not have.
    #[must_use]
    pub fn new(flush: BootFlush<F>, table: &TieredTable) -> TierReplay<F> {
        let page_bytes = table.space.page_bytes();
        debug_assert!(page_bytes.is_power_of_two(), "the space's commit page");
        let lead = table.demotion().slice_bytes.div_ceil(page_bytes).max(1) * page_bytes;
        TierReplay {
            state: ReplayState::Seeded,
            flush,
            lead,
            counters: ReplayCounters::default(),
            work: ReplayWork::default(),
            twins: Vec::new(),
            doomed: Vec::new(),
            releases: ReleaseList::Loading(IdChunks::new()),
            key: Vec::with_capacity(crate::record::MAX_KEY_LEN),
            pending: None,
        }
    }

    /// Whether a record is waiting for room: a demote step is under way, or
    /// made the room and the record is not placed yet. Never at the end of
    /// replay — every record a walk presents is applied before the walk
    /// moves on.
    #[must_use]
    pub fn waits_for_room(&self) -> bool {
        self.pending.is_some()
    }

    /// The machine's phase.
    #[must_use]
    pub fn phase(&self) -> ReplayPhase {
        match self.state {
            ReplayState::Seeded => ReplayPhase::Seeded,
            ReplayState::Fitting => ReplayPhase::Fitting,
            ReplayState::Spilling => ReplayPhase::Spilling,
            ReplayState::Settling { .. } => ReplayPhase::Settling,
        }
    }

    /// Replay's counters so far.
    #[must_use]
    pub fn counters(&self) -> ReplayCounters {
        self.counters
    }

    /// The I/O since the last call, and zero again.
    pub fn take_work(&mut self) -> ReplayWork {
        core::mem::take(&mut self.work)
    }

    /// The boot pipeline's sealed catalogue (manifested and boot-sealed).
    #[must_use]
    pub fn sealed(&self) -> &[TierFileMeta] {
        self.flush.sealed()
    }

    /// The boot pipeline's active file, if one is open.
    #[must_use]
    pub fn active(&self) -> Option<(u32, LogicalAddr, u64, u64, &std::path::Path)> {
        self.flush.active()
    }

    /// `lead` in bytes (tests).
    #[must_use]
    pub fn lead_bytes(&self) -> u64 {
        self.lead
    }

    /// The settle read for ADR-0093's rebuild (R10): the key window of
    /// the record at `addr` through the held handle, as replay's own
    /// settles read it.
    ///
    /// # Errors
    /// [`SettleReadError`] — the recovery fail-stop class.
    pub fn read_key_window(
        &mut self,
        addr: LogicalAddr,
    ) -> Result<SettleWindow<'_>, SettleReadError> {
        self.work.settle_reads += 1;
        self.flush.read_key_window(addr.to_raw())
    }

    // ---- the demote step (ADR-0174 D2) ----

    /// Step 0 of a demote step, at the room question that answered
    /// `Demote(target)` — after the length refusals and before the marker
    /// drain (ADR-0174 D1): the record waits for room with `asks` asks made and
    /// has changed nothing; the machine enters `Spilling` and counts the
    /// step once, before its first read or write.
    fn begin_demote(&mut self, target: LogicalAddr, asks: u32, len: usize) {
        self.state = ReplayState::Spilling;
        self.counters.demote_steps += 1;
        self.pending = Some(PendingRoom { target, asks, len, settled: None });
    }

    /// Makes room for the record waiting for it, under `left` budget bytes
    /// (ADR-0174 D2, A1.3): seal one lead past the target with every sealed
    /// record settled (rules 2, 3), one flush with one barrier that the
    /// boot pipeline claims whole (rule 4), release until the head reaches
    /// the target. Only the seal walk resumes: it charges every walked byte
    /// and every read, and once its charge reaches `left` after a record it
    /// keeps the span it settled and answers `More`, the boundary unmoved;
    /// the flush and the release run whole once the walk reaches its stop.
    /// A completed step is one seal, one flush cut and one barrier. `Room`
    /// once the head reached the target; the record is then presented
    /// again and keeps its asks until it is placed.
    ///
    /// # Errors
    /// The walk's settle refusals, the flush's, `DemoteStalled`, and
    /// `RoomMoved` with no record waiting or when the boundary moved under
    /// the settled span.
    pub fn demote_step(
        &mut self,
        table: &mut TieredTable,
        left: u64,
    ) -> Result<DemoteProgress, ReplayRefusal> {
        let Some(mut pending) = self.pending.take() else {
            return Err(ReplayRefusal::RoomMoved { why: "a demote step with no record waiting" });
        };
        let target = pending.target;
        if table.space.flushed() < target {
            let start = match pending.settled.take() {
                Some(span) if span.from == table.space.ro_boundary() => WalkStart::Extend(span),
                Some(_) => {
                    return Err(ReplayRefusal::RoomMoved {
                        why: "the boundary moved under the span",
                    });
                }
                None => WalkStart::At(table.space.ro_boundary()),
            };
            let stop = target.to_raw().saturating_add(self.lead);
            // The planted canary walks the whole seal in one call, as a
            // demote step did inside one record's apply.
            let budget_bytes =
                if cfg!(inf_canary_recover_seal_unbudgeted) { u64::MAX } else { left.max(1) };
            // Bound: the charge left plus one record and its exact-hash
            // group's reads.
            let (span, end) =
                self.settle_walk(table, start, WalkTo::Seal { stop, budget_bytes })?;
            if end == WalkEnd::Spent {
                pending.settled = Some(span);
                self.pending = Some(pending);
                return Ok(DemoteProgress::More);
            }
            table.seal_settled(span);
            self.flush_cut(table)?;
        }
        // Bound: ⌈(target − head) ÷ slice⌉ releases, whole pages each.
        while table.space.head() < target {
            if table.release_slice() == 0 {
                return Err(ReplayRefusal::DemoteStalled { head: table.space.head(), target });
            }
        }
        self.pending = Some(pending);
        Ok(DemoteProgress::Room)
    }

    /// Rule 4 after the seal: the flush over `[flush cursor, ro)`, one unit.
    fn flush_cut(&mut self, table: &mut TieredTable) -> Result<(), ReplayRefusal> {
        let cut = table.space.ro_boundary().to_raw();
        let cursor = table.flush_start_cursor(&self.flush);
        // One span, one barrier, one more per gap or capacity seal it
        // crosses. A boot seals a file at the capacity target, before a
        // gap and once at the hand-over (ADR-0174 D2 rule 5), so for `D`
        // the span the boot flushed (`flushed` − life origin, holes and
        // pads included) and `C` the file capacity it seals at most
        // ⌊2D ÷ C⌋ + ⌈D ÷ ring⌉ + page pads + 1 files: a capacity-sealed
        // file and the range that overflowed it hold more than `C`, a
        // file is in at most two such pairs (⌊D ÷ (C − S)⌋ when `S`, the
        // longest range one demote step appends, is below `C`), and ring
        // tops lie a ring apart. No limit of their own: the device and
        // the handle limit bound them (a typed refusal at a create).
        if cut > cursor {
            let outcome = table
                .flush_span(&mut self.flush, cut - cursor)
                .map_err(|cause| self.flush_refusal(table, cause))?;
            self.note_flush(outcome);
        }
        // The planted canary breaks D2 rule 5: the step seals the file
        // to free the partial frame, as the live stall seal does — a
        // boot file with the stall reason.
        #[cfg(inf_canary_replay_stall_seal)]
        {
            let planted = self.flush.seal_stall_planted();
            planted.map_err(|cause| self.flush_refusal(table, cause))?;
        }
        Ok(())
    }

    /// The typed refusal for a boot pipeline failure: what is sealed but
    /// not under a barrier, and the handles the pipeline holds.
    fn flush_refusal(&self, table: &TieredTable, cause: TierFlushError) -> ReplayRefusal {
        let unplaced_bytes = table.space.ro_boundary().to_raw() - table.space.flushed().to_raw();
        ReplayRefusal::Flush { cause, unplaced_bytes, handles_held: self.flush.held_handles() }
    }

    fn note_flush(&mut self, outcome: FlushSliceOutcome) {
        let barriers = u64::from(outcome.files_sealed) + u64::from(outcome.appended_bytes > 0);
        self.counters.tier_bytes += outcome.appended_bytes;
        self.counters.barriers += barriers;
        self.counters.files_sealed += u64::from(outcome.files_sealed);
        self.work.tier_bytes += outcome.appended_bytes;
        self.work.barriers += barriers;
    }

    /// Walks the records from `start`, settling every live one against
    /// its exact-hash cold slots (R7), to where `to` says: a demote
    /// step's seal walk stops at the first record start at or above its
    /// target; either walk yields after the record whose charge — its
    /// walked bytes and settle reads at [`ReplayWork::settle_charge_bytes`]
    /// — reaches its budget. Either ends at the tail. A hole is passed
    /// whole by its mark. Returns the span settled — from `start`'s address,
    /// or the span it extends — and how the walk ended; the only maker and
    /// the only extender of a [`SettledSpan`].
    ///
    /// Bound per call: the budget plus one record's bytes and its twins'
    /// reads (an exact-hash group); a seal walk passes at most
    /// `H + lead − ro` bytes plus one record over all its calls.
    fn settle_walk(
        &mut self,
        table: &mut TieredTable,
        start: WalkStart,
        to: WalkTo,
    ) -> Result<(SettledSpan, WalkEnd), ReplayRefusal> {
        let tail = table.space.tail().to_raw();
        let (from, mut at) = match start {
            WalkStart::At(addr) => (addr, addr.to_raw()),
            WalkStart::Extend(span) => (span.from, span.end.to_raw()),
        };
        let budget_bytes = match to {
            WalkTo::Seal { budget_bytes, .. } | WalkTo::Settle { budget_bytes } => budget_bytes,
        };
        let mut walked = 0u64;
        let reads_before = self.counters.settle_reads;
        let end = loop {
            if at >= tail {
                break WalkEnd::Tail;
            }
            let here = LogicalAddr::from_raw(at).expect("watermarks stay 48-bit");
            if let Some(hole) = table.space.hole_at(here) {
                at += hole;
                continue;
            }
            if let WalkTo::Seal { stop, .. } = to
                && at >= stop
            {
                break WalkEnd::AtStop;
            }
            let (len, hash) = {
                let parts = table.record(here);
                (parts.encoded_len as u64, table.hash_key(parts.key))
            };
            // The planted canary breaks R7 at the seal: a sealed record
            // leaves its same-key cold slot.
            let skip = matches!(to, WalkTo::Seal { .. }) && cfg!(inf_canary_replay_seal_no_settle);
            if !skip && self.collect_twins(table, here, hash) {
                self.key.clear();
                self.key.extend_from_slice(table.record(here).key);
                self.settle_twins(table, here, hash)?;
            }
            at += len;
            walked += len;
            let settle_reads = self.counters.settle_reads - reads_before;
            let walk = ReplayWork { walked_bytes: walked, settle_reads, ..ReplayWork::default() };
            if walk.settle_charge_bytes() >= budget_bytes {
                break if at >= tail { WalkEnd::Tail } else { WalkEnd::Spent };
            }
        };
        // Both walks' bytes are charged: the seal walk's as the end
        // settle's (ADR-0174 A1.2).
        self.work.walked_bytes += walked;
        let end_addr = LogicalAddr::from_raw(at).expect("watermarks stay 48-bit");
        Ok((SettledSpan { from, end: end_addr }, end))
    }

    /// One pass over the exact-hash group of the record at `here`: whether
    /// the record is slotted (live), with its cold siblings collected
    /// into `twins`. True when a live record has a cold sibling to settle.
    fn collect_twins(&mut self, table: &TieredTable, here: LogicalAddr, hash: u64) -> bool {
        self.twins.clear();
        let mut live = false;
        let space = &table.space;
        let twins = &mut self.twins;
        table.index.each_exact(hash, |sibling| {
            if sibling == here {
                live = true;
            } else if space.resolve(sibling) == AddrClass::Cold {
                twins.push(sibling);
            }
        });
        live && !self.twins.is_empty()
    }

    /// R7 for the live RAM record at `winner` (its key in `self.key`,
    /// its cold siblings in `twins`): each is read through the held
    /// handle, parsed into a `ColdKey` under the slot's hash, and settled
    /// when it carries the winner's key — as a ref (counted, stamped,
    /// chained, no bytes) below the origin, with its exact death above it
    /// (R8); a distinct key stays.
    fn settle_twins(
        &mut self,
        table: &mut TieredTable,
        winner: LogicalAddr,
        hash: u64,
    ) -> Result<(), ReplayRefusal> {
        // Bound: one read per cold slot of the exact-hash group — under the
        // keyed hash no client grows the group (ADR-0094).
        for i in 0..self.twins.len() {
            let cold = self.twins[i];
            let window = self
                .flush
                .read_key_window(cold.to_raw())
                .map_err(|cause| ReplayRefusal::SettleRead { addr: cold, cause })?;
            self.counters.settle_reads += 1;
            self.work.settle_reads += 1;
            let twin = ColdKey::from_window(window.bytes, window.left, hash, |k| table.hash_key(k))
                .map_err(|cause| ReplayRefusal::SettleIdentity { addr: cold, cause })?;
            if twin.key() != self.key.as_slice() {
                self.counters.settled_distinct += 1;
                continue;
            }
            let case = SettleCase::at_boot(cold, table.space.life_origin(), twin.record_len());
            if !table.settle_pair(hash, cold.to_raw(), winner.to_raw(), case) {
                return Err(ReplayRefusal::OriginRoom { winner, cold });
            }
            // A ref settled while the checkpoint loads precedes the 0x05
            // entry that would release its blob reference (R9's list). The
            // list names each address once: a settle collects only a slotted
            // pair and removes it, and no later ref slots it again (R2) — one
            // probe of the index, not a walk of the list.
            if let (SettleCase::RefAtBoot, ReleaseList::Loading(list)) = (case, &mut self.releases)
            {
                debug_assert!(
                    !table.index.contains_pair(hash, cold),
                    "a listed ref left the index"
                );
                list.push(cold.to_raw());
            }
            self.counters.settled_same_key += 1;
        }
        Ok(())
    }

    /// R6's reads for a `DEL` of `key`: every exact-hash cold slot at or
    /// above the life origin — a record this boot demoted — is read and
    /// parsed; the ones carrying the key are remembered with their exact
    /// length for the removal that follows the marker drain. Refs below
    /// the origin are the markers' (R3).
    fn verify_cold_for_delete(
        &mut self,
        table: &TieredTable,
        key: &[u8],
        hash: u64,
    ) -> Result<(), ReplayRefusal> {
        self.doomed.clear();
        if cfg!(inf_canary_replay_del_no_verify) {
            // The planted canary breaks R6: the DEL reads nothing and its
            // key's demoted copy stays.
            return Ok(());
        }
        self.twins.clear();
        let (space, twins) = (&table.space, &mut self.twins);
        let origin = space.life_origin();
        table.index.each_exact(hash, |sibling| {
            if sibling >= origin && space.resolve(sibling) == AddrClass::Cold {
                twins.push(sibling);
            }
        });
        // Bound: one read per this-life cold slot of the exact-hash group.
        for i in 0..self.twins.len() {
            let cold = self.twins[i];
            let window = self
                .flush
                .read_key_window(cold.to_raw())
                .map_err(|cause| ReplayRefusal::SettleRead { addr: cold, cause })?;
            self.counters.settle_reads += 1;
            self.work.settle_reads += 1;
            let twin = ColdKey::from_window(window.bytes, window.left, hash, |k| table.hash_key(k))
                .map_err(|cause| ReplayRefusal::SettleIdentity { addr: cold, cause })?;
            if twin.key() == key {
                self.doomed.push((cold, twin.record_len()));
            } else {
                self.counters.settled_distinct += 1;
            }
        }
        Ok(())
    }

    // ---- the end of the checkpoint, the end of replay, the hand-over ----

    /// R9, at the end of the checkpoint and before the tail, run to
    /// completion: the list closes, every listed address releases its blob
    /// reference, and the list is freed ([`close_checkpoint`],
    /// [`release_from`] and [`end_releases`] are the stepped form the
    /// recovery driver runs under its budget).
    ///
    /// [`close_checkpoint`]: Self::close_checkpoint
    /// [`release_from`]: Self::release_from
    /// [`end_releases`]: Self::end_releases
    pub fn end_of_checkpoint(&mut self, table: &mut TieredTable) {
        let listed = self.close_checkpoint();
        self.release_from(table, 0, listed);
        self.end_releases();
    }

    /// The checkpoint has ended (R9): the list closes — no settle appends
    /// to it again, so an index into it names one address for as long as
    /// it lives — and its length is what [`release_from`](Self::release_from)
    /// walks.
    pub fn close_checkpoint(&mut self) -> usize {
        let list = match core::mem::replace(&mut self.releases, ReleaseList::Released) {
            ReleaseList::Loading(list) | ReleaseList::Releasing(list) => list,
            ReleaseList::Released => IdChunks::new(),
        };
        let listed = list.len();
        self.releases = ReleaseList::Releasing(list);
        listed
    }

    /// The closed list's length: what [`release_from`](Self::release_from)
    /// walks (nothing while the checkpoint loads or once released).
    #[must_use]
    pub fn releases_listed(&self) -> usize {
        match &self.releases {
            ReleaseList::Releasing(list) => list.len(),
            ReleaseList::Loading(_) | ReleaseList::Released => 0,
        }
    }

    /// Releases the listed addresses `[at, at + max)` of the closed list,
    /// clamped to it, in settle order (R9, A1.3): each address whose
    /// blob-reference entry the 0x05 section registered dies once, now
    /// that the entry exists; nothing for an address with none. Answers how
    /// many listed addresses it walked. The list's order is the settles' —
    /// a function of the recovery unit, the same at every step budget — so
    /// the reclaim queue takes the deaths in one order at every budget.
    pub fn release_from(&mut self, table: &mut TieredTable, at: usize, max: usize) -> usize {
        let ReleaseList::Releasing(list) = &self.releases else {
            return 0;
        };
        let walked = list.len().saturating_sub(at).min(max);
        if cfg!(inf_canary_replay_blob_release_skip) {
            // The planted canary breaks R9: a settled ref's blob
            // reference stays with no slot.
            return walked;
        }
        if cfg!(inf_canary_replay_release_reordered) && at > 0 {
            // The planted canary breaks the settle order: a call that
            // resumes inside the list releases its batch in reverse.
            let batch: Vec<u64> = list.iter_from(at).take(walked).collect();
            for &addr in batch.iter().rev() {
                if table.extents.reference_at(addr).is_some() {
                    table.extents.note_death(addr);
                    self.counters.blob_releases += 1;
                }
            }
            return walked;
        }
        // Bound: `max` listed addresses, each a map probe and a death.
        let mut released = 0u64;
        for addr in list.iter_from(at).take(walked) {
            if table.extents.reference_at(addr).is_some() {
                table.extents.note_death(addr);
                released += 1;
            }
        }
        self.counters.blob_releases += released;
        walked
    }

    /// The closed list is walked: it is freed, and the settles of the tail
    /// replay that follows list nothing (their blob references' entries
    /// exist, so each settle released its own).
    pub fn end_releases(&mut self) {
        self.releases = ReleaseList::Released;
    }

    /// R10's first half: replay has ended. A machine that demoted
    /// enters `Settling` with its cursor at `ro`; one that did not is
    /// ready to hand over.
    pub fn end_of_replay(&mut self, table: &TieredTable) {
        debug_assert!(self.pending.is_none(), "replay ends with no record waiting for room");
        if self.state == ReplayState::Spilling {
            let cursor = if cfg!(inf_canary_replay_no_end_settle) {
                // The planted canary breaks R10: no end settle, so the
                // rebuild finds same-key pairs in a namespace that demoted.
                table.space.tail()
            } else {
                table.space.ro_boundary()
            };
            self.state = ReplayState::Settling { cursor };
        }
    }

    /// One end-of-replay settle step (R10): R7 on every live record of
    /// `[cursor, tail)`, by address, until the step's charge — its walked
    /// bytes and settle reads at [`ReplayWork::settle_charge_bytes`] —
    /// reaches `budget_bytes`, or the tail. `ro` does not move.
    ///
    /// # Errors
    /// A settle read or identity refusal; `ReplayNotEnded` in `Spilling`.
    pub fn settle_step(
        &mut self,
        table: &mut TieredTable,
        budget_bytes: u64,
    ) -> Result<SettleProgress, ReplayRefusal> {
        let cursor = match self.state {
            ReplayState::Seeded | ReplayState::Fitting => return Ok(SettleProgress::Done),
            ReplayState::Spilling => return Err(ReplayRefusal::ReplayNotEnded),
            ReplayState::Settling { cursor } => cursor,
        };
        let budget_bytes = budget_bytes.max(1);
        let to = WalkTo::Settle { budget_bytes };
        let (span, end) = self.settle_walk(table, WalkStart::At(cursor), to)?;
        self.state = ReplayState::Settling { cursor: span.end };
        Ok(match end {
            WalkEnd::Tail => SettleProgress::Done,
            WalkEnd::AtStop | WalkEnd::Spent => SettleProgress::More,
        })
    }

    /// R10's last half: a table that demoted drains its flush and
    /// seals its active file; the machine hands the plane a pipeline
    /// under the live claim rule with a handle for every sealed
    /// catalogue file, and returns its final counters and the I/O since
    /// the last [`take_work`](Self::take_work) — the drain's seal and
    /// barrier included, which counters read before the hand-over
    /// would miss. Only a machine with every RAM record settled gets
    /// here (R10): `Spilling` has no arm, `Settling` only at the tail.
    ///
    /// # Errors
    /// `Unsettled`, or the drain's flush error.
    pub fn hand_over(
        mut self,
        table: &mut TieredTable,
    ) -> Result<BootHandedOver<F>, ReplayRefusal> {
        let tail = table.space.tail();
        match self.state {
            ReplayState::Seeded | ReplayState::Fitting => {}
            ReplayState::Settling { cursor } if cursor == tail => {}
            ReplayState::Spilling => {
                return Err(ReplayRefusal::Unsettled { cursor: None, tail });
            }
            ReplayState::Settling { cursor } => {
                return Err(ReplayRefusal::Unsettled { cursor: Some(cursor), tail });
            }
        }
        let sealed_before = self.flush.sealed().len();
        let drained = match table.flush_drain(&mut self.flush) {
            Ok(drained) => drained,
            Err(cause) => return Err(self.flush_refusal(table, cause)),
        };
        // The drain is the active file's seal alone: every demote step
        // appended up to its cut (D2 rule 4) and the end settle moves no
        // boundary (R10), so nothing below `ro` is left to append or cross.
        debug_assert_eq!(drained.appended_bytes, 0, "the hand-over's drain appends nothing");
        debug_assert_eq!(drained.gaps_crossed, 0, "the hand-over's drain crosses no gap");
        self.note_flush(drained);
        let handles_held = self.flush.held_handles();
        let handed = self.flush.hand_over().map_err(|cause| ReplayRefusal::Flush {
            cause,
            unplaced_bytes: 0,
            handles_held,
        })?;
        let sealed_now = handed.flush.sealed().len() - sealed_before;
        debug_assert!(sealed_now <= 1, "the drain seals at most the active file");
        Ok(BootHandedOver { handed, counters: self.counters, work: self.work })
    }
}

/// What the boot's exit returns (ADR-0174 R10, D6): the plane's pipeline
/// and handles, and the machine's final counters and unread I/O — the
/// hand-over's own drain seal and barrier included.
pub struct BootHandedOver<F: SegmentFs> {
    /// The pipeline under the live claim rule, with every handle.
    pub handed: HandedOver<F>,
    /// Replay's counters, final.
    pub counters: ReplayCounters,
    /// The I/O since the last [`TierReplay::take_work`], the drain's
    /// included.
    pub work: ReplayWork,
}

impl TieredTable {
    // ---- the replay entries (ADR-0174 D1, D3) ----

    /// [`replay_upsert_lent`](Self::replay_upsert_lent) with no machine:
    /// a single replay rule on a table no boot pipeline backs — a record
    /// that needs room refuses `NoPipeline`, typed. A machine reaches a
    /// replay entry only through `Keyspace::apply_record`'s seam, the
    /// shipped dispatcher, so no harness keeps a copy of it.
    ///
    /// # Errors
    /// As [`replay_upsert_lent`](Self::replay_upsert_lent).
    pub fn replay_upsert(
        &mut self,
        markers: &[LogicalAddr],
        key: &[u8],
        value: &[u8],
        hash: u64,
    ) -> Result<LogicalAddr, ReplayRefusal> {
        let placed = self
            .replay_upsert_lent::<<NoSpill as ReplaySpill>::Fs>(None, markers, key, value, hash);
        match placed? {
            Placement::Placed(addr) => Ok(addr),
            // A record waits for room only with a machine to make it.
            Placement::NeedsRoom => Err(ReplayRefusal::RoomMoved { why: "no machine to wait on" }),
        }
    }

    /// [`replay_delete_lent`](Self::replay_delete_lent) with no machine,
    /// as [`replay_upsert`](Self::replay_upsert): reads no cold slot.
    ///
    /// # Errors
    /// As [`replay_delete_lent`](Self::replay_delete_lent).
    pub fn replay_delete(
        &mut self,
        markers: &[LogicalAddr],
        key: &[u8],
        hash: u64,
    ) -> Result<bool, ReplayRefusal> {
        let deleted =
            self.replay_delete_lent::<<NoSpill as ReplaySpill>::Fs>(None, markers, key, hash);
        deleted.map(|(removed, _reads)| removed)
    }

    /// Replays one checkpoint image or tail `SET` (R5): the typed length
    /// refusals first, then the room question until it answers `Fits` — a
    /// pad per answer, at most four asks — or `Demote`, where the record
    /// answers `NeedsRoom` having changed nothing and the driver runs the
    /// demote step under its budget and presents it again (ADR-0174 A1.5),
    /// then the parked markers (R3, R4), then the blind key-verified upsert:
    /// a RAM record of the key is overwritten in place of its slot with its
    /// origins moved to the new record; none inserts at the tail, cold
    /// exact-hash slots untouched and unread (R5).
    ///
    /// # Errors
    /// [`ReplayRefusal`] — the boot's typed refusal; nothing of this
    /// record was applied.
    pub(crate) fn replay_upsert_lent<F: SegmentFs>(
        &mut self,
        replay: Option<&mut TierReplay<F>>,
        markers: &[LogicalAddr],
        key: &[u8],
        value: &[u8],
        hash: u64,
    ) -> Result<Placement, ReplayRefusal> {
        let admitted = self.admit_inline(key, value).map_err(ReplayRefusal::TooLarge)?;
        self.replay_place(replay, markers, hash, admitted.len(), |t| t.apply_image(admitted, hash))
    }

    /// [`replay_upsert_lent`](Self::replay_upsert_lent) for a tag-9 image or a tail
    /// `StringExtentRef` (R5 over the extent kind).
    ///
    /// # Errors
    /// As [`replay_upsert_lent`](Self::replay_upsert_lent).
    pub(crate) fn replay_upsert_extent_lent<F: SegmentFs>(
        &mut self,
        replay: Option<&mut TierReplay<F>>,
        markers: &[LogicalAddr],
        key: &[u8],
        hash: u64,
        ext: ExtentRef,
    ) -> Result<Placement, ReplayRefusal> {
        let len = self.admit_extent(key, ext).map_err(ReplayRefusal::TooLarge)?;
        TieredTable::extent_admission_cost(ext, len).map_err(ReplayRefusal::TooLarge)?;
        self.replay_place(replay, markers, hash, len, |t| t.apply_extent_image(key, hash, ext))
    }

    /// The placement sequence both upserts share, after their length
    /// refusals: the state (no record after the end of replay), the room
    /// question, the parked markers, the arm, and `Seeded` → `Fitting` —
    /// one read of the state, one ask of `room` for a record that fits. A
    /// record waiting for room is the one presented again, of its length;
    /// it keeps its asks until it is placed. Only `Spilling` can hold one,
    /// so a fitting boot reads no pending record and clears none.
    fn replay_place<F: SegmentFs>(
        &mut self,
        mut replay: Option<&mut TierReplay<F>>,
        markers: &[LogicalAddr],
        hash: u64,
        len: usize,
        arm: impl FnOnce(&mut TieredTable) -> Result<LogicalAddr, OpError>,
    ) -> Result<Placement, ReplayRefusal> {
        let entry = match replay.as_deref() {
            None => PlaceEntry::Plain,
            Some(r) => {
                debug_assert!(
                    r.pending.is_none() || r.state == ReplayState::Spilling,
                    "a record waits for room only while the machine spills"
                );
                match r.state {
                    ReplayState::Settling { .. } => return Err(ReplayRefusal::ReplayEnded),
                    ReplayState::Seeded => PlaceEntry::Seeded,
                    ReplayState::Fitting => PlaceEntry::Plain,
                    ReplayState::Spilling => match &r.pending {
                        None => PlaceEntry::Plain,
                        Some(pending) if pending.len == len => PlaceEntry::Waiting,
                        Some(_) => {
                            let why = "another record presented";
                            return Err(ReplayRefusal::RoomMoved { why });
                        }
                    },
                }
            }
        };
        match self.make_room(replay.as_deref_mut(), len)? {
            RoomVerdict::Fits => {}
            RoomVerdict::NeedsRoom => return Ok(Placement::NeedsRoom),
        }
        self.drain_markers(replay.as_deref_mut(), markers, hash);
        let placed = arm(self).map_err(ReplayRefusal::Store)?;
        if let Some(r) = replay {
            match entry {
                PlaceEntry::Plain => {}
                PlaceEntry::Seeded => {
                    debug_assert_eq!(
                        r.state,
                        ReplayState::Seeded,
                        "a seeded table's first record fits"
                    );
                    r.state = ReplayState::Fitting;
                }
                PlaceEntry::Waiting => r.pending = None,
            }
        }
        Ok(Placement::Placed(placed))
    }

    /// Replays one tail `DEL` (R6): the settle read of each exact-hash
    /// cold slot this boot demoted first, then the parked markers,
    /// then the RAM record of the key — with its origins — and the read
    /// slots whose key matched, each with its exact death. Returns
    /// whether anything of the key was removed and whether it read, the
    /// boot work its answer prices. A cold slot at or above the origin
    /// exists only once a demote step began, so `Seeded` and `Fitting`
    /// read and probe nothing.
    ///
    /// # Errors
    /// A settle read or identity refusal; nothing changed.
    pub(crate) fn replay_delete_lent<F: SegmentFs>(
        &mut self,
        mut replay: Option<&mut TierReplay<F>>,
        markers: &[LogicalAddr],
        key: &[u8],
        hash: u64,
    ) -> Result<(bool, DeleteReads), ReplayRefusal> {
        if let Some(r) = replay.as_deref_mut() {
            match r.state {
                ReplayState::Settling { .. } => return Err(ReplayRefusal::ReplayEnded),
                // Only `Spilling` can hold a record waiting for room.
                ReplayState::Spilling if r.pending.is_some() => {
                    return Err(ReplayRefusal::RoomMoved { why: "a delete presented" });
                }
                ReplayState::Spilling => return self.replay_delete_spilling(r, markers, key, hash),
                ReplayState::Seeded | ReplayState::Fitting => {}
            }
        }
        self.drain_markers(replay, markers, hash);
        Ok((self.apply_delete(key, hash), DeleteReads::None))
    }

    /// [`replay_delete_lent`](Self::replay_delete_lent) once a demote step began:
    /// R6's reads before anything changes, then the drain, the RAM
    /// delete and the verified cold slots' removal.
    fn replay_delete_spilling<F: SegmentFs>(
        &mut self,
        r: &mut TierReplay<F>,
        markers: &[LogicalAddr],
        key: &[u8],
        hash: u64,
    ) -> Result<(bool, DeleteReads), ReplayRefusal> {
        let reads_before = r.counters.settle_reads;
        r.verify_cold_for_delete(self, key, hash)?;
        let reads = if r.counters.settle_reads > reads_before {
            DeleteReads::Read
        } else {
            DeleteReads::None
        };
        self.drain_markers(Some(&mut *r), markers, hash);
        let mut removed = self.apply_delete(key, hash);
        for i in 0..r.doomed.len() {
            let (cold, len) = r.doomed[i];
            self.shadow_note_removed(cold);
            // The key is deleted: the slot's origins go with it (R6).
            let _dropped = self.remove_cold_slot(hash, cold, SettleCase::ThisLife { len });
            r.counters.deletes_verified += 1;
            r.counters.settled_same_key += 1;
            removed = true;
        }
        r.doomed.clear();
        Ok((removed, reads))
    }

    /// Replays one checkpoint address reference (R1): insert unless
    /// the exact `(hash, addr)` pair is slotted (the walker's at-least-once
    /// re-emission may duplicate a ref). Counts the slot into its file
    /// exactly once per surviving slot (ADR-0058 D4). Live-byte
    /// accounting is untouched: a ref's length is unknown without a read.
    /// R2 — a ref section after a record of this life — is
    /// `apply_ref_section`'s, over the section's facts.
    ///
    /// # Panics
    /// Debug-panics when `addr` is not below this life's origin — the
    /// `.ick` reader already refused anything at or above its watermark.
    pub fn replay_ref(&mut self, hash: u64, addr: LogicalAddr) {
        debug_assert!(addr < self.space.life_origin(), "refs name pre-life addresses");
        if self.index.contains_pair(hash, addr) {
            return;
        }
        if self.index.needs_grow() {
            self.index.grow(|_, ext| ext);
        }
        self.index.insert(hash, addr);
        self.live.note_ref(addr.to_raw());
    }

    /// Replays one `ColdDisplace` marker (R3, R4): below the life origin
    /// the exact pair is removed if present — its file uncounted and
    /// stamped, its blob reference released (R3); absence is a legal
    /// interleaving. At or above the origin the marker names a
    /// crashed-life address, nothing in this life: no-op (R4) — the
    /// paired mutation resolves by key.
    pub fn replay_displace(&mut self, hash: u64, old: LogicalAddr) -> Displaced {
        if old >= self.space.life_origin() {
            return Displaced::AboveOrigin;
        }
        if !self.index.remove_if_present(hash, old) {
            return Displaced::Absent;
        }
        self.shadow_note_removed(old);
        self.live.note_displaced(old.to_raw());
        self.extents.note_death(old.to_raw());
        Displaced::Removed
    }

    /// D2 rule 1's room question, asked until it answers `Fits`: `Demote`
    /// begins a demote step and answers `NeedsRoom`, `Pad` moves the tail,
    /// `End` and a fifth ask refuse typed. The record that fits — every
    /// record of a boot that fits — asks once and leaves; the loop is the
    /// out-of-line rest. The two hints are measured: without them a
    /// replayed `SET` through a lent machine costs 26 more instructions
    /// (the fitting-boot replay A/B).
    #[inline]
    fn make_room<F: SegmentFs>(
        &mut self,
        replay: Option<&mut TierReplay<F>>,
        len: usize,
    ) -> Result<RoomVerdict, ReplayRefusal> {
        let first = self.space.room(len);
        if matches!(first, Room::Fits) {
            return Ok(RoomVerdict::Fits);
        }
        self.make_room_after(replay, len, first)
    }

    /// [`make_room`](Self::make_room) from its first answer that did not
    /// fit.
    #[cold]
    #[inline(never)]
    fn make_room_after<F: SegmentFs>(
        &mut self,
        mut replay: Option<&mut TierReplay<F>>,
        len: usize,
        first: Room,
    ) -> Result<RoomVerdict, ReplayRefusal> {
        let mut asks = 0u32;
        if let Some(pending) = replay.as_deref_mut().and_then(|r| r.pending.as_mut()) {
            // The record presented again while its demote step is under
            // way: the step resumes, and its room question is the same ask
            // (D2 rule 1, ADR-0174 A1.3), so `room` must still answer the
            // target the step makes room for.
            if self.space.head() < pending.target {
                if first != Room::Demote(pending.target) {
                    return Err(ReplayRefusal::RoomMoved { why: "room answers another target" });
                }
                if cfg!(inf_canary_recover_room_reask_counted) {
                    // The planted canary counts the resumed ask as a new one.
                    pending.asks += 1;
                    if pending.asks > REPLAY_ROOM_ASKS_MAX {
                        return Err(ReplayRefusal::RoomAsks { len, asks: pending.asks });
                    }
                }
                return Ok(RoomVerdict::NeedsRoom);
            }
            asks = pending.asks;
        }
        let mut answer = Some(first);
        loop {
            asks += 1;
            if asks > REPLAY_ROOM_ASKS_MAX {
                return Err(ReplayRefusal::RoomAsks { len, asks });
            }
            let room = answer.take().unwrap_or_else(|| self.space.room(len));
            match room {
                Room::Fits => return Ok(RoomVerdict::Fits),
                Room::End => return Err(ReplayRefusal::End { len }),
                Room::Demote(target) => {
                    if cfg!(inf_canary_replay_no_demote) {
                        // The planted canary breaks D1: the window's
                        // refusal fails the boot.
                        return Err(ReplayRefusal::Store(OpError::OutOfMemory));
                    }
                    let Some(r) = replay.as_deref_mut() else {
                        return Err(ReplayRefusal::NoPipeline { need: Room::Demote(target) });
                    };
                    r.begin_demote(target, asks, len);
                    return Ok(RoomVerdict::NeedsRoom);
                }
                Room::Pad(target) => {
                    let Some(r) = replay.as_deref_mut() else {
                        return Err(ReplayRefusal::NoPipeline { need: Room::Pad(target) });
                    };
                    let to = target.to();
                    self.space
                        .pad_tail(target)
                        .map_err(|WindowFull| ReplayRefusal::PadRefused { to })?;
                    r.counters.pads_placed += 1;
                }
            }
        }
    }

    /// The marker drain, after the room question and a `DEL`'s reads and
    /// before the mutation: each
    /// parked marker names a slot of the mutation's own key (ADR-0057
    /// D4: markers precede their mutation in its frame), so the pair's
    /// hash is the mutation's.
    fn drain_markers<F: SegmentFs>(
        &mut self,
        replay: Option<&mut TierReplay<F>>,
        markers: &[LogicalAddr],
        hash: u64,
    ) {
        let mut skipped = 0u64;
        for &old in markers {
            if self.replay_displace(hash, old) == Displaced::AboveOrigin {
                skipped += 1;
            }
        }
        if skipped > 0
            && let Some(r) = replay
        {
            r.counters.markers_skipped += skipped;
        }
    }

    /// The boundary's one boot advance (R7): to the end of a settled span
    /// that starts at the boundary, so a walk the step budget stopped and
    /// resumed still seals only records it settled (ADR-0174 A1.3).
    fn seal_settled(&mut self, span: SettledSpan) {
        assert_eq!(span.from, self.space.ro_boundary(), "a sealed span starts at the boundary");
        let from = self.space.ro_boundary().to_raw();
        if span.end.to_raw() > from {
            self.space.advance_ro_boundary(span.end);
            self.space.note_demote_slice(span.end.to_raw() - from);
        }
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test-only: a whole listing of a fixture directory")]
mod tests {
    use std::path::Path;

    use inf_foundation::KeyHasher;
    use inf_log::fs::mem::MemFs;
    use inf_log::{TierFlush, TierFlushConfig, TierIoMode};

    use super::*;
    use crate::address_space::AddressSpaceConfig;
    use crate::demote::DemotionConfig;

    const PAGE: u64 = 4 << 10;

    fn table() -> TieredTable {
        let demote = DemotionConfig::for_budget(64 << 10, PAGE);
        TieredTable::new(
            AddressSpaceConfig {
                reserve_bytes: demote.ring_reserve_bytes().expect("valid"),
                page_bytes: PAGE as usize,
                life_origin: LogicalAddr::ZERO,
            },
            demote,
            64,
            KeyHasher::default(),
        )
        .expect("ring")
    }

    fn machine(fs: &MemFs, table: &TieredTable) -> TierReplay<MemFs> {
        let flush = TierFlush::new(
            fs.clone(),
            TierFlushConfig {
                shard_dir: Path::new("shard-0").to_path_buf(),
                cell: 0,
                ns: inf_log::NsId(9),
                mode: TierIoMode::Buffered,
                file_capacity: 1 << 20,
                slice_bytes: PAGE,
                record_len_max: 16 << 20,
            },
            0,
            &inf_foundation::bounded::CapCensus::new(inf_foundation::CellId(0)).0,
        );
        TierReplay::new(BootFlush::new(flush, Vec::new()), table)
    }

    /// One record through the entry, driven as `drive_record` drives it:
    /// presented, and on `NeedsRoom` its demote step run whole, then
    /// presented again.
    fn upsert(table: &mut TieredTable, replay: &mut TierReplay<MemFs>, key: &[u8], value: &[u8]) {
        let hash = table.hash_key(key);
        // Bound: four room asks a record (D2 rule 1).
        loop {
            let placed = table.replay_upsert_lent(Some(&mut *replay), &[], key, value, hash);
            match placed.expect("replays") {
                Placement::Placed(_) => return,
                Placement::NeedsRoom => {
                    while replay.demote_step(table, u64::MAX).expect("demotes")
                        == DemoteProgress::More
                    {}
                }
            }
        }
    }

    /// Two windows of records through the entry: the machine demotes.
    fn spill(table: &mut TieredTable, replay: &mut TierReplay<MemFs>) {
        let value = vec![0x5A; 900];
        for i in 0..160u32 {
            let key = format!("k:{i:04}").into_bytes();
            upsert(table, replay, &key, &value);
        }
        assert!(replay.counters().demote_steps > 0);
        assert_eq!(replay.phase(), ReplayPhase::Spilling);
    }

    /// R10: a machine that demoted is handed over only with every RAM
    /// record settled — `Spilling` has no arm, `Settling` only at the
    /// tail; a settle step before the end of replay is declared is a
    /// typed refusal too.
    #[test]
    fn hand_over_refuses_a_table_that_demoted_until_its_end_settle_reached_the_tail() {
        let fs = MemFs::new();
        let mut table = table();
        let mut replay = machine(&fs, &table);
        spill(&mut table, &mut replay);
        assert!(matches!(replay.settle_step(&mut table, PAGE), Err(ReplayRefusal::ReplayNotEnded)));
        let Err(err) = replay.hand_over(&mut table) else { panic!("Spilling has no arm") };
        assert!(matches!(err, ReplayRefusal::Unsettled { cursor: None, .. }), "{err}");
        let mut table = table_after(&fs);
        let mut replay = machine(&fs, &table);
        spill(&mut table, &mut replay);
        replay.end_of_replay(&table);
        assert_eq!(replay.phase(), ReplayPhase::Settling);
        let ro = table.space().ro_boundary();
        let Err(err) = replay.hand_over(&mut table) else { panic!("Settling below the tail") };
        assert!(
            matches!(err, ReplayRefusal::Unsettled { cursor: Some(c), .. } if c == ro),
            "{err}"
        );
        let mut table = table_after(&fs);
        let mut replay = machine(&fs, &table);
        spill(&mut table, &mut replay);
        replay.end_of_replay(&table);
        while replay.settle_step(&mut table, PAGE).expect("settle") == SettleProgress::More {}
        let done = replay.hand_over(&mut table).expect("settled to the tail");
        let handed = done.handed;
        assert_eq!(handed.handles.len(), handed.flush.sealed().len());
        assert!(handed.flush.active().is_none());
        assert_eq!(done.counters.files_sealed, handed.flush.sealed().len() as u64);
        assert!(done.work.barriers >= 1, "the drain's barrier is the hand-over's work");
    }

    /// A record or a `DEL` after the end of replay was declared: the
    /// machine refuses typed in `Settling` before it asks for room or
    /// reads, so nothing changes and the cursor stays where it was.
    #[test]
    fn a_record_after_the_end_of_replay_refuses_typed_and_changes_nothing() {
        let fs = MemFs::new();
        let mut table = table();
        let mut replay = machine(&fs, &table);
        spill(&mut table, &mut replay);
        replay.end_of_replay(&table);
        let (tail, len, counters) = (table.space().tail(), table.len(), replay.counters());
        let value = vec![0x5A; 900];
        let hash = table.hash_key(b"late");
        let err = table
            .replay_upsert_lent(Some(&mut replay), &[], b"late", &value, hash)
            .expect_err("a SET after the end of replay");
        assert!(matches!(err, ReplayRefusal::ReplayEnded), "{err}");
        let ext = ExtentRef { extent_id: 1, offset: 0, len: 4096 };
        let err = table
            .replay_upsert_extent_lent(Some(&mut replay), &[], b"late", hash, ext)
            .expect_err("an extent reference after the end of replay");
        assert!(matches!(err, ReplayRefusal::ReplayEnded), "{err}");
        let old = table.hash_key(b"k:0000");
        let err = table
            .replay_delete_lent(Some(&mut replay), &[], b"k:0000", old)
            .expect_err("a DEL after the end of replay");
        assert!(matches!(err, ReplayRefusal::ReplayEnded), "{err}");
        assert_eq!(table.space().tail(), tail, "nothing placed");
        assert_eq!(table.len(), len, "no slot moved");
        assert_eq!(replay.counters(), counters, "no step, read or delete counted");
        assert_eq!(replay.phase(), ReplayPhase::Settling, "the cursor is kept");
    }

    /// One end-of-replay settle step charges each settle read its price
    /// beside the bytes it walks, and yields after the record whose charge
    /// reaches the budget: over a span where every record has a cold twin,
    /// a step reads at most ⌈budget ÷ price⌉ plus one record's twins.
    #[test]
    fn an_end_settle_step_yields_at_its_charge_of_reads_and_bytes() {
        let fs = MemFs::new();
        let mut table = table();
        let mut replay = machine(&fs, &table);
        spill(&mut table, &mut replay);
        // Rewrite the demoted keys inside the last window: each rewrite is
        // an `Open` record with a cold twin when replay ends.
        let value = vec![0x6B; 900];
        for i in 0..56u32 {
            let key = format!("k:{i:04}").into_bytes();
            upsert(&mut table, &mut replay, &key, &value);
        }
        replay.end_of_replay(&table);
        let budget: u64 = 64 << 10;
        let bound = budget.div_ceil(crate::limits::SETTLE_READ_CHARGE_BYTES) + 1;
        let mut steps = 0u64;
        loop {
            let reads = replay.counters().settle_reads;
            let progress = replay.settle_step(&mut table, budget).expect("settles");
            let read = replay.counters().settle_reads - reads;
            assert!(read <= bound, "step {steps}: {read} settle reads against a bound of {bound}");
            steps += 1;
            if progress == SettleProgress::Done {
                break;
            }
        }
        assert!(replay.counters().settled_same_key >= 56, "every rewrite's twin settled");
        assert!(steps >= 28, "VACUOUS: the walk did not yield on its reads ({steps} steps)");
    }

    /// A length refusal names the bound it crossed, the length and the
    /// bound's value — before the record asks for room.
    #[test]
    fn a_length_refusal_names_its_bound() {
        let fs = MemFs::new();
        let mut table = table();
        table
            .set_blob_config(crate::extents::BlobConfig { threshold_bytes: 1024, max_bytes: 4096 });
        let mut replay = machine(&fs, &table);
        let hash = table.hash_key(b"big");
        let err = table
            .replay_upsert_lent(Some(&mut replay), &[], b"big", &[0x5A; 1024], hash)
            .expect_err("a value at the blob threshold is not inline");
        let text = err.to_string();
        assert!(text.contains("BLOB-THRESHOLD") && text.contains("1024"), "{text}");
        let ext = ExtentRef { extent_id: 1, offset: 0, len: 8192 };
        let err = table
            .replay_upsert_extent_lent(Some(&mut replay), &[], b"big", hash, ext)
            .expect_err("an extent above the blob maximum");
        let text = err.to_string();
        assert!(text.contains("8192") && text.contains("4096"), "{text}");
        assert_eq!(table.space().tail(), LogicalAddr::ZERO, "nothing placed");
    }

    /// The end of the checkpoint releases the settled refs' blob references
    /// in the order the settles listed them, whatever order the origin map
    /// holds them in, and a release stepped in batches — of one address, and
    /// of two, so that calls resume inside the list (`at` 2 and 4) and walk
    /// more than one address there — releases in the same order: the
    /// reclaim queue takes the deaths in one order at every step budget.
    /// Red where a call that resumes inside the list releases its batch in
    /// reverse (`inf_canary_replay_release_reordered`).
    #[test]
    fn the_end_of_the_checkpoint_releases_blob_references_in_settle_order() {
        let fs = MemFs::new();
        // Five refs settled during image load, in descending address order;
        // each names its own extent.
        let refs = [(5 << 12, 50), (4 << 12, 40), (3 << 12, 30), (2 << 12, 20), (1 << 12, 10)];
        let settled: Vec<u64> = refs.iter().map(|&(_, extent_id)| extent_id).collect();
        // `None` releases the list whole; `Some(max)` at most `max` a call.
        let order_of = |batch: Option<usize>| {
            let mut table = table();
            let mut replay = machine(&fs, &table);
            for &(addr, extent_id) in &refs {
                table.extents.register(addr, extent_id, 4096);
            }
            replay.releases = ReleaseList::Loading(refs.iter().map(|&(addr, _)| addr).collect());
            let mut calls = 0;
            if let Some(max) = batch {
                let listed = replay.close_checkpoint();
                let mut at = 0;
                // Bound: ⌈listed ÷ max⌉ calls, each walking at least one.
                while at < listed {
                    at += replay.release_from(&mut table, at, max);
                    calls += 1;
                }
                replay.end_releases();
            } else {
                replay.end_of_checkpoint(&mut table);
            }
            assert_eq!(calls, batch.map_or(0, |max| refs.len().div_ceil(max)));
            assert_eq!(replay.counters().blob_releases, 5);
            let seeded = table.extents.sweep_seed(&[], &[]);
            assert!(seeded.is_empty());
            table.extents.reclaim_work(0, 8).iter().map(|c| c.extent_id).collect::<Vec<u64>>()
        };
        assert_eq!(order_of(None), settled, "released in settle order");
        assert_eq!(order_of(Some(1)), settled, "and so an address a call");
        assert_eq!(order_of(Some(2)), settled, "and so two a call, resumed at 2 and 4");
    }

    /// A fresh table in a fresh directory of the same `MemFs` (the file
    /// ids restart at 0, so the earlier machine's files must not collide).
    fn table_after(fs: &MemFs) -> TieredTable {
        for name in fs.list_dir(Path::new("shard-0/cold")).unwrap_or_default() {
            fs.remove_file(&Path::new("shard-0/cold").join(name)).expect("remove");
        }
        table()
    }

    /// A namespace with no boot machine (the pre-D4 shape): the entry
    /// places what fits and refuses typed, nothing changed, where a
    /// demote would be needed.
    #[test]
    fn a_namespace_without_a_machine_places_what_fits_and_refuses_the_rest_typed() {
        let mut table = table();
        let value = vec![0x5A; 900];
        let mut placed = 0u32;
        let refused = loop {
            let key = format!("k:{placed:04}").into_bytes();
            let hash = table.hash_key(&key);
            match table.replay_upsert(&[], &key, &value, hash) {
                Ok(_) => placed += 1,
                Err(err) => break err,
            }
            assert!(placed < 10_000, "a 64 KiB window admits far fewer");
        };
        assert!(
            matches!(refused, ReplayRefusal::NoPipeline { need: Room::Demote(_) }),
            "{refused}"
        );
        assert_eq!(table.len() as u32, placed, "the refused record placed nothing");
        assert!(table.space().report().committed_bytes <= (64 << 10) + PAGE);
    }
}
