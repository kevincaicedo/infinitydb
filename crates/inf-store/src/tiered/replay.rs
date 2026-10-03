//! Boot replay of one tiered namespace on one cell (ADR-0174; DRR
//! FCR-STTIER-01 §1): the replay state machine, the demote step that
//! makes room when the RAM window fills (D2), the settle walk that keeps
//! a sealed record from leaving a same-key cold slot behind (D3 R7), the
//! end-of-replay cursor (R10), the end-of-checkpoint blob release (R9)
//! and the hand-over to the plane. Every replay append and delete enters
//! through [`TieredTable::replay_upsert`], [`replay_upsert_extent`]
//! (TieredTable::replay_upsert_extent) and [`TieredTable::replay_delete`],
//! which hold the demote machinery: no tiered replay append exists outside
//! them (I2), the window's refusal is a [`Room`], never an error variant,
//! and every boot settle answers from a [`ColdKey`] read through a held
//! handle (I17, I18).
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
use crate::record::{ColdKey, ColdKeyError};
use crate::tiered::shadow::SettleCase;

/// Asks of [`AddressSpace::room`] one record may make (ADR-0174 D2 rule
/// 1: `Demote`, `Pad`, `Demote`, `Fits`). A fifth is a typed boot refusal
/// — by I15 a defect, never input.
pub const REPLAY_ROOM_ASKS_MAX: u32 = 4;

/// Where the machine stands (DRR FCR-STTIER-01 §1). `Serving` is the
/// machine consumed by [`TierReplay::hand_over`]; `Refused` the error that
/// dropped it.
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
    /// Demote steps (E1d).
    pub demote_steps: u64,
    /// Pads placed (E1p).
    pub pads_placed: u64,
    /// Record bytes replay appended to tier files.
    pub tier_bytes: u64,
    /// Barriers of the demote step and the hand-over drain.
    pub barriers: u64,
    /// Files the boot pipeline sealed (not the `Recovered` reseal).
    pub files_sealed: u64,
    /// Settle reads of E5, E10 and E12.
    pub settle_reads: u64,
    /// Slots those reads settled as the same key.
    pub settled_same_key: u64,
    /// Slots those reads kept as a distinct key.
    pub settled_distinct: u64,
    /// Cold slots a replayed `DEL` verified and removed (E5).
    pub deletes_verified: u64,
    /// Blob references released by a replay settle (E10, E14).
    pub blob_releases: u64,
    /// Markers naming an address at or above the life origin (E7) —
    /// outside the zero set: a fitting boot counts them too.
    pub markers_skipped: u64,
}

/// Boot I/O one replay call did (DRR FCR-STTIER-01 §3, §5): what the
/// recovery driver charges to its step budget at its own prices.
#[derive(Copy, Clone, Default, Debug, PartialEq, Eq)]
pub struct ReplayWork {
    /// Record bytes appended to tier files.
    pub tier_bytes: u64,
    /// Barriers made.
    pub barriers: u64,
    /// Settle reads made.
    pub settle_reads: u64,
    /// Bytes walked by the end-of-replay settle.
    pub walked_bytes: u64,
}

impl ReplayWork {
    /// Nothing to charge.
    #[must_use]
    pub fn is_zero(self) -> bool {
        self == ReplayWork::default()
    }
}

/// The replay seam (DRR FCR-STTIER-01 §7): lends a namespace's boot
/// machine to the keyspace's tiered replay arms and takes the I/O they
/// did. `None` is a namespace with no machine: until every tiered
/// namespace recovers through a manifest section (ADR-0174 D4), the
/// replay of such a namespace places what fits and refuses typed where
/// a demote would be needed — a dated deviation in the story's ticket.
pub trait ReplaySpill {
    /// The pipelines' filesystem.
    type Fs: SegmentFs;
    /// The namespace's machine, if it has one.
    fn replay_mut(&mut self, ns: inf_log::NsId) -> Option<&mut TierReplay<Self::Fs>>;
    /// Boot I/O the arm just did.
    fn charge(&mut self, work: ReplayWork);
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

    fn charge(&mut self, _work: ReplayWork) {}
}

/// A typed boot refusal from replay (DRR FCR-STTIER-01 §2): the recovery
/// fail-stop class, naming the check. Nothing of the refusing record was
/// applied; a refusal from inside a demote step leaves the table exact
/// (the boundary and the cursor never passed an unread record).
#[derive(Debug)]
pub enum ReplayRefusal {
    /// A length refusal `append` makes (E1): the key, the value, the blob
    /// threshold or half the ring.
    TooLarge,
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
    /// The boot pipeline's flush or seal failed (step 3, the hand-over
    /// drain): short or torn write, device full, a barrier.
    Flush(TierFlushError),
    /// A settle read failed (E5, E10, E12).
    SettleRead { addr: LogicalAddr, cause: SettleReadError },
    /// Settle bytes do not parse into a verified record of the slot's
    /// hash — never "distinct".
    SettleIdentity { addr: LogicalAddr, cause: ColdKeyError },
    /// A same-key settle would exceed the survivor's origin room.
    OriginRoom { winner: LogicalAddr, cold: LogicalAddr },
    /// A settle step before the end of replay was declared.
    ReplayNotEnded,
    /// A hand-over with RAM records unsettled (I16).
    Unsettled { cursor: Option<LogicalAddr>, tail: LogicalAddr },
    /// The store refused the placement after `room` answered `Fits` —
    /// disk admission is open at boot and the window was just made, so
    /// this names a defect, typed.
    Store(OpError),
}

impl core::fmt::Display for ReplayRefusal {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            ReplayRefusal::TooLarge => write!(f, "record exceeds a length bound (ADR-0174 E1)"),
            ReplayRefusal::End { len } => {
                write!(f, "no room for {len} bytes: the 48-bit end of the address space")
            }
            ReplayRefusal::RoomAsks { len, asks } => {
                write!(f, "room asked {asks} times for {len} bytes (ADR-0174 D2 rule 1 bounds 4)")
            }
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
            ReplayRefusal::Flush(e) => write!(f, "boot tier flush: {e}"),
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
            ReplayRefusal::Unsettled { cursor, tail } => write!(
                f,
                "hand-over with RAM records unsettled (cursor {:?}, tail {}) — ADR-0174 R10",
                cursor.map(LogicalAddr::to_raw),
                tail.to_raw()
            ),
            ReplayRefusal::Store(e) => write!(f, "the store refused a fitted placement: {e:?}"),
        }
    }
}

impl std::error::Error for ReplayRefusal {}

/// What a marker did (E6, E7).
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

/// The settle walk's receipt (I5): the boundary advances only to the end
/// of a span whose every live record was settled against its cold twins.
#[must_use = "a settled span is what the boundary may advance to"]
pub struct SettledSpan {
    end: LogicalAddr,
}

/// What one end-of-replay settle step reports (E12).
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
    /// Scratch: the key of the record being settled (its RAM bytes may
    /// move under the settle).
    key: Vec<u8>,
}

impl<F: SegmentFs> TierReplay<F> {
    /// A machine over a recovered boot pipeline, `Seeded`. `slice_bytes`
    /// is the namespace's `MAINTAIN-SLICE`, `page_bytes` the commit page.
    #[must_use]
    pub fn new(flush: BootFlush<F>, slice_bytes: u64, page_bytes: u64) -> TierReplay<F> {
        assert!(page_bytes.is_power_of_two(), "commit pages are powers of two");
        let lead = slice_bytes.div_ceil(page_bytes).max(1) * page_bytes;
        TierReplay {
            state: ReplayState::Seeded,
            flush,
            lead,
            counters: ReplayCounters::default(),
            work: ReplayWork::default(),
            twins: Vec::new(),
            doomed: Vec::new(),
            key: Vec::with_capacity(crate::record::MAX_KEY_LEN),
        }
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

    /// The settle read for ADR-0093's rebuild (E13): the key window of
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
        self.flush.read_key_window(addr.to_raw(), TieredTable::KEY_PREFIX_LEN)
    }

    // ---- the demote step (ADR-0174 D2) ----

    /// Makes room for a record whose `room` answered `Demote(target)`:
    /// seal one lead past the need with every sealed record settled
    /// (rule 2, rule 3), one flush with one barrier that the boot
    /// pipeline claims whole (rule 4), release until the head reaches the
    /// target. Enters `Spilling` before the first read or write.
    fn demote(
        &mut self,
        table: &mut TieredTable,
        target: LogicalAddr,
    ) -> Result<(), ReplayRefusal> {
        self.state = ReplayState::Spilling;
        self.counters.demote_steps += 1;
        if table.space.flushed() < target {
            let stop = target.to_raw().saturating_add(self.lead);
            let from = table.space.ro_boundary();
            let (span, _) = self.settle_walk(table, from, Some(stop), None)?;
            table.seal_settled(span);
            let cut = table.space.ro_boundary().to_raw();
            let cursor = table.flush_start_cursor(&self.flush);
            if cut > cursor {
                let outcome = table
                    .flush_span(&mut self.flush, cut - cursor)
                    .map_err(ReplayRefusal::Flush)?;
                self.note_flush(outcome);
            }
            if cfg!(inf_canary_replay_stall_seal) {
                // The planted canary (DRR FCR-STTIER-01 §6): the step
                // seals the file to free the partial frame, as the live
                // stall seal does — a boot file with the stall reason.
                self.flush.seal_stall_planted().map_err(ReplayRefusal::Flush)?;
            }
        }
        while table.space.head() < target {
            if table.release_slice() == 0 {
                return Err(ReplayRefusal::DemoteStalled { head: table.space.head(), target });
            }
        }
        Ok(())
    }

    fn note_flush(&mut self, outcome: FlushSliceOutcome) {
        let barriers = u64::from(outcome.files_sealed) + u64::from(outcome.appended_bytes > 0);
        self.counters.tier_bytes += outcome.appended_bytes;
        self.counters.barriers += barriers;
        self.counters.files_sealed += u64::from(outcome.files_sealed);
        self.work.tier_bytes += outcome.appended_bytes;
        self.work.barriers += barriers;
    }

    /// Walks the records of `[from, stop)` — `stop` a target address
    /// (the first record start at or above it ends the walk), else the
    /// tail — settling every live one against its exact-hash cold slots
    /// (E10). A hole is passed whole by its mark. With a byte `budget`
    /// the walk yields after the record that exhausts it. Returns the
    /// span settled and whether the walk reached the tail.
    fn settle_walk(
        &mut self,
        table: &mut TieredTable,
        from: LogicalAddr,
        stop: Option<u64>,
        budget: Option<u64>,
    ) -> Result<(SettledSpan, bool), ReplayRefusal> {
        let tail = table.space.tail().to_raw();
        let mut at = from.to_raw();
        let mut walked = 0u64;
        while at < tail {
            let here = LogicalAddr::from_raw(at).expect("watermarks stay 48-bit");
            if let Some(hole) = table.space.hole_at(here) {
                at += hole;
                continue;
            }
            if stop.is_some_and(|s| at >= s) {
                break;
            }
            let (len, hash) = {
                let parts = table.record(here);
                self.key.clear();
                self.key.extend_from_slice(parts.key);
                (parts.encoded_len as u64, table.hash_key(parts.key))
            };
            // The planted canary (DRR FCR-STTIER-01 §6): E10 skipped at
            // the seal — a sealed record leaves its same-key cold slot.
            let settle_here = stop.is_none() || !cfg!(inf_canary_replay_seal_no_settle);
            if settle_here && table.index.contains_pair(hash, here) {
                self.settle_twins(table, here, hash)?;
            }
            at += len;
            walked += len;
            if budget.is_some_and(|b| walked >= b) {
                break;
            }
        }
        self.work.walked_bytes += walked;
        let end = LogicalAddr::from_raw(at).expect("watermarks stay 48-bit");
        Ok((SettledSpan { end }, at >= tail))
    }

    /// E10 for the live RAM record at `winner` (its key in `self.key`):
    /// each exact-hash cold slot is read through the held handle, parsed
    /// into a `ColdKey` under the slot's hash, and settled when it
    /// carries the winner's key — as a ref (counted, stamped, chained,
    /// no bytes) below the origin, with its exact death above it (R8);
    /// a distinct key stays.
    fn settle_twins(
        &mut self,
        table: &mut TieredTable,
        winner: LogicalAddr,
        hash: u64,
    ) -> Result<(), ReplayRefusal> {
        self.twins.clear();
        let space = &table.space;
        let twins = &mut self.twins;
        table.index.each_exact(hash, |sibling| {
            if space.resolve(sibling) == AddrClass::Cold {
                twins.push(sibling);
            }
        });
        for i in 0..self.twins.len() {
            let cold = self.twins[i];
            let window = self
                .flush
                .read_key_window(cold.to_raw(), TieredTable::KEY_PREFIX_LEN)
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
            self.counters.settled_same_key += 1;
        }
        Ok(())
    }

    /// E5's reads for a `DEL` of `key`: every exact-hash cold slot at or
    /// above the life origin — a record this boot demoted — is read and
    /// parsed; the ones carrying the key are remembered with their exact
    /// length for the removal that follows the marker drain. Refs below
    /// the origin are the markers' (E6).
    fn verify_cold_for_delete(
        &mut self,
        table: &TieredTable,
        key: &[u8],
        hash: u64,
    ) -> Result<(), ReplayRefusal> {
        self.doomed.clear();
        if cfg!(inf_canary_replay_del_no_verify) {
            // The planted canary (DRR FCR-STTIER-01 §6): E5 skipped.
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
        for i in 0..self.twins.len() {
            let cold = self.twins[i];
            let window = self
                .flush
                .read_key_window(cold.to_raw(), TieredTable::KEY_PREFIX_LEN)
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

    /// R9 (E14), at the end of the checkpoint and before the tail: every
    /// address a boot settle chained releases its blob reference, now
    /// that the 0x05 section has registered the entries the settles
    /// preceded. Nothing for an address with no entry.
    pub fn end_of_checkpoint(&mut self, table: &mut TieredTable) {
        if cfg!(inf_canary_replay_blob_release_skip) {
            // The planted canary (DRR FCR-STTIER-01 §6): E14 skipped.
            return;
        }
        if table.reloc_origins.is_empty() {
            return;
        }
        let mut released = 0u64;
        for origins in table.reloc_origins.values() {
            for &(addr, _) in origins {
                if table.extents.reference_at(addr).is_some() {
                    table.extents.note_death(addr);
                    released += 1;
                }
            }
        }
        self.counters.blob_releases += released;
    }

    /// R10's first half (E12): replay has ended. A machine that demoted
    /// enters `Settling` with its cursor at `ro`; one that did not is
    /// ready to hand over.
    pub fn end_of_replay(&mut self, table: &TieredTable) {
        if self.state == ReplayState::Spilling {
            let cursor = if cfg!(inf_canary_replay_no_end_settle) {
                // The planted canary (DRR FCR-STTIER-01 §6): E12 skipped.
                table.space.tail()
            } else {
                table.space.ro_boundary()
            };
            self.state = ReplayState::Settling { cursor };
        }
    }

    /// One end-of-replay settle step (E12): E10 on every live record of
    /// `[cursor, tail)`, by address, until `budget_bytes` were walked or
    /// the tail is reached. `ro` does not move.
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
        let (span, at_tail) = self.settle_walk(table, cursor, None, Some(budget_bytes.max(1)))?;
        self.state = ReplayState::Settling { cursor: span.end };
        Ok(if at_tail { SettleProgress::Done } else { SettleProgress::More })
    }

    /// R10's last half (E13): a table that demoted drains its flush and
    /// seals its active file; the machine hands the plane a pipeline
    /// under the live claim rule with a handle for every sealed
    /// catalogue file. Only a machine with every RAM record settled gets
    /// here (I16): `Spilling` has no arm, `Settling` only at the tail.
    ///
    /// # Errors
    /// `Unsettled`, or the drain's flush error.
    pub fn hand_over(mut self, table: &mut TieredTable) -> Result<HandedOver<F>, ReplayRefusal> {
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
        let drained = table.flush_drain(&mut self.flush).map_err(ReplayRefusal::Flush)?;
        self.note_flush(drained);
        let hand = self.flush.hand_over().map_err(ReplayRefusal::Flush)?;
        let sealed_now = hand.flush.sealed().len() - sealed_before;
        debug_assert!(sealed_now <= 1, "the drain seals at most the active file");
        Ok(hand)
    }
}

impl TieredTable {
    // ---- the replay entries (ADR-0174 D1, D3; DRR FCR-STTIER-01 E1–E5) ----

    /// Replays one checkpoint image or tail `SET` (R5): the typed length
    /// refusals first, then the room question until it answers `Fits` —
    /// a demote step or a pad per answer, at most four asks — then the
    /// parked markers (E6, E7), then the blind key-verified upsert: a RAM
    /// record of the key is overwritten in place of its slot with its
    /// origins moved to the new record (E2); none inserts at the tail,
    /// cold exact-hash slots untouched and unread (E3).
    ///
    /// # Errors
    /// [`ReplayRefusal`] — the boot's typed refusal; nothing of this
    /// record was applied.
    pub fn replay_upsert<F: SegmentFs>(
        &mut self,
        mut replay: Option<&mut TierReplay<F>>,
        markers: &[LogicalAddr],
        key: &[u8],
        value: &[u8],
        hash: u64,
    ) -> Result<LogicalAddr, ReplayRefusal> {
        let len = self.admit_inline(key, value).map_err(|_| ReplayRefusal::TooLarge)?;
        self.make_room(replay.as_deref_mut(), len)?;
        self.drain_markers(replay.as_deref_mut(), markers, hash);
        let placed = self.apply_image(key, value, hash).map_err(ReplayRefusal::Store)?;
        if let Some(r) = replay
            && r.state == ReplayState::Seeded
        {
            r.state = ReplayState::Fitting;
        }
        Ok(placed)
    }

    /// [`replay_upsert`](Self::replay_upsert) for a tag-9 image or a tail
    /// `StringExtentRef` (R5 over the extent kind).
    ///
    /// # Errors
    /// As [`replay_upsert`](Self::replay_upsert).
    pub fn replay_upsert_extent<F: SegmentFs>(
        &mut self,
        mut replay: Option<&mut TierReplay<F>>,
        markers: &[LogicalAddr],
        key: &[u8],
        hash: u64,
        ext: ExtentRef,
    ) -> Result<LogicalAddr, ReplayRefusal> {
        let len = self.admit_extent(key, ext).map_err(|_| ReplayRefusal::TooLarge)?;
        self.make_room(replay.as_deref_mut(), len)?;
        self.drain_markers(replay.as_deref_mut(), markers, hash);
        let placed = self.apply_extent_image(key, hash, ext).map_err(ReplayRefusal::Store)?;
        if let Some(r) = replay
            && r.state == ReplayState::Seeded
        {
            r.state = ReplayState::Fitting;
        }
        Ok(placed)
    }

    /// Replays one tail `DEL` (R6): the settle read of each exact-hash
    /// cold slot this boot demoted first (E5), then the parked markers,
    /// then the RAM record of the key — with its origins — and the read
    /// slots whose key matched, each with its exact death. Returns
    /// whether anything of the key was removed.
    ///
    /// # Errors
    /// A settle read or identity refusal; nothing changed.
    pub fn replay_delete<F: SegmentFs>(
        &mut self,
        mut replay: Option<&mut TierReplay<F>>,
        markers: &[LogicalAddr],
        key: &[u8],
        hash: u64,
    ) -> Result<bool, ReplayRefusal> {
        if let Some(r) = replay.as_deref_mut() {
            r.verify_cold_for_delete(self, key, hash)?;
        }
        self.drain_markers(replay.as_deref_mut(), markers, hash);
        let mut removed = self.apply_delete(key, hash);
        if let Some(r) = replay {
            for i in 0..r.doomed.len() {
                let (cold, len) = r.doomed[i];
                self.index.remove(hash, cold);
                self.shadow_note_removed(cold);
                self.note_death(cold, u64::from(len));
                if !self.reloc_origins.is_empty() {
                    self.reloc_origins.remove(&(hash, cold.to_raw()));
                }
                r.counters.deletes_verified += 1;
                r.counters.settled_same_key += 1;
                removed = true;
            }
            r.doomed.clear();
        }
        Ok(removed)
    }

    /// Replays one checkpoint address reference (R1, E8): insert unless
    /// the exact `(hash, addr)` pair is slotted (the walker's at-least-once
    /// re-emission may duplicate a ref). Counts the slot into its file
    /// exactly once per surviving slot (ADR-0058 D4). Live-byte
    /// accounting is untouched: a ref's length is unknown without a read.
    /// E9 — a ref section after a record of this life — is
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
    /// stamped, its blob reference released (E6); absence is a legal
    /// interleaving. At or above the origin the marker names a
    /// crashed-life address, nothing in this life: no-op (E7) — the
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

    /// E1's room question, asked until it answers `Fits`: `Demote` runs
    /// the step, `Pad` moves the tail, `End` and a fifth ask refuse typed.
    fn make_room<F: SegmentFs>(
        &mut self,
        mut replay: Option<&mut TierReplay<F>>,
        len: usize,
    ) -> Result<(), ReplayRefusal> {
        let mut asks = 0u32;
        loop {
            asks += 1;
            if asks > REPLAY_ROOM_ASKS_MAX {
                return Err(ReplayRefusal::RoomAsks { len, asks });
            }
            match self.space.room(len) {
                Room::Fits => return Ok(()),
                Room::End => return Err(ReplayRefusal::End { len }),
                Room::Demote(target) => {
                    if cfg!(inf_canary_replay_no_demote) {
                        // The planted canary (DRR FCR-STTIER-01 §6): the
                        // refusal HEAD made.
                        return Err(ReplayRefusal::Store(OpError::OutOfMemory));
                    }
                    let Some(r) = replay.as_deref_mut() else {
                        return Err(ReplayRefusal::NoPipeline { need: Room::Demote(target) });
                    };
                    r.demote(self, target)?;
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

    /// The marker drain, after E1 and E5 and before the mutation: each
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
        if let Some(r) = replay {
            r.counters.markers_skipped += skipped;
        }
    }

    /// The boundary's one boot advance (I5): to the end of a settled span.
    fn seal_settled(&mut self, span: SettledSpan) {
        let from = self.space.ro_boundary().to_raw();
        if span.end.to_raw() > from {
            self.space.advance_ro_boundary(span.end);
            self.space.note_demote_slice(span.end.to_raw() - from);
        }
    }
}
