//! Cell-local log staging (M2-S03): the §7.1 **log-staging domain** where
//! one iteration's [`MutationEffect`]s accumulate during EXECUTE and are
//! drained into the active frame at LOG (L2, L3).
//!
//! The domain is realized as a **double-buffered frame pair**, not a
//! wrap-around ring (ADR-0012): a frame must be physically contiguous for
//! the one-`writev`-per-iteration rule (L3), and a wrap would split it.
//! One buffer accepts appends (EXECUTE); the other may be sealed and
//! **leased** to an in-flight write until its completion arrives (the
//! M2-S05 write→fsync CQE). Both buffers are fixed-capacity, allocated
//! once at cell construction — the append path performs zero heap
//! allocation by construction (L5; asserted by `tests/staging_alloc.rs`).
//!
//! Backpressure is explicit and bounded (never an unbounded queue):
//! - [`StagingRing::stage`] refuses with typed [`StagingFull`] when the
//!   record would overflow the staging buffer — the signal the server
//!   layer uses to stop re-arming reads on connections writing to durable
//!   namespaces that iteration (wired with S05/S08).
//! - [`StagingRing::seal`] requires the previous lease released: at most
//!   one frame is in flight, so domain memory is `2 × capacity`, always.
//!
//! LSN handoff: [`stage`](StagingRing::stage) returns a [`StagedAt`]
//! generation token; after LOG reserves the frame's base, the
//! [`FrameLease`] resolves each token to the record's real LSN — the input
//! the S06 `WatermarkGate` registration consumes. Stale tokens are
//! unrepresentable-by-accident: generations are checked.

use core::fmt;

use crate::effect::MutationEffect;
use crate::frame::{
    DEFAULT_MAX_FRAME_LEN, FRAME_HEADER_LEN, FRAME_TRAILER_LEN, FrameBuilder, FrameStamp,
};
use crate::fs::SegmentFs;
use crate::lsn::Lsn;
use crate::segment::{LogError, SegmentRotor};

/// Default per-buffer capacity of the log-staging domain (bytes). One
/// iteration's records are far smaller in practice; the bound exists to be
/// hit only under pathological pipelining, where it *must* push back.
pub const DEFAULT_STAGING_BYTES: u32 = 4 << 20;

/// Log-staging domain configuration (per cell).
#[derive(Copy, Clone, Debug)]
pub struct StagingConfig {
    /// Capacity of each staging buffer: the maximum frame (header + records
    /// + trailer) one iteration may emit.
    pub capacity_bytes: u32,
}

impl Default for StagingConfig {
    fn default() -> Self {
        StagingConfig { capacity_bytes: DEFAULT_STAGING_BYTES }
    }
}

/// Typed admission refusal: the record does not fit the staging buffer
/// this iteration. This is backpressure, not failure — the caller stops
/// re-arming durable-namespace reads and retries after the LOG drain.
///
/// One case is not retryable: `needed >
/// [`max_record_len`](StagingRing::max_record_len)` can never fit any
/// drain. Admission (M2-S08) must check that bound up front and reject
/// oversized durable writes with a user-facing error instead of retrying
/// — retrying it is a livelock.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct StagingFull {
    /// Encoded size the record needs.
    pub needed: u32,
    /// Bytes still available before the frame would exceed capacity.
    pub available: u32,
}

impl fmt::Display for StagingFull {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "staging full: record needs {} bytes, {} available — durable admission must pause",
            self.needed, self.available
        )
    }
}

impl std::error::Error for StagingFull {}

/// Where a staged record sits: a generation-checked token, resolved to the
/// record's LSN by the [`FrameLease`] of the same generation. `Copy` so
/// pending response futures can hold it across the EXECUTE→LOG boundary
/// (it owns nothing — no buffer borrow crosses an iteration, per the
/// buffer-lifecycle rule).
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct StagedAt {
    generation: u64,
    /// Byte offset of the record's length prefix within the frame body.
    body_offset: u32,
}

/// Exclusive handle on the sealed, in-flight frame: produced by
/// [`StagingRing::seal`], surrendered to [`StagingRing::release`] when the
/// covering write completes. Not `Clone`: one frame, one lease. Dropping a
/// lease without releasing blocks the next seal — a leak is loud, never a
/// corruption.
#[derive(Debug)]
#[must_use = "an in-flight frame lease must be released on write completion"]
pub struct FrameLease {
    generation: u64,
    first_record_lsn: Lsn,
    frame_len: u32,
    record_count: u32,
}

impl FrameLease {
    /// LSN of the frame's first record.
    #[must_use]
    pub fn first_record_lsn(&self) -> Lsn {
        self.first_record_lsn
    }

    /// Total sealed frame bytes (header + records + trailer).
    #[must_use]
    pub fn frame_len(&self) -> u32 {
        self.frame_len
    }

    /// Records carried by the sealed frame.
    #[must_use]
    pub fn record_count(&self) -> u32 {
        self.record_count
    }

    /// Resolve a staged-record token to its durable LSN — the value an
    /// `always` response future gates on (M2-S06 `WatermarkGate`).
    ///
    /// # Panics
    /// If `at` belongs to a different staging generation — a stale token
    /// is an internal invariant violation of the LOG step, never a runtime
    /// condition.
    #[must_use]
    pub fn lsn_of(&self, at: StagedAt) -> Lsn {
        assert_eq!(
            at.generation, self.generation,
            "stale StagedAt: token generation does not match this lease"
        );
        self.first_record_lsn.advance(at.body_offset)
    }
}

/// Cumulative staging counters (cell-local, no atomics — L1).
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct StagingStats {
    pub appends: u64,
    /// Sum of encoded record bytes accepted (the cumulative side of the
    /// `log_staging_bytes` accounting).
    pub append_bytes: u64,
    /// Typed `StagingFull` refusals — the backpressure tripwire input.
    pub refusals: u64,
    pub seals: u64,
    pub releases: u64,
}

struct InFlight {
    buf: usize,
    generation: u64,
}

/// The log-staging domain of one cell. Single-threaded by design (L1):
/// EXECUTE appends, LOG seals and commits, the write completion releases —
/// all on the cell thread.
pub struct StagingRing {
    bufs: [FrameBuilder; 2],
    staging: usize,
    in_flight: Option<InFlight>,
    /// Generation of the buffer currently accepting appends; bumped at
    /// every seal so tokens and leases cannot cross iterations silently.
    generation: u64,
    /// The log life every sealed frame is stamped with (ADR-0031 D6):
    /// 1 for fresh logs and pre-recovery tiers; recovery-derived otherwise
    /// (`SegmentRotor::resume_epoch`, wired at cell assembly).
    frame_epoch: u32,
    /// Next frame ordinal within `frame_epoch` (from 1, +1 per seal).
    next_frame_seq: u64,
    capacity_bytes: u32,
    stats: StagingStats,
}

impl fmt::Debug for StagingRing {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StagingRing")
            .field("capacity_bytes", &self.capacity_bytes)
            .field("staged_bytes", &self.staged_bytes())
            .field("pending_records", &self.pending_records())
            .field("in_flight", &self.in_flight.is_some())
            .field("generation", &self.generation)
            .field("stats", &self.stats)
            .finish()
    }
}

impl StagingRing {
    /// Allocate the domain: two fixed buffers of `capacity_bytes` each
    /// (`resident_bytes` = 2 × capacity, attributed to the log-staging
    /// domain — L5). This is cell construction, the one allowed allocation
    /// point; the append path never allocates again.
    ///
    /// # Panics
    /// If `capacity_bytes` cannot hold a minimal frame or exceeds
    /// [`DEFAULT_MAX_FRAME_LEN`] (every written frame must be readable by
    /// a default-configured reader) — boot-configuration invariants.
    #[must_use]
    pub fn new(cfg: StagingConfig) -> StagingRing {
        let min = (FRAME_HEADER_LEN + FRAME_TRAILER_LEN + 4) as u32;
        assert!(cfg.capacity_bytes >= min, "staging capacity below one minimal frame");
        assert!(
            cfg.capacity_bytes <= DEFAULT_MAX_FRAME_LEN,
            "staging capacity exceeds the frame decoder bound"
        );
        let capacity = cfg.capacity_bytes as usize;
        StagingRing {
            bufs: [FrameBuilder::with_capacity(capacity), FrameBuilder::with_capacity(capacity)],
            staging: 0,
            in_flight: None,
            generation: 0,
            frame_epoch: 1,
            next_frame_seq: 1,
            capacity_bytes: cfg.capacity_bytes,
            stats: StagingStats::default(),
        }
    }

    /// Adopt the recovery-derived log life (ADR-0031 D5/D6): every frame
    /// sealed from here on stamps `epoch`, with `seq` restarting at 1.
    /// Boot wiring only — before the first seal.
    ///
    /// # Panics
    /// If a frame was already sealed under the construction-default epoch
    /// (mixing lives within one ring is an assembly bug), or `epoch == 0`
    /// (reserved).
    pub fn set_frame_epoch(&mut self, epoch: u32) {
        assert!(epoch > 0, "frame epoch 0 is reserved (ADR-0031 D1)");
        assert_eq!(self.stats.seals, 0, "set_frame_epoch after a seal (boot wiring only)");
        self.frame_epoch = epoch;
        self.next_frame_seq = 1;
    }

    /// Append one effect's record in place (EXECUTE step). Refuses with
    /// typed [`StagingFull`] when the frame would exceed capacity — the
    /// caller's backpressure signal; the effect is *not* partially staged.
    pub fn stage(&mut self, effect: &MutationEffect<'_>) -> Result<StagedAt, StagingFull> {
        let record = effect.record();
        let needed = record.encoded_len() as u32;
        let available = self.remaining_capacity();
        if needed > available {
            self.stats.refusals += 1;
            return Err(StagingFull { needed, available });
        }
        let builder = &mut self.bufs[self.staging];
        let body_offset = builder.frame_len() - (FRAME_HEADER_LEN + FRAME_TRAILER_LEN) as u32;
        builder.append(&record);
        self.stats.appends += 1;
        self.stats.append_bytes += u64::from(needed);
        Ok(StagedAt { generation: self.generation, body_offset })
    }

    /// Bytes a further record may still occupy this iteration.
    #[must_use]
    pub fn remaining_capacity(&self) -> u32 {
        self.capacity_bytes - self.bufs[self.staging].frame_len()
    }

    /// True when `stage` would accept a record of `encoded_len` bytes —
    /// the admission pre-check for the read-rearm decision (S05/S08).
    #[must_use]
    pub fn would_fit(&self, encoded_len: usize) -> bool {
        u32::try_from(encoded_len).is_ok_and(|len| len <= self.remaining_capacity())
    }

    /// The largest record an *empty* ring can stage. A record above this
    /// bound can never be staged by any drain: admission must reject it
    /// with a documented error, never retry it (see [`StagingFull`]).
    /// Boot configuration owns the invariant that this bound covers the
    /// store's maximum record encoding.
    #[must_use]
    pub fn max_record_len(&self) -> u32 {
        self.capacity_bytes - (FRAME_HEADER_LEN + FRAME_TRAILER_LEN) as u32
    }

    /// No records staged this iteration (empty iterations emit no frame).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.bufs[self.staging].is_empty()
    }

    /// Records staged and not yet sealed.
    #[must_use]
    pub fn pending_records(&self) -> u32 {
        self.bufs[self.staging].record_count()
    }

    /// Sum of encoded record bytes staged and not yet sealed — the live
    /// `log_staging_bytes` gauge, exact at every append/seal site (L5).
    #[must_use]
    pub fn staged_bytes(&self) -> u32 {
        self.bufs[self.staging].frame_len() - (FRAME_HEADER_LEN + FRAME_TRAILER_LEN) as u32
    }

    /// Total frame bytes the pending records would seal into — what
    /// `SegmentRotor::begin_frame` must reserve.
    #[must_use]
    pub fn pending_frame_len(&self) -> u32 {
        self.bufs[self.staging].frame_len()
    }

    /// Bytes of the sealed in-flight frame (0 when none).
    #[must_use]
    pub fn in_flight_bytes(&self) -> u32 {
        match &self.in_flight {
            Some(in_flight) => u32::try_from(self.bufs[in_flight.buf].sealed_frame().len())
                .expect("frame fits u32"),
            None => 0,
        }
    }

    /// Fixed domain memory: both buffers, allocated at construction (L5
    /// attribution: the log-staging domain line of `INFO memory`).
    #[must_use]
    pub fn resident_bytes(&self) -> usize {
        2 * self.capacity_bytes as usize
    }

    /// The configured per-buffer capacity — the admission bound the
    /// `log_staging_capacity_bytes` observable exports (M4.5-S27).
    #[must_use]
    pub fn capacity_bytes(&self) -> u32 {
        self.capacity_bytes
    }

    /// True when the previous frame is still in flight: sealing must wait
    /// for its release (bounded: at most one in-flight frame).
    #[must_use]
    pub fn backlogged(&self) -> bool {
        self.in_flight.is_some()
    }

    /// True when the LOG step may seal now: records are pending and no
    /// earlier frame is still in flight.
    #[must_use]
    pub fn can_seal(&self) -> bool {
        !self.is_empty() && self.in_flight.is_none()
    }

    /// Seal the pending records into a frame at `first_record_lsn` (from
    /// the rotor's reserved slot), stamped with the current epoch/seq and
    /// `covered_lsn` — the group-commit durability watermark at this LOG
    /// step (`Lsn::to_u64`; 0 when nothing is covered yet — the ADR-0031
    /// D1 attestation). Swaps staging to the free buffer. The sealed frame
    /// stays resident under the returned lease until
    /// [`release`](Self::release).
    ///
    /// # Panics
    /// If nothing is staged or the previous lease is unreleased — LOG-step
    /// invariants; callers check [`can_seal`](Self::can_seal).
    pub fn seal(&mut self, first_record_lsn: Lsn, covered_lsn: u64) -> FrameLease {
        assert!(!self.is_empty(), "seal with no staged records");
        assert!(self.in_flight.is_none(), "seal while a frame lease is outstanding");
        let sealed = self.staging;
        let generation = self.generation;
        let stamp = FrameStamp { epoch: self.frame_epoch, seq: self.next_frame_seq, covered_lsn };
        let builder = &mut self.bufs[sealed];
        let record_count = builder.record_count();
        builder.finalize(first_record_lsn, stamp);
        let frame_len = u32::try_from(builder.sealed_frame().len()).expect("frame fits u32");
        self.in_flight = Some(InFlight { buf: sealed, generation });
        self.staging = 1 - sealed;
        self.generation += 1;
        self.next_frame_seq += 1;
        self.stats.seals += 1;
        FrameLease { generation, first_record_lsn, frame_len, record_count }
    }

    /// The sealed frame's bytes — what the LOG step hands to the segment
    /// write. Borrowed only for the submission; the lease token, not this
    /// slice, crosses iteration boundaries.
    #[must_use]
    pub fn leased_frame(&self, lease: &FrameLease) -> &[u8] {
        let in_flight = self.in_flight.as_ref().expect("no frame in flight");
        assert_eq!(in_flight.generation, lease.generation, "lease does not match in-flight frame");
        self.bufs[in_flight.buf].sealed_frame()
    }

    /// Return the lease after the covering write completes; the buffer
    /// rejoins the staging rotation.
    pub fn release(&mut self, lease: FrameLease) {
        let in_flight = self.in_flight.take().expect("release with no frame in flight");
        assert_eq!(in_flight.generation, lease.generation, "lease does not match in-flight frame");
        self.bufs[in_flight.buf].reset();
        self.stats.releases += 1;
    }

    /// Synchronous LOG-step choreography for the pre-S05 tiers (tests,
    /// recovery tooling): reserve → seal → write through the rotor. The
    /// caller resolves LSNs via the returned lease, then releases it.
    /// Returns `Ok(None)` on an empty iteration (no frame is emitted).
    /// Frames stamp `covered_lsn = 0` — the synchronous tiers run no
    /// group commit, and 0 attests nothing (conservative — ADR-0031 D6).
    ///
    /// On rotor errors the staged records stay intact: a failed
    /// reservation (`NoSpace`, seal-fsync) leaves staging untouched for
    /// retry-after-maintain; a failed *write* is fail-stop territory for
    /// the cell anyway (§8.4).
    ///
    /// # Panics
    /// If the previous lease is unreleased (see [`seal`](Self::seal)).
    pub fn flush_into<F: SegmentFs>(
        &mut self,
        rotor: &mut SegmentRotor<F>,
        now_ms: u64,
    ) -> Result<Option<FrameLease>, LogError> {
        if self.is_empty() {
            return Ok(None);
        }
        let slot = rotor.begin_frame(self.pending_frame_len(), now_ms)?;
        let lease = self.seal(slot.first_record_lsn(), 0);
        rotor.commit_frame(slot, self.leased_frame(&lease))?;
        Ok(Some(lease))
    }

    #[must_use]
    pub fn stats(&self) -> StagingStats {
        self.stats
    }
}
