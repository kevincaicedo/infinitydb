//! Boot replay of one tiered namespace on one cell (ADR-0174): the replay
//! state machine, the demote step that makes room when the RAM window
//! fills (D2), the settle walk that keeps a sealed record from leaving a
//! same-key cold slot behind (D3 R7), the end-of-replay cursor (R10), the
//! end-of-checkpoint blob release (R9) and the hand-over to the plane.
//! Every replay append and delete enters through
//! [`TieredTable::replay_upsert`], [`replay_upsert_extent`]
//! (TieredTable::replay_upsert_extent) and [`TieredTable::replay_delete`],
//! which hold the demote machinery: no tiered replay append exists outside
//! them (D1), the window's refusal is a [`Room`], never an error variant,
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
use crate::limits::{REPLAY_ROOM_ASKS_MAX, SETTLE_READ_CHARGE_BYTES};
use crate::record::{ColdKey, ColdKeyError};
use crate::tiered::shadow::SettleCase;

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

/// The replay seam: lends a namespace's boot machine to the keyspace's
/// tiered replay arms, one lookup per record. The machine accumulates
/// the boot I/O it does ([`ReplayWork`]); the seam's owner drains it with
/// [`TierReplay::take_work`] where it decides whether to yield — at a
/// frame or section boundary, and per end-of-replay settle step — so no
/// record pays for the charge. `None` is a namespace with no machine:
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
    /// A length refusal `append` makes, before room (D1): the key, the
    /// value, the blob threshold or half the ring.
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
}

impl core::fmt::Display for ReplayRefusal {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            ReplayRefusal::TooLarge => write!(f, "record exceeds a length bound (ADR-0174 D1)"),
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
/// so no RAM record below it has a same-key cold slot.
#[must_use = "a settled span is what the boundary may advance to"]
pub struct SettledSpan {
    end: LogicalAddr,
}

/// Where a settle walk goes: a demote step's seal walk to a target
/// address, or one end-of-replay step to a budget.
#[derive(Copy, Clone, Debug)]
enum WalkTo {
    /// To the first record start at or above `stop` (D2 rule 2).
    Seal { stop: u64 },
    /// Until the step's charge reaches `budget_bytes` (R10).
    Settle { budget_bytes: u64 },
}

/// How a settle walk ended.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum WalkEnd {
    /// At the tail: every record from the walk's start is settled.
    Tail,
    /// At its stop or its budget, short of the tail.
    Stopped,
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
            // Bound: `H + lead − ro` bytes walked, plus one record.
            let (span, _) = self.settle_walk(table, from, WalkTo::Seal { stop })?;
            table.seal_settled(span);
            let cut = table.space.ro_boundary().to_raw();
            let cursor = table.flush_start_cursor(&self.flush);
            // One span, one barrier, one more per gap or capacity seal it
            // crosses. Files sealed per boot have no limit of their own:
            // ⌊D ÷ file capacity⌋ + ⌈D ÷ ring⌉ + page pads + the hand-over's
            // one, for `D` bytes demoted — the device and the handle limit
            // bound them (a typed refusal at a create).
            if cut > cursor {
                let outcome = table
                    .flush_span(&mut self.flush, cut - cursor)
                    .map_err(|cause| self.flush_refusal(table, cause))?;
                self.note_flush(outcome);
            }
            if cfg!(inf_canary_replay_stall_seal) {
                // The planted canary breaks D2 rule 5: the step seals the
                // file to free the partial frame, as the live stall seal
                // does — a boot file with the stall reason.
                let planted = self.flush.seal_stall_planted();
                planted.map_err(|cause| self.flush_refusal(table, cause))?;
            }
        }
        // Bound: ⌈(target − head) ÷ slice⌉ releases, whole pages each.
        while table.space.head() < target {
            if table.release_slice() == 0 {
                return Err(ReplayRefusal::DemoteStalled { head: table.space.head(), target });
            }
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

    /// Walks the records from `from`, settling every live one against
    /// its exact-hash cold slots (R7), to where `to` says: a demote
    /// step's seal walk stops at the first record start at or above its
    /// target; an end-of-replay step yields after the record whose charge
    /// — the bytes walked plus [`SETTLE_READ_CHARGE_BYTES`] per settle
    /// read — reaches its budget. Either ends at the tail. A hole is
    /// passed whole by its mark. Returns the span settled and how the
    /// walk ended.
    ///
    /// Bound per call: a seal walk passes `H + lead − ro` bytes plus one
    /// record; an end-of-replay step charges at most its budget plus one
    /// record's bytes and its twins' reads (an exact-hash group).
    fn settle_walk(
        &mut self,
        table: &mut TieredTable,
        from: LogicalAddr,
        to: WalkTo,
    ) -> Result<(SettledSpan, WalkEnd), ReplayRefusal> {
        let tail = table.space.tail().to_raw();
        let mut at = from.to_raw();
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
            if let WalkTo::Seal { stop } = to
                && at >= stop
            {
                break WalkEnd::Stopped;
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
            if let WalkTo::Settle { budget_bytes } = to {
                let reads = self.counters.settle_reads - reads_before;
                let charge = walked.saturating_add(reads.saturating_mul(SETTLE_READ_CHARGE_BYTES));
                if charge >= budget_bytes {
                    break if at >= tail { WalkEnd::Tail } else { WalkEnd::Stopped };
                }
            }
        };
        if matches!(to, WalkTo::Settle { .. }) {
            self.work.walked_bytes += walked;
        }
        let end_addr = LogicalAddr::from_raw(at).expect("watermarks stay 48-bit");
        Ok((SettledSpan { end: end_addr }, end))
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

    /// R9, at the end of the checkpoint and before the tail: every
    /// address a boot settle chained releases its blob reference, now
    /// that the 0x05 section has registered the entries the settles
    /// preceded. Nothing for an address with no entry.
    pub fn end_of_checkpoint(&mut self, table: &mut TieredTable) {
        if cfg!(inf_canary_replay_blob_release_skip) {
            // The planted canary breaks R9: a settled ref's blob
            // reference stays with no slot.
            return;
        }
        if table.reloc_origins.is_empty() {
            return;
        }
        // Bound: one pass over the origin map, once per boot — at most the
        // refs settled during image load, each a map lookup.
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

    /// R10's first half: replay has ended. A machine that demoted
    /// enters `Settling` with its cursor at `ro`; one that did not is
    /// ready to hand over.
    pub fn end_of_replay(&mut self, table: &TieredTable) {
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
    /// `[cursor, tail)`, by address, until the step's charge — bytes
    /// walked plus [`SETTLE_READ_CHARGE_BYTES`] per settle read — reaches
    /// `budget_bytes`, or the tail. `ro` does not move.
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
        let (span, end) = self.settle_walk(table, cursor, WalkTo::Settle { budget_bytes })?;
        self.state = ReplayState::Settling { cursor: span.end };
        Ok(match end {
            WalkEnd::Tail => SettleProgress::Done,
            WalkEnd::Stopped => SettleProgress::More,
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

    /// Replays one checkpoint image or tail `SET` (R5): the typed length
    /// refusals first, then the room question until it answers `Fits` —
    /// a demote step or a pad per answer, at most four asks — then the
    /// parked markers (R3, R4), then the blind key-verified upsert: a RAM
    /// record of the key is overwritten in place of its slot with its
    /// origins moved to the new record; none inserts at the tail, cold
    /// exact-hash slots untouched and unread (R5).
    ///
    /// # Errors
    /// [`ReplayRefusal`] — the boot's typed refusal; nothing of this
    /// record was applied.
    pub fn replay_upsert<F: SegmentFs>(
        &mut self,
        replay: Option<&mut TierReplay<F>>,
        markers: &[LogicalAddr],
        key: &[u8],
        value: &[u8],
        hash: u64,
    ) -> Result<LogicalAddr, ReplayRefusal> {
        let admitted = self.admit_inline(key, value).map_err(|_| ReplayRefusal::TooLarge)?;
        self.replay_place(replay, markers, hash, admitted.len(), |t| t.apply_image(admitted, hash))
    }

    /// [`replay_upsert`](Self::replay_upsert) for a tag-9 image or a tail
    /// `StringExtentRef` (R5 over the extent kind).
    ///
    /// # Errors
    /// As [`replay_upsert`](Self::replay_upsert).
    pub fn replay_upsert_extent<F: SegmentFs>(
        &mut self,
        replay: Option<&mut TierReplay<F>>,
        markers: &[LogicalAddr],
        key: &[u8],
        hash: u64,
        ext: ExtentRef,
    ) -> Result<LogicalAddr, ReplayRefusal> {
        let len = self.admit_extent(key, ext).map_err(|_| ReplayRefusal::TooLarge)?;
        TieredTable::extent_admission_cost(ext, len).map_err(|_| ReplayRefusal::TooLarge)?;
        self.replay_place(replay, markers, hash, len, |t| t.apply_extent_image(key, hash, ext))
    }

    /// The placement sequence both upserts share, after their length
    /// refusals: the state (no record after the end of replay), the room
    /// question, the parked markers, the arm, and `Seeded` → `Fitting` —
    /// one read of the state, one ask of `room` for a record that fits.
    fn replay_place<F: SegmentFs>(
        &mut self,
        mut replay: Option<&mut TierReplay<F>>,
        markers: &[LogicalAddr],
        hash: u64,
        len: usize,
        arm: impl FnOnce(&mut TieredTable) -> Result<LogicalAddr, OpError>,
    ) -> Result<LogicalAddr, ReplayRefusal> {
        let seeded = match replay.as_deref().map(|r| r.state) {
            Some(ReplayState::Settling { .. }) => return Err(ReplayRefusal::ReplayEnded),
            Some(ReplayState::Seeded) => true,
            Some(ReplayState::Fitting | ReplayState::Spilling) | None => false,
        };
        self.make_room(replay.as_deref_mut(), len)?;
        self.drain_markers(replay.as_deref_mut(), markers, hash);
        let placed = arm(self).map_err(ReplayRefusal::Store)?;
        if seeded && let Some(r) = replay {
            debug_assert_eq!(r.state, ReplayState::Seeded, "a seeded table's first record fits");
            r.state = ReplayState::Fitting;
        }
        Ok(placed)
    }

    /// Replays one tail `DEL` (R6): the settle read of each exact-hash
    /// cold slot this boot demoted first, then the parked markers,
    /// then the RAM record of the key — with its origins — and the read
    /// slots whose key matched, each with its exact death. Returns
    /// whether anything of the key was removed. A cold slot at or above
    /// the origin exists only once a demote step began, so `Seeded` and
    /// `Fitting` read and probe nothing.
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
            match r.state {
                ReplayState::Settling { .. } => return Err(ReplayRefusal::ReplayEnded),
                ReplayState::Spilling => return self.replay_delete_spilling(r, markers, key, hash),
                ReplayState::Seeded | ReplayState::Fitting => {}
            }
        }
        self.drain_markers(replay, markers, hash);
        Ok(self.apply_delete(key, hash))
    }

    /// [`replay_delete`](Self::replay_delete) once a demote step began:
    /// R6's reads before anything changes, then the drain, the RAM
    /// delete and the verified cold slots' removal.
    fn replay_delete_spilling<F: SegmentFs>(
        &mut self,
        r: &mut TierReplay<F>,
        markers: &[LogicalAddr],
        key: &[u8],
        hash: u64,
    ) -> Result<bool, ReplayRefusal> {
        r.verify_cold_for_delete(self, key, hash)?;
        self.drain_markers(Some(&mut *r), markers, hash);
        let mut removed = self.apply_delete(key, hash);
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
        Ok(removed)
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

    /// D2 rule 1's room question, asked until it answers `Fits`: `Demote` runs
    /// the step, `Pad` moves the tail, `End` and a fifth ask refuse typed.
    /// The record that fits — every record of a boot that fits — asks
    /// once and leaves; the loop is the out-of-line rest. The two hints
    /// are measured: without them a replayed `SET` through a lent machine
    /// costs 26 more instructions (the fitting-boot replay A/B).
    #[inline]
    fn make_room<F: SegmentFs>(
        &mut self,
        replay: Option<&mut TierReplay<F>>,
        len: usize,
    ) -> Result<(), ReplayRefusal> {
        let first = self.space.room(len);
        if matches!(first, Room::Fits) {
            return Ok(());
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
    ) -> Result<(), ReplayRefusal> {
        let mut answer = Some(first);
        let mut asks = 0u32;
        loop {
            asks += 1;
            if asks > REPLAY_ROOM_ASKS_MAX {
                return Err(ReplayRefusal::RoomAsks { len, asks });
            }
            let room = answer.take().unwrap_or_else(|| self.space.room(len));
            match room {
                Room::Fits => return Ok(()),
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

    /// The boundary's one boot advance (R7): to the end of a settled span.
    fn seal_settled(&mut self, span: SettledSpan) {
        let from = self.space.ro_boundary().to_raw();
        if span.end.to_raw() > from {
            self.space.advance_ro_boundary(span.end);
            self.space.note_demote_slice(span.end.to_raw() - from);
        }
    }
}

#[cfg(test)]
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

    fn machine(fs: &MemFs) -> TierReplay<MemFs> {
        let flush = TierFlush::new(
            fs.clone(),
            TierFlushConfig {
                shard_dir: Path::new("shard-0").to_path_buf(),
                cell: 0,
                ns: inf_log::NsId(9),
                mode: TierIoMode::Buffered,
                file_capacity: 1 << 20,
                slice_bytes: PAGE,
            },
            0,
        );
        TierReplay::new(BootFlush::new(flush, Vec::new()), PAGE, PAGE)
    }

    /// Two windows of records through the entry: the machine demotes.
    fn spill(table: &mut TieredTable, replay: &mut TierReplay<MemFs>) {
        let value = vec![0x5A; 900];
        for i in 0..160u32 {
            let key = format!("k:{i:04}").into_bytes();
            let hash = table.hash_key(&key);
            table.replay_upsert(Some(replay), &[], &key, &value, hash).expect("replays");
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
        let mut replay = machine(&fs);
        spill(&mut table, &mut replay);
        assert!(matches!(replay.settle_step(&mut table, PAGE), Err(ReplayRefusal::ReplayNotEnded)));
        let Err(err) = replay.hand_over(&mut table) else { panic!("Spilling has no arm") };
        assert!(matches!(err, ReplayRefusal::Unsettled { cursor: None, .. }), "{err}");
        let mut table = table_after(&fs);
        let mut replay = machine(&fs);
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
        let mut replay = machine(&fs);
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
        let mut replay = machine(&fs);
        spill(&mut table, &mut replay);
        replay.end_of_replay(&table);
        let (tail, len, counters) = (table.space().tail(), table.len(), replay.counters());
        let value = vec![0x5A; 900];
        let hash = table.hash_key(b"late");
        let err = table
            .replay_upsert(Some(&mut replay), &[], b"late", &value, hash)
            .expect_err("a SET after the end of replay");
        assert!(matches!(err, ReplayRefusal::ReplayEnded), "{err}");
        let ext = ExtentRef { extent_id: 1, offset: 0, len: 4096 };
        let err = table
            .replay_upsert_extent(Some(&mut replay), &[], b"late", hash, ext)
            .expect_err("an extent reference after the end of replay");
        assert!(matches!(err, ReplayRefusal::ReplayEnded), "{err}");
        let old = table.hash_key(b"k:0000");
        let err = table
            .replay_delete(Some(&mut replay), &[], b"k:0000", old)
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
        let mut replay = machine(&fs);
        spill(&mut table, &mut replay);
        // Rewrite the demoted keys inside the last window: each rewrite is
        // an `Open` record with a cold twin when replay ends.
        let value = vec![0x6B; 900];
        for i in 0..56u32 {
            let key = format!("k:{i:04}").into_bytes();
            let hash = table.hash_key(&key);
            table.replay_upsert(Some(&mut replay), &[], &key, &value, hash).expect("replays");
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
            match table.replay_upsert::<MemFs>(None, &[], &key, &value, hash) {
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
