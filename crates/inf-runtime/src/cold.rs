//! Hardened cold-read custody (M4-S08) + cold-read shaping (M4-S10):
//! the aligned-buffer pool, the per-file in-flight read pins (§3.3), the
//! completion-value guard that makes buffer/pin lifecycle **structural**,
//! and — since S10 — the bounded intent queues, per-cell device-QD cap,
//! class deficit split, and read coalescing (ADR-0055).
//!
//! The S08 custody model, unchanged in spirit:
//!
//! - **Queued** (S10), the intent holds only its file pin; the future
//!   holds only a gate waiter. An intent whose waiter dies while queued
//!   is skipped at drain (pin released, counted) — a dead command never
//!   spends device bandwidth.
//! - **In flight**, the buffer and file pins belong to the *device read*,
//!   held in the cell-local in-flight table.
//! - **At terminal completion**, [`ColdReads::record_completion`] records
//!   the device read's outcome and links its window onto the ready list;
//!   it visits no member. The turn's delivery service,
//!   [`ColdReads::deliver_ready`], then visits the ready windows' members
//!   round-robin, at most a turn's allowance a turn
//!   (`COLD_DELIVERIES_PER_TURN`, ADR-0181 R1), giving each waiter a
//!   [`ColdDone`] carrying its subrange of the shared window. Every
//!   `ColdDone` drop releases that waiter's file pin; the **last** drop,
//!   once every member was visited, releases the buffer lease — exactly
//!   once across every cancellation interleaving (waiter resumed,
//!   cancelled before completion, cancelled after delivery, or a
//!   fully-cancelled merged read). Singleton and merged reads are one code
//!   path: a singleton is a window with one waiter.
//!
//! Shaping (ADR-0055): [`ColdReads::enqueue`] parks an intent in a
//! bounded per-class FIFO; [`ColdReads::drain`] — once per reactor
//! iteration, the plan's "collect the batch's suspended cold reads"
//! window — admits device reads up to the QD cap under a 3:1
//! foreground:maintain deficit (work-conserving), merging
//! adjacent/overlapping same-file ranges that fit one pool buffer into
//! one device read (never across files — pins and base arithmetic
//! differ; never bridging gaps — unrequested bytes are bandwidth theft).
//! The FIFO head always seeds the next device read, so admission order
//! is FIFO exactly; a single O(queue) scan in arrival order joins
//! mergeable intents to the seed (deterministic; out-of-order chains it
//! misses are bounded conservatism, visible in the ratio).
//!
//! Observables (the plan's five, sources in parentheses):
//! `cold_reads_inflight` ([`ColdReads::inflight_total`]) ·
//! `cold_queue_depth` ([`ColdReads::queue_depth`] + high-water in the
//! counters) · `cold_read_qd_p99` ([`ColdReads::qd_percentile`]) ·
//! `coalesce_ratio` (derived: `issued` vs `enqueued`, with
//! `merged_waiters` closing the identity `enqueued = issued +
//! merged_waiters + cancelled_queued` at quiesce) · `cold_read_p99_us`
//! ([`ColdReads::latency_percentile_us`], **injected time** — `now_us`
//! parameters, sim time in DST; no ambient clocks in cell code, L7).
//! `INFO tiering` renders it since the S26 wiring; enqueue and
//! completion must stamp the **same** injected clock — a zero enqueue
//! stamp degrades the percentile to absolute uptime (the v0.4.0-alpha
//! soak instrument fix, regression-pinned in this module's tests).
//!
//! Pins (§3.3): a tier file with queued or in-flight cold reads is never
//! closed or deleted — compaction/reclaim (S15) and namespace drop (S19)
//! consult [`ColdReads::inflight_on`] and defer the unlink until it
//! drains. Cell-local, no atomics (L1). The pin table lives *here*,
//! beside the custody guard that must decrement it, because the dep DAG
//! forbids an `inf-runtime ↔ inf-store` edge (the ADR-0051 placement
//! argument; recorded as an S08 deviation from the plan's "Where"
//! sketch).
//!
//! Read-promotion stays **off** (plan anti-goal): nothing here promotes
//! a record for having been read — update-driven copy-to-tail (S06) is
//! the only promotion path, and the `READ-PROMOTE` knob stays reserved.

use core::cell::RefCell;
#[allow(clippy::disallowed_types, reason = "container: C")]
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;

use inf_alloc::{AlignedBufId, AlignedPool};
use inf_foundation::limits::DRIVER_OP_BYTES_MAX;
use inf_foundation::{BuildIntHasher, FileOffset, FileOffsetRefused, LogHistogram};

use crate::budget::Issue;
use crate::driver::{IoOp, RawFd, StableBytesMut};
use crate::gate::{GateWait, KeyedGate};
use crate::limits::{
    COLD_DELIVERIES_PER_TURN, COLD_IDENTITY_PAIRS_MAX, COLD_PREPARATION_WORK_PER_TURN,
};
use crate::token::{CompletionToken, MAX_SLOT, TokenClass};

/// Opaque per-cell tier-file identity — the pin-table key. Minted by the
/// layer that owns the address→file mapping (the MANIFEST's successor,
/// S12; harnesses until then); this module never interprets it.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub struct TierFileId(u32);

impl TierFileId {
    #[must_use]
    pub fn new(id: u32) -> TierFileId {
        TierFileId(id)
    }

    #[must_use]
    pub fn as_u32(self) -> u32 {
        self.0
    }
}

/// Cold-read class (ADR-0055 D3): foreground command reads vs
/// maintenance reads (S15 compaction). The drain grants device slots
/// 3:1 under contention, work-conserving when one class is idle — the
/// split that keeps background copy-forward from starving command tails.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum ReadClass {
    Foreground,
    Maintain,
}

impl ReadClass {
    fn index(self) -> usize {
        match self {
            ReadClass::Foreground => 0,
            ReadClass::Maintain => 1,
        }
    }
}

/// A request's logical identity (ADR-0181 R4): the even word of its pair,
/// the key its value is delivered under. Minted only by the host's
/// [`PairIssuer`], beside the [`DeviceId`] the same pair reserves. The two
/// are distinct types, so neither stands where the other is asked for:
///
/// ```compile_fail,E0308
/// use inf_runtime::{DeviceId, LogicalId};
/// fn device_of(logical: LogicalId) -> DeviceId {
///     logical
/// }
/// ```
///
/// Its twin, the same function over one type, compiles:
///
/// ```
/// use inf_runtime::DeviceId;
/// fn device_of(device: DeviceId) -> DeviceId {
///     device
/// }
/// ```
///
/// A value is delivered under its logical id alone, so a device id never
/// completes the cold gate:
///
/// ```compile_fail,E0308
/// use inf_runtime::gate::KeyedGate;
/// use inf_runtime::{ColdDone, DeviceId, LogicalId};
/// fn deliver(gate: &KeyedGate<LogicalId, ColdDone>, device: DeviceId, done: ColdDone) -> bool {
///     gate.complete(device, done)
/// }
/// ```
///
/// Its twin, the logical id, compiles:
///
/// ```
/// use inf_runtime::gate::KeyedGate;
/// use inf_runtime::{ColdDone, LogicalId};
/// fn deliver(gate: &KeyedGate<LogicalId, ColdDone>, logical: LogicalId, done: ColdDone) -> bool {
///     gate.complete(logical, done)
/// }
/// ```
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub struct LogicalId(CompletionToken);

/// A device read's identity (ADR-0181 R4): the odd word of its pair, the
/// token its `TierRead` carries and its receipt returns. A read that serves
/// a cohort carries its seed's; a member's own device id retires unused.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub struct DeviceId(CompletionToken);

impl LogicalId {
    /// The pair this id belongs to: its word index halved.
    fn pair(self) -> u64 {
        word_index(self.0) >> 1
    }
}

impl DeviceId {
    /// The token a `TierRead` carries for this read.
    fn token(self) -> CompletionToken {
        self.0
    }
}

/// The `TierRead` token of word index `index` (`< 2^56`): the low 24 bits in
/// the slot, the rest in the generation, as every cold token is packed.
fn tier_read_token(index: u64) -> CompletionToken {
    debug_assert!(index < 2 * COLD_IDENTITY_PAIRS_MAX, "a word index below 2^56");
    // `index < 2^56`, so `index >> 24 < 2^32`: both narrowings are lossless.
    CompletionToken::new(
        TokenClass::TierRead,
        (index & u64::from(MAX_SLOT)) as u32,
        (index >> 24) as u32,
    )
}

/// A cold token's word index: the inverse of [`tier_read_token`].
fn word_index(token: CompletionToken) -> u64 {
    (u64::from(token.generation()) << 24) | u64::from(token.slot())
}

/// The host's identity issuer (ADR-0181 R4): pair `n` is the logical id `2n`
/// and the device id `2n + 1`, `n < COLD_IDENTITY_PAIRS_MAX`, each minted once
/// in the host's lifetime and never returned. A host takes its issuer when it
/// is built: [`PairIssuer::new`] starts at pair 0, the server's;
/// [`PairIssuer::starting_at`] places a harness inside the space, so its
/// exhaustion falls within a run.
#[derive(Debug)]
pub struct PairIssuer {
    /// The next pair to mint; `COLD_IDENTITY_PAIRS_MAX` once every pair is spent.
    next_pair: u64,
}

/// A pair the issuer can mint, read before the request's reservations and
/// spent only on its success arm: a refusal after it mints nothing.
struct PairPermit {
    pair: u64,
}

impl PairIssuer {
    /// The issuer at pair 0.
    #[must_use]
    pub const fn new() -> PairIssuer {
        PairIssuer { next_pair: 0 }
    }

    /// The issuer at pair `pair`, or `None` at or past
    /// [`COLD_IDENTITY_PAIRS_MAX`].
    #[must_use]
    pub const fn starting_at(pair: u64) -> Option<PairIssuer> {
        if pair < COLD_IDENTITY_PAIRS_MAX { Some(PairIssuer { next_pair: pair }) } else { None }
    }

    /// The pairs spent or skipped: the issuer's position.
    #[must_use]
    pub const fn pairs_used(&self) -> u64 {
        self.next_pair
    }

    /// The next pair, when one is left; nothing is spent.
    fn reserve(&self) -> Option<PairPermit> {
        (self.next_pair < COLD_IDENTITY_PAIRS_MAX).then_some(PairPermit { pair: self.next_pair })
    }

    /// Spends the reserved pair: its logical and device ids.
    fn mint(&mut self, permit: PairPermit) -> (LogicalId, DeviceId) {
        debug_assert_eq!(permit.pair, self.next_pair, "a permit is spent at the position it read");
        self.next_pair = permit.pair + 1;
        let logical = LogicalId(tier_read_token(2 * permit.pair));
        let device = DeviceId(tier_read_token(2 * permit.pair + 1));
        (logical, device)
    }

    /// A cancelled request's pair retires: it is never minted again.
    fn retire(&mut self, logical: LogicalId) {
        #[cfg(not(inf_canary_cold_identity_recycled))]
        debug_assert!(logical.pair() < self.next_pair, "a retired pair was minted");
        // Canary: the cancelled request's pair returns to the issuer, which
        // mints it again.
        #[cfg(inf_canary_cold_identity_recycled)]
        {
            self.next_pair = self.next_pair.min(logical.pair());
        }
    }
}

impl Default for PairIssuer {
    fn default() -> PairIssuer {
        PairIssuer::new()
    }
}

/// One host turn's cold-read allowances (ADR-0181 R1–R3): the delivery
/// budget and the preparation budget, built together by
/// [`ColdTurn::begin`], their one constructor. The native reactor begins one
/// per iteration and lends both through `LoopCx`; a standalone host begins
/// one per turn it declares, its pumps sharing it. Neither budget is `Copy`
/// or `Clone`, and no caller can renew one, so every call of a turn spends
/// from the same allowance.
#[derive(Debug)]
pub struct ColdTurn {
    delivery: ColdDeliveryBudget,
    preparation: ColdPreparationBudget,
}

impl ColdTurn {
    /// A turn's full allowances: [`COLD_DELIVERIES_PER_TURN`] member visits
    /// and [`COLD_PREPARATION_WORK_PER_TURN`] preparation units.
    #[must_use]
    pub fn begin() -> ColdTurn {
        ColdTurn {
            delivery: ColdDeliveryBudget { remaining: COLD_DELIVERIES_PER_TURN, spent: 0 },
            preparation: ColdPreparationBudget {
                remaining: COLD_PREPARATION_WORK_PER_TURN,
                spent: 0,
            },
        }
    }

    /// The turn's delivery allowance, lent, never copied.
    pub fn delivery(&mut self) -> &mut ColdDeliveryBudget {
        &mut self.delivery
    }

    /// The turn's preparation allowance, lent, never copied.
    pub fn preparation(&mut self) -> &mut ColdPreparationBudget {
        &mut self.preparation
    }
}

/// One turn's member visits (ADR-0181 R1): [`COLD_DELIVERIES_PER_TURN`] at
/// [`ColdTurn::begin`], one spent before each member a delivery step
/// visits. Its fields are private, so no struct literal outside this module
/// builds one:
///
/// ```compile_fail,E0451
/// use inf_runtime::ColdDeliveryBudget;
/// let _ = ColdDeliveryBudget { remaining: 64, spent: 0 };
/// ```
///
/// Its twin, the one constructor, compiles:
///
/// ```
/// use inf_runtime::{ColdDeliveryBudget, ColdTurn};
/// let mut turn = ColdTurn::begin();
/// let budget: &mut ColdDeliveryBudget = turn.delivery();
/// assert_eq!(budget.remaining(), 64);
/// ```
#[derive(Debug)]
pub struct ColdDeliveryBudget {
    remaining: u32,
    /// Units spent this turn: what the turn's group is charged.
    spent: u32,
}

impl ColdDeliveryBudget {
    /// Spends one visit, before the member it visits; `false` once the
    /// turn's are spent, with nothing spent.
    fn take(&mut self) -> bool {
        let Some(left) = self.remaining.checked_sub(1) else { return false };
        self.remaining = left;
        self.spent += 1;
        true
    }

    /// Member visits left this turn.
    #[must_use]
    pub fn remaining(&self) -> u32 {
        self.remaining
    }

    /// Member visits spent this turn.
    #[must_use]
    pub fn spent(&self) -> u32 {
        self.spent
    }
}

/// One turn's preparation work (ADR-0181 R3):
/// [`COLD_PREPARATION_WORK_PER_TURN`] units at [`ColdTurn::begin`], beside
/// the delivery allowance and apart from it. Its fields are private:
///
/// ```compile_fail,E0451
/// use inf_runtime::ColdPreparationBudget;
/// let _ = ColdPreparationBudget { remaining: 64, spent: 0 };
/// ```
///
/// Its twin compiles:
///
/// ```
/// use inf_runtime::{ColdPreparationBudget, ColdTurn};
/// let mut turn = ColdTurn::begin();
/// let budget: &mut ColdPreparationBudget = turn.preparation();
/// assert_eq!(budget.remaining(), 64);
/// ```
#[derive(Debug)]
pub struct ColdPreparationBudget {
    remaining: u32,
    spent: u32,
}

impl ColdPreparationBudget {
    /// Work units left this turn.
    #[must_use]
    pub fn remaining(&self) -> u32 {
        self.remaining
    }

    /// Work units spent this turn.
    #[must_use]
    pub fn spent(&self) -> u32 {
        self.spent
    }
}

/// Shaping knobs (ADR-0055; `COLD-READ-QD` and friends surface via S19).
#[derive(Copy, Clone, Debug)]
pub struct ColdReadConfig {
    /// Device reads in flight per cell (merged reads count once — the
    /// cap protects the device, and the device sees SQEs). Default 64,
    /// S22-calibrated. The pool is the physical ceiling underneath.
    pub qd_cap: usize,
    /// Queued intents per class; a full FIFO refuses typed
    /// (`QueueFull`) — never an unbounded queue. Default `4 × qd_cap`.
    pub overflow_cap: usize,
    /// Deficit grants per refill under contention (foreground).
    pub grants_foreground: u32,
    /// Deficit grants per refill under contention (maintain).
    pub grants_maintain: u32,
    /// Coalescing on/off — the §2 cut-line flag (M0-S14 rule: a losing
    /// A/B demotes this to default-off by ADR amendment, nothing else).
    pub merge: bool,
}

impl Default for ColdReadConfig {
    fn default() -> ColdReadConfig {
        ColdReadConfig {
            qd_cap: 64,
            overflow_cap: 256,
            grants_foreground: 3,
            grants_maintain: 1,
            merge: true,
        }
    }
}

/// Always-on cold-read path counters (S08, extended by S10). At quiesce
/// the admission identity holds exactly:
/// `enqueued = issued + merged_waiters + cancelled_queued` — the DST
/// oracle checks it, and `coalesce_ratio` derives from it at scrape.
#[derive(Copy, Clone, Default, Debug, PartialEq, Eq)]
pub struct ColdReadCounters {
    /// Logical read intents accepted by [`ColdReads::enqueue`] (chunked
    /// staging counts each chunk — the ratio is about device trips).
    pub enqueued: u64,
    /// Device reads issued (post-merge — one per SQE).
    pub issued: u64,
    /// Waiters that rode a shared device read beyond its seed (the
    /// coalescing savings in device-trip units).
    pub merged_waiters: u64,
    pub completed: u64,
    pub errored: u64,
    /// `ColdDone` values delivered to live waiters.
    pub delivered: u64,
    /// Terminal completions whose waiter was cancelled mid-flight —
    /// custody released here; the designed path, not a leak.
    pub unclaimed: u64,
    /// Intents dropped at drain because their waiter died while queued.
    pub cancelled_queued: u64,
    /// Drain stalls with cap headroom but no free pool buffer (sizing
    /// pressure, distinct from the policy cap — never an error).
    pub pool_dry: u64,
    /// Enqueue refusals on a full class FIFO (typed backpressure).
    pub queue_full: u64,
    /// High-water mark of queued intents across both classes (the
    /// overflow-depth tripwire input).
    pub queue_depth_high_water: u32,
    /// `Maintain` reads the device budget deferred at drain (M4.5-S36,
    /// ADR-0088 D5) — per drain, not per intent; the intents stay queued.
    pub maintain_deferred: u64,
    /// Member visits the delivery service spent: one a member, live,
    /// cancelled or orphaned (ADR-0181 R1; `cold_delivery_work`).
    pub delivery_work: u64,
    /// The most member visits one turn spent, at most
    /// `COLD_DELIVERIES_PER_TURN` (`cold_delivery_work_turn_max`).
    pub delivery_work_turn_max: u32,
    /// Receipts whose word named no Issued window — never minted, already
    /// recorded, or their window gone: refused and counted, never asserted,
    /// and 0 on a healthy run (ADR-0181 R1; `cold_receipt_foreign`).
    pub receipts_foreign: u64,
    /// Ready-list links that named no ready window: a defect, 0 on every
    /// run, the walk stopped where it met one (`cold_link_stale`).
    pub links_stale: u64,
}

impl ColdReadCounters {
    /// ADR-0055 D5 `coalesce_ratio`, in milli-units: `1 −
    /// device_reads/logical_reads` — 0 with nothing merged, approaching
    /// the merged fraction under heavy coalescing. Derived at scrape
    /// from the two raw counters (both stay exposed); 0 on an idle
    /// engine. (The v0.4.0-alpha soak rendered the *inverted*
    /// `enqueued/issued`, which reads 1000 at exactly zero coalescing —
    /// the defect this method replaces.)
    #[must_use]
    pub fn coalesce_ratio_milli(&self) -> u64 {
        let saved = self.enqueued.saturating_sub(self.issued);
        saved.saturating_mul(1000) / self.enqueued.max(1)
    }
}

/// Why [`ColdReads::enqueue`] refused: temporary backpressure, or a
/// request no device read can serve. `QueueFull` counts `queue_full` and
/// changes nothing else; `Unrepresentable` changes nothing (ADR-0167 D3).
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum ColdRefused {
    /// The class FIFO is at `overflow_cap`: temporary. The command layer
    /// surfaces backpressure (retry next iteration / typed error upward).
    /// Pool dryness is no longer a refusal: it stalls the drain and counts
    /// `pool_dry` (S10 moved admission off the enqueue path).
    QueueFull,
    /// The request cannot become a device read: permanent, so the same
    /// request is refused again (ADR-0167 D3). Checked before the queue
    /// bound; the caller answers a read failure, never backpressure.
    Unrepresentable(UnrepresentableRead),
    /// The host has issued every one of its [`COLD_IDENTITY_PAIRS_MAX`]
    /// identity pairs (ADR-0181 R4): permanent for this host, so every
    /// later request is refused too. Checked after the request's own faults
    /// and before the queue bound, with nothing changed; admitted reads still
    /// issue and deliver. The caller answers `IdentityExhausted(ColdRead)`
    /// (ADR-0172 D1), never backpressure.
    IdentityExhausted,
}

/// Which check refused an [`ColdRefused::Unrepresentable`] request, in the
/// order `enqueue` runs them (ADR-0167 D3): a request with two faults
/// answers the first.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum UnrepresentableRead {
    /// The position is above `FILE_OFFSET_BYTES_MAX` (ADR-0167 D1).
    OffsetAboveMax(FileOffsetRefused),
    /// A zero-byte window.
    EmptyWindow,
    /// A window wider than one pool buffer, the read-window maximum.
    WindowAboveMax {
        /// The requested window, before any narrowing.
        window_bytes: usize,
        /// The pool buffer size.
        window_bytes_max: usize,
    },
}

/// A queued read intent — plain data; the §3.3 pin is taken at enqueue.
struct Intent {
    logical: LogicalId,
    /// The pair's device id, the read's token when this intent seeds it.
    device: DeviceId,
    fd: RawFd,
    file: TierFileId,
    offset: FileOffset,
    len: u32,
    enqueued_us: u64,
}

/// One waiter's subrange of an in-flight device read.
struct Waiter {
    logical: LogicalId,
    /// Offset into the merged window.
    skip: u32,
    len: u32,
    enqueued_us: u64,
}

/// Where a window stands between its issue and its last member's visit
/// (ADR-0181 R5). A window whose every member was visited leaves the
/// table; its shared custody alone remains, until its last value drops.
#[derive(Copy, Clone, Debug)]
enum WindowPhase {
    /// The device holds the read.
    Issued,
    /// The receipt is recorded: the outcome every member carries, and its
    /// injected time.
    Ready { outcome: Result<(), i32>, receipt_us: u64 },
}

/// One device read's window (singleton or merged — same shape): its
/// members, its phase and its delivery cursor.
struct DeviceRead {
    buf: AlignedBufId,
    file: TierFileId,
    len: u32,
    waiters: Vec<Waiter>,
    phase: WindowPhase,
    /// The next member to visit; `waiters.len()` once every one was.
    cursor: u32,
    /// The next window on the ready list, while this one is on it.
    ready_next: Option<CompletionToken>,
}

impl DeviceRead {
    /// A window the device holds: Issued, its cursor at its first member.
    fn issued(buf: AlignedBufId, file: TierFileId, len: u32, waiters: Vec<Waiter>) -> DeviceRead {
        DeviceRead {
            buf,
            file,
            len,
            waiters,
            phase: WindowPhase::Issued,
            cursor: 0,
            ready_next: None,
        }
    }
}

/// A terminally-completed window shared by its waiters' `ColdDone`s;
/// the buffer releases when `remaining` hits zero (last drop).
struct SharedWindow {
    buf: AlignedBufId,
    remaining: u32,
}

#[allow(clippy::disallowed_types, reason = "container: C")]
struct ColdState {
    pool: AlignedPool,
    config: ColdReadConfig,
    /// Bounded per-class FIFOs (index = [`ReadClass::index`]).
    pending: [VecDeque<Intent>; 2],
    /// Deficit grants remaining per class (refilled together when both
    /// are spent under contention).
    grants: [u32; 2],
    /// Every window from its issue until its last member is visited,
    /// keyed by its device token: Issued, then Ready.
    inflight: HashMap<CompletionToken, DeviceRead, BuildIntHasher>,
    /// Every window from its receipt until its last value drops.
    windows: HashMap<CompletionToken, SharedWindow, BuildIntHasher>,
    /// Issued windows: the device QD the cap bounds (`cold_reads_inflight`).
    device_reads: usize,
    /// The ready list's ends: windows with members left to visit, linked
    /// through their `ready_next` in receipt order, a visited window
    /// re-linked at the tail.
    ready_head: Option<CompletionToken>,
    ready_tail: Option<CompletionToken>,
    /// Members of ready windows not yet visited, and those windows
    /// (`cold_delivery_pending_members`, `cold_delivery_pending_windows`).
    pending_members: u64,
    pending_windows: u64,
    /// Receipt-to-delivery delay of delivered values, injected µs
    /// (`cold_delivery_delay_p99_us`).
    delay_hist: LogHistogram,
    /// Per-file pin count over queued + in-flight + undropped `ColdDone`
    /// custody (§3.3). Entries exist only while nonzero.
    pins: HashMap<TierFileId, u32, BuildIntHasher>,
    counters: ColdReadCounters,
    /// Device-QD samples at each issue (`cold_read_qd_p99`).
    qd_hist: LogHistogram,
    /// Enqueue→delivery latency of delivered reads, injected-µs
    /// (`cold_read_p99_us`).
    latency_hist: LogHistogram,
    /// Drained waiter lists recycled across device reads (bounded by the
    /// in-flight cap; no steady-state allocation on the read path).
    spare_waiters: Vec<Vec<Waiter>>,
    /// Merge scratch recycled across drains (bounded by `overflow_cap`).
    merge_scratch: Vec<Intent>,
    /// The host's identity pairs (ADR-0181 R4).
    issuer: PairIssuer,
    /// `enqueue`s refused `IdentityExhausted` (cell, cumulative): the
    /// refusal's own count, apart from the admission counters it leaves as
    /// they were.
    identity_refusals: u64,
}

impl ColdState {
    #[allow(clippy::disallowed_types, reason = "container: C")]
    fn unpin(pins: &mut HashMap<TierFileId, u32, BuildIntHasher>, file: TierFileId) {
        let count = pins.get_mut(&file).expect("pinned file has a pin entry");
        *count -= 1;
        if *count == 0 {
            pins.remove(&file);
        }
    }
}

/// The cold-read boundary (ADR-0167 D3): a request becomes a typed
/// `(position, window)` or is refused, checking the position, then an empty
/// window, then one wider than a pool buffer. The window checks read the
/// `usize` before it narrows, so `1 << 32` is refused as too wide, never
/// read as an empty window.
fn parse_window(
    offset_bytes: u64,
    window_bytes: usize,
    buf_size: usize,
) -> Result<(FileOffset, u32), UnrepresentableRead> {
    let offset = FileOffset::new(offset_bytes).map_err(UnrepresentableRead::OffsetAboveMax)?;
    if window_bytes == 0 {
        return Err(UnrepresentableRead::EmptyWindow);
    }
    let too_wide = UnrepresentableRead::WindowAboveMax { window_bytes, window_bytes_max: buf_size };
    if window_bytes > buf_size {
        return Err(too_wide);
    }
    // `window_bytes ≤ buf_size ≤ DRIVER_OP_BYTES_MAX` (`with_config`), so
    // this narrowing never refuses; were it to, the fault is the same.
    let len = u32::try_from(window_bytes).map_err(|_| too_wide)?;
    Ok((offset, len))
}

/// What [`ColdReads::record_completion`] made of a receipt.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
#[must_use]
pub enum ColdReceipt {
    /// The window it named was Issued: now Ready, linked for delivery.
    Recorded,
    /// It named no Issued window: counted, and nothing changed.
    Foreign,
}

/// Whether ready windows remain after a delivery call.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum ColdDeliveryState {
    /// No window has a member left to visit.
    Idle,
    /// Members wait for a later step: the turn's allowance ran out.
    Ready,
}

/// One delivery call's work (ADR-0181 R1): the members it visited,
/// [`examined`](Self::examined), split into values handed to live waiters
/// and members whose waiter was gone. It never calls an orphan a read.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct ColdDeliveryProgress {
    /// Values delivered to live waiters.
    pub delivered: u32,
    /// Members whose waiter was gone: each value dropped at its visit.
    pub unclaimed: u32,
    /// Whether ready windows remain.
    pub state: ColdDeliveryState,
}

impl ColdDeliveryProgress {
    /// Members visited: `delivered + unclaimed`.
    #[must_use]
    pub fn examined(&self) -> u32 {
        self.delivered + self.unclaimed
    }
}

/// One member's visit, built under the state borrow and delivered after it.
struct DeliveryStep {
    logical: LogicalId,
    done: ColdDone,
    enqueued_us: u64,
    receipt_us: u64,
}

/// Leak report from [`ColdReads::reconcile`].
#[derive(Debug, PartialEq, Eq)]
pub struct ColdLeak {
    pub leased: usize,
    pub inflight: usize,
    pub pinned_files: usize,
    pub queued: usize,
    pub windows: usize,
}

/// The future a suspended cold read parks on, keyed by its request's
/// [`LogicalId`]; resolves to the custody-carrying [`ColdDone`].
pub type ColdWait = GateWait<LogicalId, ColdDone>;

/// One cell's cold-read path state: pool, pins, queues, in-flight table,
/// and the completion gate. Cheap-clone shared handle (cell-local `Rc`,
/// L1) — the plane's completion dispatch and every reading future share
/// it.
pub struct ColdReads {
    state: Rc<RefCell<ColdState>>,
    gate: KeyedGate<LogicalId, ColdDone>,
}

impl ColdReads {
    /// Takes ownership of the registered aligned pool (register it with
    /// the driver first — `BackendDriver::register_tier_pool`) under the
    /// default shaping config.
    #[must_use]
    pub fn new(pool: AlignedPool) -> ColdReads {
        ColdReads::with_config(pool, ColdReadConfig::default())
    }

    /// [`new`](Self::new) with explicit shaping knobs (ADR-0055).
    ///
    /// # Panics
    /// Panics on a zero cap, zero queue bound, or zero grant — a
    /// zero-limit config is a misconfiguration, not a throttle — and on a
    /// pool buffer wider than [`DRIVER_OP_BYTES_MAX`]: the buffer is the
    /// read-window maximum, and a device read's length is a `u32`
    /// (ADR-0167 D1).
    #[must_use]
    #[allow(clippy::disallowed_types, reason = "container: C")]
    pub fn with_config(pool: AlignedPool, config: ColdReadConfig) -> ColdReads {
        assert!(config.qd_cap > 0, "qd_cap must admit at least one device read");
        assert!(config.overflow_cap > 0, "overflow_cap must queue at least one intent");
        assert!(config.grants_foreground > 0, "foreground grants must be nonzero");
        assert!(config.grants_maintain > 0, "maintain grants must be nonzero");
        assert!(
            pool.buf_size() as u64 <= DRIVER_OP_BYTES_MAX,
            "cold-read pool buffer exceeds a driver op's byte range"
        );
        let grants = [config.grants_foreground, config.grants_maintain];
        ColdReads {
            state: Rc::new(RefCell::new(ColdState {
                pool,
                config,
                pending: [VecDeque::new(), VecDeque::new()],
                grants,
                inflight: HashMap::default(),
                windows: HashMap::default(),
                device_reads: 0,
                ready_head: None,
                ready_tail: None,
                pending_members: 0,
                pending_windows: 0,
                delay_hist: LogHistogram::new(),
                pins: HashMap::default(),
                counters: ColdReadCounters::default(),
                qd_hist: LogHistogram::new(),
                latency_hist: LogHistogram::new(),
                spare_waiters: Vec::new(),
                merge_scratch: Vec::new(),
                issuer: PairIssuer::new(),
                identity_refusals: 0,
            })),
            gate: KeyedGate::new(),
        }
    }

    /// [`with_config`](Self::with_config) with the host's identity issuer at
    /// `issuer`'s position instead of pair 0: a harness places the space's
    /// exhaustion inside its run (ADR-0181 R4).
    ///
    /// # Panics
    /// As [`with_config`](Self::with_config).
    #[must_use]
    pub fn with_issuer(pool: AlignedPool, config: ColdReadConfig, issuer: PairIssuer) -> ColdReads {
        let cold = ColdReads::with_config(pool, config);
        cold.state.borrow_mut().issuer = issuer;
        cold
    }

    /// Pool buffer size — the read-window ceiling per device read
    /// (records larger than this stage through multiple intents —
    /// chunked staging; merged windows are capped here too).
    #[must_use]
    pub fn buf_size(&self) -> usize {
        self.state.borrow().pool.buf_size()
    }

    /// Attributed bytes of the aligned cold-read pool (L5 — the
    /// `cold_pool_bytes` gauge): the whole reservation, leased or not.
    #[must_use]
    pub fn pool_reserved_bytes(&self) -> u64 {
        self.state.borrow().pool.reserved_bytes()
    }

    /// Parks a read intent in its class FIFO and returns the waiter.
    /// The file pin is taken now; the buffer is leased at drain time.
    /// **Nothing but plain data crosses the suspension** — custody sits
    /// in the queue/in-flight tables until the terminal completion is
    /// recorded ([`record_completion`](Self::record_completion)) and the
    /// turn's delivery ([`deliver_ready`](Self::deliver_ready)) visits it.
    ///
    /// `now_us` is injected time (L7): the reactor clock in production,
    /// sim time in DST — it feeds the `cold_read_p99_us` histogram.
    ///
    /// # Errors
    /// [`ColdRefused::Unrepresentable`] when the request cannot become a
    /// device read — permanent, answered before the queue bound and with no
    /// state changed (ADR-0167 D3). [`ColdRefused::IdentityExhausted`] when
    /// the host's identity pairs are spent — permanent, answered next, before
    /// the queue bound; it counts its own refusal and changes nothing else
    /// (ADR-0181 R4). [`ColdRefused::QueueFull`] when the class FIFO is at
    /// its bound — backpressure the caller shapes; it counts `queue_full`
    /// and changes nothing else.
    pub fn enqueue(
        &self,
        fd: RawFd,
        file: TierFileId,
        offset: u64,
        len: usize,
        class: ReadClass,
        now_us: u64,
    ) -> Result<ColdWait, ColdRefused> {
        let mut state = self.state.borrow_mut();
        let (offset, len) = parse_window(offset, len, state.pool.buf_size())
            .map_err(ColdRefused::Unrepresentable)?;
        // Both permanent refusals precede the temporary one, and the pair is
        // minted on the success arm alone (ADR-0181 R4).
        let Some(permit) = state.issuer.reserve() else {
            state.identity_refusals += 1;
            return Err(ColdRefused::IdentityExhausted);
        };
        if state.pending[class.index()].len() >= state.config.overflow_cap {
            state.counters.queue_full += 1;
            return Err(ColdRefused::QueueFull);
        }
        let (logical, device) = state.issuer.mint(permit);
        *state.pins.entry(file).or_insert(0) += 1;
        state.pending[class.index()].push_back(Intent {
            logical,
            device,
            fd,
            file,
            offset,
            len,
            enqueued_us: now_us,
        });
        state.counters.enqueued += 1;
        let depth = (state.pending[0].len() + state.pending[1].len()) as u32;
        state.counters.queue_depth_high_water = state.counters.queue_depth_high_water.max(depth);
        drop(state);
        Ok(self.gate.waiter(logical))
    }

    /// Admits queued intents to the device: merges (when enabled),
    /// leases buffers, and emits one `IoOp` per device read up to the
    /// QD cap under the class deficit. Called once per reactor iteration
    /// after the EXECUTE batch (and harmlessly more often — it is
    /// idempotent when nothing is admissible). Returns device reads
    /// issued.
    pub fn drain(&self, push: impl FnMut(IoOp)) -> u32 {
        self.drain_budgeted(|_, _| Issue::Now, |_, _, _| {}, push)
    }

    /// [`drain`](Self::drain) under a device budget (M4.5-S36, ADR-0088
    /// D5; the answer is ADR-0178 D1's [`Issue`]): before a class's read
    /// is built, `admit(class, bytes)` is asked with the pool buffer size
    /// (the window's bound); a `Maintain` read answered `NotThisSlice`
    /// stays queued and the drain continues with the foreground only.
    /// `Foreground` is charged, never refused (the caller's `admit`
    /// answers `Now` for it by contract; a refusal ends the slice,
    /// bounded either way).
    /// Every `admit` is one device op beside its bytes. After the op is
    /// built, `refund(class, unused_bytes, unused_ops)` returns the bound
    /// minus the issued window and no op; a pool-dry stall issues nothing
    /// and returns the whole bound and the op (F-L11-04). A deferred
    /// maintain keeps its contention grant — "not this slice" is not a
    /// turn taken.
    pub fn drain_budgeted(
        &self,
        mut admit: impl FnMut(ReadClass, u64) -> Issue,
        mut refund: impl FnMut(ReadClass, u64, u64),
        mut push: impl FnMut(IoOp),
    ) -> u32 {
        let mut issued = 0u32;
        let mut maintain_allowed = true;
        loop {
            let mut state_ref = self.state.borrow_mut();
            let state = &mut *state_ref;
            if state.device_reads >= state.config.qd_cap {
                break;
            }
            Self::purge_stale_heads(state, &self.gate);
            if state.pending[0].is_empty() && (state.pending[1].is_empty() || !maintain_allowed) {
                break;
            }
            let (class, grant_spent) = Self::pick_class(state, maintain_allowed);
            let class_enum = if class == 0 { ReadClass::Foreground } else { ReadClass::Maintain };
            let bound = state.pool.buf_size() as u64;
            match admit(class_enum, bound) {
                Issue::Now => {}
                Issue::NotThisSlice if class_enum == ReadClass::Foreground => {
                    // Contract breach (ADR-0178 D1: the foreground is never
                    // refused). Nothing changes within this slice, so it
                    // ends here with the intent queued for the next one —
                    // a `continue` would spin the cell.
                    break;
                }
                Issue::NotThisSlice => {
                    state.counters.maintain_deferred += 1;
                    maintain_allowed = false;
                    if grant_spent {
                        state.grants[1] += 1;
                    }
                    continue;
                }
            }
            let Some(buf) = state.pool.try_lease() else {
                state.counters.pool_dry += 1;
                refund(class_enum, bound, 1);
                break;
            };
            let op = Self::admit(state, &self.gate, class, buf);
            let IoOp::TierRead { buf: ref window, .. } = op else {
                unreachable!("admit builds reads")
            };
            refund(class_enum, bound.saturating_sub(u64::from(window.len())), 0);
            drop(state_ref);
            push(op);
            issued += 1;
        }
        issued
    }

    /// Drops cancelled-while-queued heads of both FIFOs (pins released,
    /// counted). Mid-queue stale intents are filtered by the merge scan
    /// or when they surface as heads — lazy, bounded, deterministic.
    fn purge_stale_heads(state: &mut ColdState, gate: &KeyedGate<LogicalId, ColdDone>) {
        let ColdState { pending, pins, counters, issuer, .. } = state;
        for queue in pending.iter_mut() {
            while let Some(head) = queue.front() {
                if gate.has_waiter(&head.logical) {
                    break;
                }
                let intent = queue.pop_front().expect("front observed");
                ColdState::unpin(pins, intent.file);
                counters.cancelled_queued += 1;
                issuer.retire(intent.logical);
            }
        }
    }

    /// Deficit pick over non-empty classes: work-conserving when one
    /// class is idle (its grants are forfeit, not banked); 3:1 metering
    /// only under contention (ADR-0055 D3). The flag says a contention
    /// grant was spent on the pick (the caller gives it back on a
    /// deferral).
    fn pick_class(state: &mut ColdState, maintain_allowed: bool) -> (usize, bool) {
        let foreground = !state.pending[0].is_empty();
        let maintain = !state.pending[1].is_empty() && maintain_allowed;
        debug_assert!(foreground || maintain, "caller checked non-empty");
        if !maintain {
            return (0, false);
        }
        if !foreground {
            return (1, false);
        }
        if state.grants[0] == 0 && state.grants[1] == 0 {
            state.grants = [state.config.grants_foreground, state.config.grants_maintain];
        }
        if state.grants[0] > 0 {
            state.grants[0] -= 1;
            (0, true)
        } else {
            state.grants[1] -= 1;
            (1, true)
        }
    }

    /// Pops the class FIFO's live head as the seed, joins mergeable
    /// intents (same file, adjacent/overlapping, union fits one pool
    /// buffer), and builds the device read ([`issue`](Self::issue)). FIFO
    /// order of everything left behind is preserved (one rotate pass).
    fn admit(
        state: &mut ColdState,
        gate: &KeyedGate<LogicalId, ColdDone>,
        class: usize,
        buf: AlignedBufId,
    ) -> IoOp {
        let buf_size = state.pool.buf_size();
        let merge = state.config.merge;
        let device = {
            let ColdState { pending, pins, counters, merge_scratch, issuer, .. } = state;
            let queue = &mut pending[class];
            let seed = queue.pop_front().expect("picked class has a live head");
            debug_assert!(gate.has_waiter(&seed.logical), "heads were purged");
            // The read carries its seed's reserved device id (ADR-0181 R4).
            let device = seed.device;
            // Span ends come from `bytes_after`: at most `i64::MAX`, so the
            // union arithmetic below cannot wrap (ADR-0167 D1).
            let mut lo = seed.offset.bytes();
            let mut hi = seed.offset.bytes_after(seed.len);
            let (seed_fd, seed_file) = (seed.fd, seed.file);
            debug_assert!(merge_scratch.is_empty(), "scratch drained after every admit");
            merge_scratch.push(seed);
            if merge {
                // One pass in arrival order: pop each intent and either
                // join it to the seed's window or rotate it to the back —
                // after exactly `len` pops the kept intents are in their
                // original FIFO order.
                for _ in 0..queue.len() {
                    let intent = queue.pop_front().expect("counted");
                    if !gate.has_waiter(&intent.logical) {
                        ColdState::unpin(pins, intent.file);
                        counters.cancelled_queued += 1;
                        issuer.retire(intent.logical);
                        continue;
                    }
                    let start = intent.offset.bytes();
                    let end = intent.offset.bytes_after(intent.len);
                    // One device read has one fd: an intent on the same
                    // file id through a different descriptor never joins
                    // (blob extents open one reader per fetch under one
                    // synthetic id — review remediation of 2026-09-01,
                    // N5: the former `debug_assert_eq!(intent.fd,
                    // seed_fd)` was a cell panic under two concurrent
                    // reads of one extent).
                    let joins = intent.file == seed_file
                        && intent.fd == seed_fd
                        && start <= hi
                        && end >= lo;
                    let new_lo = lo.min(start);
                    let new_hi = hi.max(end);
                    if joins && (new_hi - new_lo) as usize <= buf_size {
                        lo = new_lo;
                        hi = new_hi;
                        merge_scratch.push(intent);
                    } else {
                        queue.push_back(intent);
                    }
                }
            }
            device
        };
        Self::issue(state, device, buf)
    }

    /// Builds the merged window's device read under `device`, the seed's
    /// reserved id: each member's record written once from the union, the
    /// window Issued in the table, the device QD counted.
    fn issue(state: &mut ColdState, device: DeviceId, buf: AlignedBufId) -> IoOp {
        let buf_size = state.pool.buf_size();
        let ColdState { pool, inflight, counters, spare_waiters, merge_scratch, .. } = state;
        let (fd, file) = (merge_scratch[0].fd, merge_scratch[0].file);
        // The lowest intent's position is already a `FileOffset`: the merged
        // read never re-parses a position (ADR-0167 D3).
        let lo = merge_scratch.iter().map(|intent| intent.offset).min().expect("seed present");
        let hi = merge_scratch
            .iter()
            .map(|intent| intent.offset.bytes_after(intent.len))
            .max()
            .expect("seed present");
        // Lossless below: the union is at most `buf_size`, which
        // `with_config` bounds by `DRIVER_OP_BYTES_MAX` (a `u32`).
        let window_len = (hi - lo.bytes()) as usize;
        debug_assert!(window_len <= buf_size, "merge respected the buffer cap");
        let mut waiters = spare_waiters.pop().unwrap_or_default();
        debug_assert!(waiters.is_empty(), "spare lists come back drained");
        for intent in merge_scratch.drain(..) {
            waiters.push(Waiter {
                logical: intent.logical,
                skip: (intent.offset.bytes() - lo.bytes()) as u32,
                len: intent.len,
                enqueued_us: intent.enqueued_us,
            });
        }
        counters.issued += 1;
        counters.merged_waiters += waiters.len() as u64 - 1;
        let target = &mut pool.bytes_mut(buf)[..window_len];
        // SAFETY: the pool buffer's address is stable for the pool's
        // lifetime (inf-alloc invariant) and its lease is held by the
        // window — released only when its last value drops, after the
        // receipt and every member's visit — so nothing reads, writes, or
        // re-leases these bytes while the driver owns them, regardless of
        // what any issuing future does (including being cancelled).
        let stable = unsafe { StableBytesMut::new(target) };
        inflight.insert(device.token(), DeviceRead::issued(buf, file, window_len as u32, waiters));
        state.device_reads += 1;
        state.qd_hist.record(state.device_reads as u64);
        IoOp::TierRead { fd, offset: lo, buf: stable, token: device.token() }
    }

    /// Records a `TierRead` receipt (ADR-0181 R1): `result` is the outcome
    /// the completion dispatch decided, `Ok(())` for a full window or
    /// `Err(errno)`. The window the device token names, while Issued,
    /// becomes Ready with `result` and `now_us` (injected time, L7), returns
    /// its device-QD position and is linked once at the ready list's tail.
    /// No member is visited, no task woken, and no buffer or pin returns:
    /// the turn's [`deliver_ready`](Self::deliver_ready) does that.
    ///
    /// A receipt is what the host receives from the driver, so it is
    /// checked here: a word that names no Issued window — never minted,
    /// already recorded, or its window gone — is [`ColdReceipt::Foreign`],
    /// counted (`receipts_foreign`) and changing nothing, never a panic.
    pub fn record_completion(
        &self,
        token: CompletionToken,
        result: Result<(), i32>,
        now_us: u64,
    ) -> ColdReceipt {
        let mut state_ref = self.state.borrow_mut();
        let state = &mut *state_ref;
        let Some(read) = state.inflight.get_mut(&token) else {
            state.counters.receipts_foreign += 1;
            return ColdReceipt::Foreign;
        };
        let WindowPhase::Issued = read.phase else {
            state.counters.receipts_foreign += 1;
            return ColdReceipt::Foreign;
        };
        for waiter in &read.waiters {
            debug_assert!(
                waiter.skip + waiter.len <= read.len,
                "every waiter subrange sits inside the window"
            );
        }
        read.phase = WindowPhase::Ready { outcome: result, receipt_us: now_us };
        let members = read.waiters.len();
        let buf = read.buf;
        match result {
            Ok(()) => state.counters.completed += 1,
            Err(_) => state.counters.errored += 1,
        }
        state.device_reads -= 1;
        // Lossless: a window's members are at most the class queue's bound.
        state.windows.insert(token, SharedWindow { buf, remaining: members as u32 });
        state.pending_members += members as u64;
        state.pending_windows += 1;
        Self::link_ready(state, token);
        ColdReceipt::Recorded
    }

    /// The turn's delivery service (ADR-0181 R1): visits the ready windows'
    /// members round-robin, one member a step and each step one unit of
    /// `budget` spent before its visit, until the list is empty or the
    /// turn's allowance is spent. A step reads the member at its window's
    /// cursor, advances the cursor and re-links the window at the tail (or
    /// takes it off at its last member) **before** it completes the gate,
    /// with the state borrow released, so a value dropped in the gate's
    /// orphan cleanup re-resolves its window. A live waiter receives its
    /// [`ColdDone`] (`delivered`); an orphaned member's value is dropped,
    /// returning its share (`unclaimed`). `now_us` is injected time (L7).
    pub fn deliver_ready(
        &self,
        budget: &mut ColdDeliveryBudget,
        now_us: u64,
    ) -> ColdDeliveryProgress {
        let spent_before = budget.spent();
        let mut progress =
            ColdDeliveryProgress { delivered: 0, unclaimed: 0, state: ColdDeliveryState::Idle };
        while let Some(step) = self.delivery_step(budget) {
            if self.gate.complete(step.logical, step.done) {
                progress.delivered += 1;
                let mut state = self.state.borrow_mut();
                state.counters.delivered += 1;
                state.latency_hist.record(now_us.saturating_sub(step.enqueued_us));
                state.delay_hist.record(now_us.saturating_sub(step.receipt_us));
            } else {
                progress.unclaimed += 1;
                self.state.borrow_mut().counters.unclaimed += 1;
            }
        }
        let mut state = self.state.borrow_mut();
        let spent = budget.spent().saturating_sub(spent_before);
        state.counters.delivery_work += u64::from(spent);
        let turn_max = state.counters.delivery_work_turn_max.max(budget.spent());
        state.counters.delivery_work_turn_max = turn_max;
        if state.ready_head.is_some() {
            progress.state = ColdDeliveryState::Ready;
        }
        progress
    }

    /// One delivery step under a short state borrow: a unit spent, the
    /// ready head's member at its cursor read, the cursor advanced and the
    /// window re-linked or retired, and the member's value built. `None`
    /// with nothing spent when no window is ready or the allowance is
    /// spent; `None` after the unit when a link names no ready window, a
    /// defect counted where the walk stopped.
    fn delivery_step(&self, budget: &mut ColdDeliveryBudget) -> Option<DeliveryStep> {
        let mut state_ref = self.state.borrow_mut();
        let state = &mut *state_ref;
        let window = state.ready_head?;
        if !budget.take() {
            return None;
        }
        let Some(read) = state.inflight.get_mut(&window) else {
            return Self::link_stale(state, "the ready head names no window");
        };
        let WindowPhase::Ready { outcome, receipt_us } = read.phase else {
            return Self::link_stale(state, "the ready head names an Issued window");
        };
        let Some(waiter) = read.waiters.get(read.cursor as usize) else {
            return Self::link_stale(state, "the ready head has no member left");
        };
        let (logical, skip, len, enqueued_us) =
            (waiter.logical, waiter.skip, waiter.len, waiter.enqueued_us);
        read.cursor += 1;
        let last = read.cursor as usize == read.waiters.len();
        let file = read.file;
        state.ready_head = read.ready_next.take();
        if state.ready_head.is_none() {
            state.ready_tail = None;
        }
        state.pending_members -= 1;
        if last {
            state.pending_windows -= 1;
            if let Some(mut read) = state.inflight.remove(&window) {
                read.waiters.clear();
                state.spare_waiters.push(read.waiters);
            }
        } else {
            Self::link_ready(state, window);
        }
        let done = ColdDone { outcome, state: Rc::clone(&self.state), window, skip, len, file };
        Some(DeliveryStep { logical, done, enqueued_us, receipt_us })
    }

    /// Links `window` at the ready list's tail.
    fn link_ready(state: &mut ColdState, window: CompletionToken) {
        match state.ready_tail {
            Some(tail) => match state.inflight.get_mut(&tail) {
                Some(read) => read.ready_next = Some(window),
                None => {
                    state.counters.links_stale += 1;
                    debug_assert!(false, "cold-link-stale: the ready tail names no window");
                }
            },
            None => state.ready_head = Some(window),
        }
        state.ready_tail = Some(window);
    }

    /// A ready-list link that named no ready window (a defect): counted, a
    /// debug assertion carrying `cold-link-stale`, and the walk stops.
    fn link_stale(state: &mut ColdState, why: &str) -> Option<DeliveryStep> {
        state.counters.links_stale += 1;
        debug_assert!(false, "cold-link-stale: {why}");
        None
    }

    /// Whether a window has members left to visit: the park decision's
    /// delivery half (ADR-0181 R2).
    #[must_use]
    pub fn has_ready_delivery(&self) -> bool {
        self.state.borrow().ready_head.is_some()
    }

    /// Members of ready windows not yet visited, and those windows
    /// (`cold_delivery_pending_members`, `cold_delivery_pending_windows`).
    #[must_use]
    pub fn delivery_pending(&self) -> (u64, u64) {
        let state = self.state.borrow();
        (state.pending_members, state.pending_windows)
    }

    /// Receipt-to-delivery delay percentile in injected µs
    /// (`cold_delivery_delay_p99_us` at `p = 99.0`; delivered values only).
    #[must_use]
    pub fn delivery_delay_percentile_us(&self, p: f64) -> u64 {
        self.state.borrow().delay_hist.percentile(p)
    }

    /// Cold reads pinning `file` — queued, in flight, or delivered with
    /// live custody. The §3.3 pin observable: compaction/reclaim/drop
    /// defer close+unlink until this drains.
    #[must_use]
    pub fn inflight_on(&self, file: TierFileId) -> u32 {
        self.state.borrow().pins.get(&file).copied().unwrap_or(0)
    }

    /// Device reads in flight — `cold_reads_inflight`, bounded by the
    /// QD cap (merged reads count once; the S08-era hook, now capped): the
    /// Issued windows, a Ready one no longer counted.
    #[must_use]
    pub fn inflight_total(&self) -> usize {
        self.state.borrow().device_reads
    }

    /// Queued intents across both classes — `cold_queue_depth` (the
    /// high-water mark rides the counters).
    #[must_use]
    pub fn queue_depth(&self) -> usize {
        let state = self.state.borrow();
        state.pending[0].len() + state.pending[1].len()
    }

    /// Device-QD percentile over issue-time samples (`cold_read_qd_p99`
    /// at `p = 99.0`).
    #[must_use]
    pub fn qd_percentile(&self, p: f64) -> u64 {
        self.state.borrow().qd_hist.percentile(p)
    }

    /// Enqueue→delivery latency percentile in injected µs
    /// (`cold_read_p99_us` at `p = 99.0`; delivered reads only).
    #[must_use]
    pub fn latency_percentile_us(&self, p: f64) -> u64 {
        self.state.borrow().latency_hist.percentile(p)
    }

    /// Counter snapshot.
    #[must_use]
    pub fn counters(&self) -> ColdReadCounters {
        self.state.borrow().counters
    }

    /// The identity pairs the host's issuer has spent or skipped, its
    /// position (`cold_identity_pairs_used`, cell scope).
    #[must_use]
    pub fn identity_pairs_used(&self) -> u64 {
        self.state.borrow().issuer.pairs_used()
    }

    /// `enqueue`s refused [`ColdRefused::IdentityExhausted`]
    /// (`cold_identity_exhausted`, cell scope, cumulative).
    #[must_use]
    pub fn identity_refusals(&self) -> u64 {
        self.state.borrow().identity_refusals
    }

    /// The in-flight device-read cap (ADR-0055 D2) — the most intents one
    /// command should have queued before it suspends (`SCAN`'s chunk).
    #[must_use]
    pub fn qd_cap(&self) -> usize {
        self.state.borrow().config.qd_cap
    }

    /// Leak hook: after a storm drains, every lease is back, nothing is
    /// queued or in flight, no window holds custody, and no file stays
    /// pinned.
    ///
    /// # Errors
    /// The outstanding custody counts.
    pub fn reconcile(&self) -> Result<(), ColdLeak> {
        let state = self.state.borrow();
        let leak = ColdLeak {
            leased: state.pool.leased(),
            inflight: state.inflight.len(),
            pinned_files: state.pins.len(),
            queued: state.pending[0].len() + state.pending[1].len(),
            windows: state.windows.len(),
        };
        if leak.leased == 0
            && leak.inflight == 0
            && leak.pinned_files == 0
            && leak.queued == 0
            && leak.windows == 0
        {
            Ok(())
        } else {
            Err(leak)
        }
    }
}

impl Clone for ColdReads {
    /// Shared handle: the completion dispatch and reading futures share
    /// one state (cell-local `Rc`, the gate pattern).
    fn clone(&self) -> ColdReads {
        ColdReads { state: Rc::clone(&self.state), gate: self.gate.clone() }
    }
}

impl core::fmt::Debug for ColdReads {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let state = self.state.borrow();
        write!(
            f,
            "ColdReads {{ queued: {}, inflight: {}, pinned_files: {}, leased: {} }}",
            state.pending[0].len() + state.pending[1].len(),
            state.inflight.len(),
            state.pins.len(),
            state.pool.leased()
        )
    }
}

/// A terminally-completed cold read **and** this waiter's share of the
/// window custody. Dropping it — resumed, cancelled, or unclaimed —
/// releases this waiter's file pin and, on the last drop of the shared
/// window, once every member was visited, the buffer lease: exactly once,
/// wherever the values end up. One share is one value, so it is not
/// `Clone`:
///
/// ```compile_fail,E0599
/// fn twice(done: inf_runtime::ColdDone) -> (inf_runtime::ColdDone, inf_runtime::ColdDone) {
///     (done.clone(), done)
/// }
/// ```
///
/// Its twin, the share moved once, compiles:
///
/// ```
/// fn once(done: inf_runtime::ColdDone) -> inf_runtime::ColdDone {
///     done
/// }
/// ```
pub struct ColdDone {
    outcome: Result<(), i32>,
    state: Rc<RefCell<ColdState>>,
    /// Shared-window key (the device read's token).
    window: CompletionToken,
    /// This waiter's subrange of the window.
    skip: u32,
    len: u32,
    file: TierFileId,
}

impl ColdDone {
    /// `Ok` when the window is full; `Err(errno)` on the op's terminal error
    /// (EIO inside the flushed range is corruption — the caller surfaces it
    /// typed).
    pub fn outcome(&self) -> Result<(), i32> {
        self.outcome
    }

    /// The file this read pinned (resume-side validation input).
    #[must_use]
    pub fn file(&self) -> TierFileId {
        self.file
    }

    /// Reads this waiter's filled subrange under a short borrow (never
    /// held across an await — the closure shape makes escape
    /// unrepresentable). Waiters of one merged window each see exactly
    /// their own range; overlapping requests see the same bytes.
    ///
    /// # Panics
    /// Debug-panics when the read errored — decode-after-error is a
    /// caller bug ([`outcome`](Self::outcome) gates it).
    pub fn bytes<R>(&self, read: impl FnOnce(&[u8]) -> R) -> R {
        debug_assert!(self.outcome.is_ok(), "decoding an errored cold read");
        let state = self.state.borrow();
        let window = &state.windows[&self.window];
        let at = self.skip as usize;
        read(&state.pool.bytes(window.buf)[at..at + self.len as usize])
    }
}

impl Drop for ColdDone {
    fn drop(&mut self) {
        let mut state = self.state.borrow_mut();
        let ColdState { pins, windows, pool, .. } = &mut *state;
        ColdState::unpin(pins, self.file);
        let window = windows.get_mut(&self.window).expect("shared window outlives its ColdDones");
        window.remaining -= 1;
        if window.remaining == 0 {
            pool.release(window.buf);
            windows.remove(&self.window);
        }
    }
}

impl core::fmt::Debug for ColdDone {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "ColdDone {{ outcome: {:?}, file: {:?}, skip: {}, len: {} }}",
            self.outcome, self.file, self.skip, self.len
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use inf_foundation::limits::FILE_OFFSET_BYTES_MAX;

    const BUF: usize = 4 * inf_alloc::TIER_READ_ALIGN;
    const FRAME: u64 = inf_alloc::TIER_READ_ALIGN as u64;

    fn path(buffers: usize) -> ColdReads {
        ColdReads::new(AlignedPool::new(buffers, BUF))
    }

    fn shaped(buffers: usize, config: ColdReadConfig) -> ColdReads {
        ColdReads::with_config(AlignedPool::new(buffers, BUF), config)
    }

    /// Enqueue at t=0, foreground (the common shape in these tests).
    fn ask(cold: &ColdReads, fd: RawFd, file: TierFileId, offset: u64, len: usize) -> ColdWait {
        cold.enqueue(fd, file, offset, len, ReadClass::Foreground, 0).expect("queue sized")
    }

    fn drain_ops(cold: &ColdReads) -> Vec<IoOp> {
        let mut ops = Vec::new();
        cold.drain(|op| ops.push(op));
        ops
    }

    /// Plays a receipt and the turns that deliver it: records `result` for
    /// `token`, then serves whole turns until no window is ready; returns
    /// the values delivered to live waiters.
    fn deliver_all(
        cold: &ColdReads,
        token: CompletionToken,
        result: Result<(), i32>,
        now_us: u64,
    ) -> u32 {
        assert_eq!(cold.record_completion(token, result, now_us), ColdReceipt::Recorded);
        let mut delivered = 0;
        while cold.has_ready_delivery() {
            let mut turn = ColdTurn::begin();
            delivered += cold.deliver_ready(turn.delivery(), now_us).delivered;
        }
        delivered
    }

    /// Play the driver: fill the destination with `fill` through the
    /// stable handle and hand back the device token.
    fn complete(op: IoOp, fill: u8) -> CompletionToken {
        let IoOp::TierRead { token, buf, .. } = op else { panic!("drain builds TierRead") };
        // SAFETY: test-only driver stand-in — the in-flight table holds
        // the lease; this is the only writer, exactly as the backend
        // would be under the StableBytesMut contract.
        unsafe { core::slice::from_raw_parts_mut(buf.as_mut_ptr(), buf.len() as usize) }.fill(fill);
        token
    }

    /// The normal life: enqueue → drain → complete → waiter decodes →
    /// drop releases.
    #[test]
    fn resumed_read_owns_then_releases_custody() {
        let cold = path(2);
        let file = TierFileId::new(7);
        let waiter = ask(&cold, 3, file, FRAME, 100);
        assert_eq!(cold.inflight_on(file), 1, "pinned at enqueue");
        assert_eq!(cold.queue_depth(), 1);
        let mut ops = drain_ops(&cold);
        assert_eq!(ops.len(), 1, "one intent, one device read");
        assert_eq!(cold.queue_depth(), 0);
        assert_eq!(cold.inflight_total(), 1);
        let token = complete(ops.remove(0), 0xC0);
        assert_eq!(deliver_all(&cold, token, Ok(()), 5), 1);
        let done = block_on_ready(waiter);
        assert_eq!(done.outcome(), Ok(()));
        assert_eq!(done.file(), file);
        done.bytes(|bytes| {
            assert_eq!(bytes.len(), 100);
            assert!(bytes.iter().all(|&b| b == 0xC0));
        });
        assert_eq!(cold.inflight_on(file), 1, "pin held until custody drops");
        drop(done);
        assert_eq!(cold.inflight_on(file), 0);
        assert_eq!(cold.reconcile(), Ok(()));
        let counters = cold.counters();
        assert_eq!(counters.completed, 1);
        assert_eq!(counters.delivered, 1);
        assert!(cold.latency_percentile_us(99.0) >= 5, "injected time reached the histogram");
    }

    /// Cancelled after issue, before completion: the unclaimed delivery
    /// releases here.
    #[test]
    fn cancellation_before_completion_releases_at_the_gate() {
        let cold = path(1);
        let file = TierFileId::new(1);
        let waiter = ask(&cold, 3, file, 0, 64);
        let mut ops = drain_ops(&cold);
        assert_eq!(ops.len(), 1);
        drop(waiter); // the command was cancelled mid-suspension
        assert_eq!(cold.inflight_on(file), 1, "the op still owns the pin");
        let token = complete(ops.remove(0), 0xC0);
        assert_eq!(deliver_all(&cold, token, Ok(()), 0), 0, "no live waiter");
        assert_eq!(cold.reconcile(), Ok(()), "custody released at the unclaimed delivery");
        assert_eq!(cold.counters().unclaimed, 1);
    }

    /// Cancelled after delivery, before polling: the parked value's drop
    /// (inside the gate slot) releases.
    #[test]
    fn cancellation_after_delivery_releases_with_the_parked_value() {
        let cold = path(1);
        let file = TierFileId::new(2);
        let waiter = ask(&cold, 3, file, 0, 64);
        let mut ops = drain_ops(&cold);
        let token = complete(ops.remove(0), 0xC0);
        assert_eq!(deliver_all(&cold, token, Ok(()), 0), 1, "delivery parks");
        drop(waiter); // cancelled between delivery and poll
        assert_eq!(cold.reconcile(), Ok(()), "the parked ColdDone released on drop");
    }

    /// Cancelled while still queued: the drain skips the intent — no
    /// buffer leased, no device read, pin released, counted.
    #[test]
    fn cancellation_while_queued_spends_no_device_read() {
        let cold = path(1);
        let file = TierFileId::new(8);
        let waiter = ask(&cold, 3, file, 0, 64);
        drop(waiter); // dead before any drain ran
        assert!(drain_ops(&cold).is_empty(), "a dead command buys no bandwidth");
        assert_eq!(cold.counters().cancelled_queued, 1);
        assert_eq!(cold.counters().issued, 0);
        assert_eq!(cold.reconcile(), Ok(()));
    }

    /// Errors return custody exactly like success (the M0 terminal rule).
    #[test]
    fn errored_read_returns_custody_typed() {
        let cold = path(1);
        let file = TierFileId::new(3);
        let waiter = ask(&cold, 3, file, 0, 64);
        let mut ops = drain_ops(&cold);
        let IoOp::TierRead { token, .. } = ops.remove(0) else { panic!("drain builds TierRead") };
        assert_eq!(deliver_all(&cold, token, Err(libc::EIO), 0), 1);
        let done = block_on_ready(waiter);
        assert_eq!(done.outcome(), Err(libc::EIO), "EIO in the flushed range is corruption");
        drop(done);
        assert_eq!(cold.reconcile(), Ok(()));
        assert_eq!(cold.counters().errored, 1);
    }

    /// A dry pool stalls the drain (counted, queue intact) and the read
    /// proceeds once custody returns — sizing pressure, not loss.
    #[test]
    fn pool_dry_stalls_drain_and_recovers() {
        let cold = path(1);
        let file = TierFileId::new(4);
        // Far apart: the merge window must not join them.
        let first = ask(&cold, 3, file, 0, 64);
        let second = ask(&cold, 3, file, 1 << 20, 64);
        let mut ops = drain_ops(&cold);
        assert_eq!(ops.len(), 1, "one buffer, one device read");
        assert_eq!(cold.counters().pool_dry, 1, "the second intent stalled on the pool");
        assert_eq!(cold.queue_depth(), 1);
        let token = complete(ops.remove(0), 0xAA);
        deliver_all(&cold, token, Ok(()), 0);
        drop(block_on_ready(first)); // returns the lease
        let mut ops = drain_ops(&cold);
        assert_eq!(ops.len(), 1, "the queued intent proceeds after the release");
        let token = complete(ops.remove(0), 0xBB);
        deliver_all(&cold, token, Ok(()), 0);
        drop(block_on_ready(second));
        assert_eq!(cold.reconcile(), Ok(()));
    }

    /// A full class FIFO refuses typed with nothing pinned or leaked.
    #[test]
    fn queue_full_refuses_without_side_effects() {
        let config = ColdReadConfig { overflow_cap: 1, ..ColdReadConfig::default() };
        let cold = shaped(2, config);
        let file = TierFileId::new(5);
        let _first = ask(&cold, 3, file, 0, 64);
        let refused = cold.enqueue(3, file, 1 << 20, 64, ReadClass::Foreground, 0);
        assert!(matches!(refused, Err(ColdRefused::QueueFull)));
        assert_eq!(cold.counters().queue_full, 1);
        assert_eq!(cold.inflight_on(file), 1, "the refusal pinned nothing");
        assert_eq!(cold.queue_depth(), 1);
    }

    /// The QD cap holds under a flood and the overflow FIFO drains in
    /// arrival order as completions free slots.
    #[test]
    fn qd_cap_holds_and_overflow_drains_fifo() {
        let config = ColdReadConfig { qd_cap: 2, merge: false, ..ColdReadConfig::default() };
        let cold = shaped(4, config);
        let file = TierFileId::new(6);
        let offsets: [u64; 4] = [0, 1 << 20, 2 << 20, 3 << 20];
        let waiters: Vec<ColdWait> =
            offsets.iter().map(|&at| ask(&cold, 3, file, at, 64)).collect();
        let ops = drain_ops(&cold);
        assert_eq!(ops.len(), 2, "the cap admits exactly qd_cap device reads");
        assert_eq!(cold.inflight_total(), 2);
        assert_eq!(cold.queue_depth(), 2);
        let issued: Vec<u64> = ops
            .iter()
            .map(|op| {
                let IoOp::TierRead { offset, .. } = op else { panic!("TierRead") };
                offset.bytes()
            })
            .collect();
        assert_eq!(issued, offsets[..2], "FIFO admission order");
        assert!(drain_ops(&cold).is_empty(), "cap holds while nothing completed");
        let token = complete(ops.into_iter().next().expect("first"), 0xCC);
        deliver_all(&cold, token, Ok(()), 0);
        let ops = drain_ops(&cold);
        assert_eq!(ops.len(), 1, "one freed slot admits one queued intent");
        let IoOp::TierRead { offset, .. } = &ops[0] else { panic!("TierRead") };
        assert_eq!(offset.bytes(), offsets[2], "oldest queued intent first");
        assert!(cold.counters().queue_depth_high_water >= 4);
        assert!(cold.qd_percentile(99.0) <= 2, "sampled QD never exceeded the cap");
        drop(waiters);
    }

    /// Adjacent and overlapping same-file intents merge into one device
    /// read; the CQE fans out per-waiter subranges of the shared window.
    #[test]
    fn adjacent_reads_coalesce_and_fan_out() {
        let cold = path(2);
        let file = TierFileId::new(9);
        // Three whole frames, contiguous, plus one overlapping the middle.
        let a = ask(&cold, 3, file, FRAME, FRAME as usize);
        let b = ask(&cold, 3, file, 2 * FRAME, FRAME as usize);
        let c = ask(&cold, 3, file, 3 * FRAME, FRAME as usize);
        let d = ask(&cold, 3, file, 2 * FRAME, 128);
        let mut ops = drain_ops(&cold);
        assert_eq!(ops.len(), 1, "one merged device read");
        let IoOp::TierRead { offset, ref buf, .. } = ops[0] else { panic!("TierRead") };
        assert_eq!(offset.bytes(), FRAME);
        assert_eq!(buf.len() as u64, 3 * FRAME, "the union window");
        let counters = cold.counters();
        assert_eq!(counters.issued, 1);
        assert_eq!(counters.merged_waiters, 3);
        let token = complete(ops.remove(0), 0xEE);
        assert_eq!(deliver_all(&cold, token, Ok(()), 9), 4, "full fan-out");
        for (waiter, len) in [(a, FRAME as usize), (b, FRAME as usize), (c, FRAME as usize)] {
            let done = block_on_ready(waiter);
            done.bytes(|bytes| assert_eq!(bytes.len(), len));
            drop(done);
        }
        let done = block_on_ready(d);
        done.bytes(|bytes| {
            assert_eq!(bytes.len(), 128, "the overlapping waiter sees its own subrange");
            assert!(bytes.iter().all(|&byte| byte == 0xEE));
        });
        drop(done);
        assert_eq!(cold.reconcile(), Ok(()), "last drop released the shared window");
        // Admission identity at quiesce.
        let counters = cold.counters();
        assert_eq!(
            counters.enqueued,
            counters.issued + counters.merged_waiters + counters.cancelled_queued
        );
        // D5 ratio on the live engine: four logical reads, one device
        // read — three quarters of the trips were saved.
        assert_eq!(counters.coalesce_ratio_milli(), 750);
    }

    /// ADR-0055 D5 conformance for `coalesce_ratio`: `1 − device/logical`
    /// — near-zero merging reads ≈ 0 milli, heavy merging reads the
    /// merged fraction. Pins the v0.4.0-alpha soak inversion, which
    /// rendered `enqueued/issued` = 1000 at effectively zero coalescing.
    #[test]
    fn coalesce_ratio_follows_the_d5_definition() {
        // The soak's raw counters: 427 merged waiters in 102 M reads.
        let soak = ColdReadCounters {
            enqueued: 102_311_118,
            issued: 102_310_691,
            ..ColdReadCounters::default()
        };
        assert_eq!(soak.coalesce_ratio_milli(), 0, "essentially zero coalescing reads 0");
        let merged =
            ColdReadCounters { enqueued: 1000, issued: 250, ..ColdReadCounters::default() };
        assert_eq!(merged.coalesce_ratio_milli(), 750, "three of four trips saved");
        assert_eq!(ColdReadCounters::default().coalesce_ratio_milli(), 0, "idle engine reads 0");
    }

    /// Cancelling one waiter of a merged read releases exactly its share:
    /// the survivors decode, the window frees on the last drop.
    #[test]
    fn merged_read_survives_partial_cancellation() {
        let cold = path(2);
        let file = TierFileId::new(10);
        let keep = ask(&cold, 3, file, 0, FRAME as usize);
        let cancel = ask(&cold, 3, file, FRAME, FRAME as usize);
        let mut ops = drain_ops(&cold);
        assert_eq!(ops.len(), 1, "merged before the cancellation");
        drop(cancel); // cancelled while the merged read is in flight
        let token = complete(ops.remove(0), 0xDD);
        assert_eq!(deliver_all(&cold, token, Ok(()), 0), 1);
        let counters = cold.counters();
        assert_eq!(counters.delivered, 1);
        assert_eq!(counters.unclaimed, 1, "the cancelled share released at delivery");
        let done = block_on_ready(keep);
        done.bytes(|bytes| assert!(bytes.iter().all(|&byte| byte == 0xDD)));
        drop(done);
        assert_eq!(cold.reconcile(), Ok(()));
    }

    /// Reads never merge across descriptors either (N5, review
    /// remediation of 2026-09-01): two fetches of one blob extent open
    /// two readers under one synthetic file id — each device read
    /// carries exactly the fd its intent named.
    #[test]
    fn merging_never_crosses_descriptors() {
        let cold = path(4);
        let file = TierFileId::new(0x8000_0007);
        let a = ask(&cold, 3, file, 0, FRAME as usize);
        let b = ask(&cold, 4, file, FRAME, FRAME as usize);
        let ops = drain_ops(&cold);
        assert_eq!(ops.len(), 2, "adjacent offsets, one file id, two fds: two device reads");
        drop((a, b));
    }

    /// Reads never merge across files, whatever their offsets say.
    #[test]
    fn merging_never_crosses_files() {
        let cold = path(4);
        let a = ask(&cold, 3, TierFileId::new(11), 0, FRAME as usize);
        let b = ask(&cold, 4, TierFileId::new(12), FRAME, FRAME as usize);
        let ops = drain_ops(&cold);
        assert_eq!(ops.len(), 2, "adjacent offsets, different files: two device reads");
        drop((a, b));
    }

    /// Under two-class contention the deficit split admits foreground
    /// and maintain in the configured 3:1 pattern; a lone class is
    /// work-conserving (takes every slot).
    #[test]
    fn deficit_split_meters_contention_three_to_one() {
        let config = ColdReadConfig { qd_cap: 1, merge: false, ..ColdReadConfig::default() };
        // Pool sized for every read: the parked ColdDones (waiters are
        // never polled here) hold their leases until the final drop.
        let cold = shaped(16, config);
        let file = TierFileId::new(13);
        // Foreground at low offsets, maintain at high offsets — the
        // offset marks the class in the issued order. 12:4 sustains the
        // 3:1 pattern for exactly 16 slots (both classes stay non-empty
        // until the end, so the contention meter never disengages).
        let mut waiters = Vec::new();
        for lane in 0..12u64 {
            waiters.push(ask(&cold, 3, file, lane << 20, 64));
        }
        for lane in 0..4u64 {
            waiters.push(
                cold.enqueue(3, file, (100 + lane) << 20, 64, ReadClass::Maintain, 0)
                    .expect("queue sized"),
            );
        }
        let mut order = Vec::new();
        for _ in 0..16 {
            let mut ops = drain_ops(&cold);
            assert_eq!(ops.len(), 1, "qd_cap 1 serializes admissions");
            let IoOp::TierRead { offset, .. } = ops[0] else { panic!("TierRead") };
            order.push(if offset.bytes() >= 100 << 20 { 'm' } else { 'f' });
            let token = complete(ops.remove(0), 0x11);
            deliver_all(&cold, token, Ok(()), 0);
        }
        let split: String = order.iter().collect();
        assert_eq!(split, "fffmfffmfffmfffm", "3:1 deficit under contention");
        drop(waiters);
    }

    /// The v0.4.0-alpha soak fingerprint (`cold_read_p99_us:85899345919`):
    /// the histogram must report the enqueue→delivery *delta*, so a read
    /// issued deep into a long run reports its own latency, never the
    /// absolute clock. The wired plane stamps both ends with the same
    /// injected loop clock; a zero enqueue stamp is the bug this pins —
    /// the percentile then reads ~uptime, and after 24 h that lands on
    /// the log-bucket edge 2^36 + 2^34 - 1 µs.
    #[test]
    fn latency_measures_the_delta_not_the_absolute_clock() {
        let cold = path(1);
        let file = TierFileId::new(14);
        // ~23.9 h of injected uptime, then a 12 µs device read.
        let late_us: u64 = 86_000_000_000;
        let waiter =
            cold.enqueue(3, file, 0, 64, ReadClass::Foreground, late_us).expect("queue sized");
        let mut ops = drain_ops(&cold);
        let token = complete(ops.remove(0), 0xC0);
        assert_eq!(deliver_all(&cold, token, Ok(()), late_us + 12), 1);
        assert_eq!(cold.latency_percentile_us(99.0), 12, "the delta, never uptime");
        drop(block_on_ready(waiter));
        assert_eq!(cold.reconcile(), Ok(()));
    }

    /// The percentile walk must hold past any u32 sample count (the 24 h
    /// soak recorded 102 M deliveries; this goes to > 2^32 for margin):
    /// bucket counts and their accumulation are u64 end to end, so the
    /// reported value stays inside the recorded range instead of walking
    /// off the buckets. Counts inflate by histogram merge-doubling —
    /// recording 4.3 B samples one by one has no place in a unit test.
    #[test]
    fn latency_percentile_holds_past_u32_sample_counts() {
        let cold = path(1);
        let file = TierFileId::new(15);
        let waiter = ask(&cold, 3, file, 0, 64);
        let mut ops = drain_ops(&cold);
        let token = complete(ops.remove(0), 0xC0);
        assert_eq!(deliver_all(&cold, token, Ok(()), 12), 1);
        {
            // Same-file test module: reach the private histogram and
            // Fibonacci-double its counts past u32::MAX (25 rounds of
            // pairwise merge ≈ F(51) ≈ 2×10^10 samples of value 12).
            let mut state = cold.state.borrow_mut();
            let mut mirror = LogHistogram::new();
            mirror.merge(&state.latency_hist);
            for _ in 0..25 {
                state.latency_hist.merge(&mirror);
                mirror.merge(&state.latency_hist);
            }
            assert!(state.latency_hist.count() > u64::from(u32::MAX), "count exceeds any u32");
        }
        assert_eq!(cold.latency_percentile_us(50.0), 12);
        assert_eq!(cold.latency_percentile_us(99.0), 12);
        assert_eq!(cold.latency_percentile_us(99.9), 12, "inside the recorded range");
        drop(block_on_ready(waiter));
        assert_eq!(cold.reconcile(), Ok(()));
    }

    /// A caller whose `admit` refuses the foreground breaches ADR-0178 D1
    /// (a foreground offer is `Now`) — the slice must END (the intent
    /// stays queued for the next one), never spin on a refusal that cannot
    /// change within it.
    #[test]
    fn a_refused_foreground_ends_the_slice() {
        let cold = path(2);
        let _wait = ask(&cold, 3, TierFileId::new(7), FRAME, 100);
        let mut asked = 0u32;
        let issued = cold.drain_budgeted(
            |_, _| {
                asked += 1;
                Issue::NotThisSlice
            },
            |_, _, _| {},
            |_| panic!("a refused read never issues"),
        );
        assert_eq!(issued, 0);
        assert_eq!(asked, 1, "one refusal ends the slice");
        assert_eq!(cold.queue_depth(), 1, "the intent stays queued");
        assert_eq!(cold.counters().maintain_deferred, 0, "not a maintain deferral");
    }

    /// F-L11-04 (review 2026-08-30 lane L11): every `admit` charges one
    /// device op beside the byte bound; a pool-dry stall issues no SQE,
    /// so the op must come back with the bytes — pre-fix only the bytes
    /// did, and `io_budget_ops_cold_read_*` drifted by `pool_dry`.
    #[test]
    fn a_pool_dry_stall_refunds_the_charged_op() {
        let cold = path(1);
        let file = TierFileId::new(4);
        // Far apart: the merge window must not join them.
        let first = ask(&cold, 3, file, 0, 64);
        let second = ask(&cold, 3, file, 1 << 20, 64);
        let (mut charged_bytes, mut charged_ops) = (0u64, 0u64);
        let (mut refunded_bytes, mut refunded_ops) = (0u64, 0u64);
        let mut windows = 0u64;
        let issued = cold.drain_budgeted(
            |_, bytes| {
                charged_bytes += bytes;
                charged_ops += 1;
                Issue::Now
            },
            |_, bytes, ops| {
                refunded_bytes += bytes;
                refunded_ops += ops;
            },
            |op| {
                let IoOp::TierRead { buf, .. } = op else { panic!("TierRead") };
                windows += u64::from(buf.len());
            },
        );
        assert_eq!(issued, 1, "one buffer, one device read");
        assert_eq!(cold.counters().pool_dry, 1, "the second intent stalled on the pool");
        assert_eq!(charged_bytes - refunded_bytes, windows, "net bytes = issued windows");
        assert_eq!(
            charged_ops - refunded_ops,
            u64::from(issued),
            "net ops = reads issued (drifts by pool_dry pre-fix)"
        );
        drop((first, second));
    }

    /// F-L11-04, the lane's second half: a maintain read the budget
    /// defers keeps its contention grant — the deferral is "not this
    /// slice", not a turn taken. Pre-fix the grant was spent on nothing
    /// and the next slice refilled foreground-first.
    #[test]
    fn a_deferred_maintain_keeps_its_grant() {
        let config = ColdReadConfig { merge: false, ..ColdReadConfig::default() };
        let cold = shaped(16, config);
        let file = TierFileId::new(13);
        let mut waiters = Vec::new();
        for lane in 0..4u64 {
            waiters.push(ask(&cold, 3, file, lane << 20, 64));
        }
        waiters.push(
            cold.enqueue(3, file, 100 << 20, 64, ReadClass::Maintain, 0).expect("queue sized"),
        );
        // Slice 1: fff, then the maintain turn (grants 3:1) is deferred,
        // then the fourth foreground drains alone.
        let mut first = Vec::new();
        let issued = cold.drain_budgeted(
            |class, _| match class {
                ReadClass::Foreground => Issue::Now,
                ReadClass::Maintain => Issue::NotThisSlice,
            },
            |_, _, _| {},
            |op| first.push(op),
        );
        assert_eq!(issued, 4);
        assert_eq!(cold.counters().maintain_deferred, 1);
        // Slice 2, under contention again: the deferred maintain's turn
        // comes first — its grant survived the deferral.
        waiters.push(ask(&cold, 3, file, 50 << 20, 64));
        let mut second = Vec::new();
        cold.drain_budgeted(|_, _| Issue::Now, |_, _, _| {}, |op| second.push(op));
        let IoOp::TierRead { offset, .. } = second[0] else { panic!("TierRead") };
        assert_eq!(
            offset.bytes(),
            100 << 20,
            "the deferred maintain read goes first, not a refilled fff"
        );
        drop((first, second, waiters));
    }

    /// Everything a refused `enqueue` must leave untouched (ADR-0167 D3):
    /// counters, queue depth, the file's pins and the issuer's position.
    fn footprint(cold: &ColdReads, file: TierFileId) -> (ColdReadCounters, usize, u32, u64) {
        let pairs_used = cold.identity_pairs_used();
        (cold.counters(), cold.queue_depth(), cold.inflight_on(file), pairs_used)
    }

    /// The permanent refusal `enqueue` answered, or a panic naming `what`.
    fn unrepresentable(asked: Result<ColdWait, ColdRefused>, what: &str) -> UnrepresentableRead {
        match asked {
            Err(ColdRefused::Unrepresentable(read)) => read,
            Err(ColdRefused::QueueFull) => panic!("{what}: answered QueueFull"),
            Err(ColdRefused::IdentityExhausted) => panic!("{what}: answered IdentityExhausted"),
            Ok(_) => panic!("{what}: enqueue accepted it"),
        }
    }

    /// ADR-0167 D3: a position above `FILE_OFFSET_BYTES_MAX` is one the
    /// kernel cannot address for a whole driver op (io_uring reads
    /// `u64::MAX` as its −1 "current position" sentinel). `enqueue` refuses
    /// it, carrying the value, before any state changes. The saturated blob
    /// extent position is the real producer of `u64::MAX`.
    #[test]
    fn enqueue_refuses_an_offset_the_kernel_cannot_address() {
        let cold = path(1);
        let file = TierFileId::new(16);
        let saturated =
            inf_log::blob::extent_frame_offset(u64::MAX / inf_log::TIER_FRAME_BYTES as u64 + 1);
        assert_eq!(saturated, u64::MAX, "the extent producer saturates");
        let inputs = [i64::MAX.cast_unsigned() + 1, u64::MAX, FILE_OFFSET_BYTES_MAX + 1, saturated];
        for offset in inputs {
            let before = footprint(&cold, file);
            let asked = cold.enqueue(3, file, offset, FRAME as usize, ReadClass::Foreground, 0);
            let Err(ColdRefused::Unrepresentable(UnrepresentableRead::OffsetAboveMax(refused))) =
                asked
            else {
                panic!("enqueue accepted offset {offset} (above FILE_OFFSET_BYTES_MAX)");
            };
            assert_eq!(refused.offset_bytes(), offset, "the refusal carries the value");
            assert_eq!(footprint(&cold, file), before, "a refusal changes nothing");
        }
        assert_eq!(cold.counters(), ColdReadCounters::default(), "nothing enqueued or counted");
        assert!(drain_ops(&cold).is_empty(), "no device read");
        assert_eq!(cold.reconcile(), Ok(()));
    }

    /// ADR-0167 D3: an empty window, and one wider than a pool buffer, are
    /// permanent refusals read on the `usize` — `1 << 32` would narrow to
    /// an empty `u32` window, and is refused as too wide instead.
    #[test]
    fn enqueue_refuses_an_empty_or_oversized_window() {
        let cold = path(1);
        let file = TierFileId::new(17);
        let before = footprint(&cold, file);
        let asked = cold.enqueue(3, file, FRAME, 0, ReadClass::Foreground, 0);
        assert_eq!(unrepresentable(asked, "window 0"), UnrepresentableRead::EmptyWindow);
        for window_bytes in [BUF + 1, 1usize << 32] {
            let asked = cold.enqueue(3, file, FRAME, window_bytes, ReadClass::Maintain, 0);
            assert_eq!(
                unrepresentable(asked, "oversized window"),
                UnrepresentableRead::WindowAboveMax { window_bytes, window_bytes_max: BUF },
                "the refusal carries the unnarrowed length"
            );
        }
        assert_eq!(footprint(&cold, file), before, "a refusal changes nothing");
        assert_eq!(cold.reconcile(), Ok(()));
    }

    /// ADR-0167 D3's order: the position is checked first, so a request
    /// that is both unaddressable and a bad window answers `OffsetAboveMax`.
    #[test]
    fn an_unaddressable_offset_is_refused_before_the_window() {
        let cold = path(1);
        let file = TierFileId::new(18);
        let offset = i64::MAX.cast_unsigned() + 1;
        let before = footprint(&cold, file);
        for window_bytes in [0, BUF + 1] {
            let asked = cold.enqueue(3, file, offset, window_bytes, ReadClass::Foreground, 0);
            let read = unrepresentable(asked, "two-fault request");
            let UnrepresentableRead::OffsetAboveMax(refused) = read else {
                panic!("window {window_bytes}: answered {read:?}, not the position fault");
            };
            assert_eq!(refused.offset_bytes(), offset);
        }
        assert_eq!(footprint(&cold, file), before, "a refusal changes nothing");
    }

    /// ADR-0167 D3: an unrepresentable request is refused permanently even
    /// when its class FIFO is full — never `QueueFull`, which a caller would
    /// retry, and `queue_full` does not count it.
    #[test]
    fn an_unrepresentable_request_wins_over_a_full_queue() {
        let config = ColdReadConfig { overflow_cap: 1, ..ColdReadConfig::default() };
        let cold = shaped(2, config);
        let file = TierFileId::new(19);
        let _queued = ask(&cold, 3, file, 0, 64);
        let before = footprint(&cold, file);
        let asked = cold.enqueue(3, file, u64::MAX, FRAME as usize, ReadClass::Foreground, 0);
        let read = unrepresentable(asked, "unaddressable into a full FIFO");
        assert!(matches!(read, UnrepresentableRead::OffsetAboveMax(_)), "{read:?}");
        assert_eq!(footprint(&cold, file), before, "queue_full unchanged; nothing pinned");
        assert_eq!(cold.counters().queue_full, 0);
    }

    /// ADR-0055 D4's merge at the top of the range (ADR-0167 D3): windows at
    /// `bound − 1 frame` and at `bound` join into one device read at the
    /// lower position; the span end (`bound + 1 frame ≤ i64::MAX`) comes
    /// from `bytes_after` and is never re-parsed as a position.
    #[test]
    fn boundary_windows_merge_below_i64_max() {
        let cold = path(2);
        let file = TierFileId::new(20);
        let bound = FILE_OFFSET_BYTES_MAX;
        let low = ask(&cold, 3, file, bound - FRAME, FRAME as usize);
        let high = ask(&cold, 3, file, bound, FRAME as usize);
        let ops = drain_ops(&cold);
        assert_eq!(ops.len(), 1, "the two windows join");
        let IoOp::TierRead { offset, ref buf, .. } = ops[0] else { panic!("TierRead") };
        assert_eq!(offset.bytes(), bound - FRAME, "the merged read starts at the lower window");
        assert_eq!(u64::from(buf.len()), 2 * FRAME, "the union window");
        assert!(offset.bytes_after(buf.len()) <= i64::MAX.cast_unsigned(), "span end in range");
        assert_eq!(cold.counters().merged_waiters, 1);
        drop((low, high, ops));
    }

    /// Minimal single-future block_on for gate waiters whose value is
    /// already parked (first poll returns Ready — the gate contract).
    fn block_on_ready(waiter: ColdWait) -> ColdDone {
        use core::future::Future;
        use core::task::{Context, Poll, Waker};
        let mut waiter = waiter;
        match core::pin::Pin::new(&mut waiter).poll(&mut Context::from_waker(Waker::noop())) {
            Poll::Ready(done) => done,
            Poll::Pending => panic!("value was parked; first poll must be Ready"),
        }
    }

    /// Counts the wakes its tasks receive: the delivery oracle reads this
    /// waker, never the host's counters.
    #[derive(Default)]
    struct WakeCount(std::sync::atomic::AtomicUsize);

    impl std::task::Wake for WakeCount {
        fn wake(self: std::sync::Arc<Self>) {
            self.wake_by_ref();
        }

        fn wake_by_ref(self: &std::sync::Arc<Self>) {
            self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// One device read of `members` identical requests, each polled to
    /// `Pending` under a counting waker, then its one receipt and the turns
    /// that deliver it: returns the wakes the receipt made and each turn's.
    /// Every value is then polled, read as the receipt's error and dropped,
    /// and custody reconciles.
    fn wakes_of_one_receipt(members: usize) -> (usize, Vec<usize>) {
        use core::future::Future;
        use core::task::{Context, Poll, Waker};
        let config =
            ColdReadConfig { qd_cap: 1, overflow_cap: members, ..ColdReadConfig::default() };
        let cold = shaped(1, config);
        let file = TierFileId::new(77);
        let count = std::sync::Arc::new(WakeCount::default());
        let waker = Waker::from(std::sync::Arc::clone(&count));
        let mut cx = Context::from_waker(&waker);
        let mut waits: Vec<ColdWait> = (0..members).map(|_| ask(&cold, 99, file, 0, 8)).collect();
        for wait in &mut waits {
            assert!(core::pin::Pin::new(wait).poll(&mut cx).is_pending(), "nothing delivered yet");
        }
        let ops = drain_ops(&cold);
        assert_eq!(ops.len(), 1, "the {members} identical requests merge into one device read");
        let IoOp::TierRead { token, .. } = ops[0] else { panic!("drain builds TierRead") };
        let wakes = || count.0.load(std::sync::atomic::Ordering::Relaxed);
        assert_eq!(cold.record_completion(token, Err(libc::EIO), 1), ColdReceipt::Recorded);
        let at_receipt = wakes();
        let mut turns = Vec::new();
        while cold.has_ready_delivery() {
            assert!(turns.len() < members, "every turn visits a member");
            let before = wakes();
            let mut turn = ColdTurn::begin();
            let progress = cold.deliver_ready(turn.delivery(), 2);
            assert_eq!(progress.examined() as usize, wakes() - before, "each visit one wake");
            turns.push(wakes() - before);
        }
        let woken = at_receipt + turns.iter().sum::<usize>();
        assert_eq!(woken, members, "engagement: every member was delivered, over the turns");
        for (index, mut wait) in waits.into_iter().enumerate() {
            let Poll::Ready(done) = core::pin::Pin::new(&mut wait).poll(&mut cx) else {
                panic!("a delivered member is ready");
            };
            assert_eq!(done.outcome(), Err(libc::EIO), "each member carries the receipt's error");
            drop(done);
            let pins = cold.inflight_on(file) as usize;
            assert_eq!(pins, members - index - 1, "each value's drop returns its own pin");
        }
        let counts = cold.counters();
        let exact = (counts.issued, counts.errored, counts.unclaimed, counts.delivered);
        assert_eq!(exact, (1, 1, 0, members as u64), "one read, its error, every member delivered");
        assert_eq!(
            counts.enqueued,
            counts.issued + counts.merged_waiters + counts.cancelled_queued
        );
        drop(ops);
        assert_eq!(cold.reconcile(), Ok(()), "every value dropped: custody is back");
        (at_receipt, turns)
    }

    /// A turn delivers at most 64 members (`COLD_DELIVERIES_PER_TURN`,
    /// ADR-0181 R1) and a device receipt delivers none: it marks its window
    /// ready for the turn's delivery. Here 65 identical requests merge into
    /// one device read whose one receipt is routed, then turns deliver it.
    /// The oracle is the waker's count, independent of the host; the
    /// one-member read engages it, no wake at the receipt and one at its
    /// turn.
    #[test]
    fn one_receipt_delivers_at_most_a_turns_allowance() {
        let per_turn = COLD_DELIVERIES_PER_TURN as usize;
        assert_eq!(wakes_of_one_receipt(1), (0, vec![1]), "engagement: one member, one wake");
        let members = per_turn + 1;
        let (at_receipt, turns) = wakes_of_one_receipt(members);
        assert!(
            at_receipt <= per_turn,
            "one receipt of a {members}-member read woke {at_receipt} tasks in one call; a turn \
             delivers at most {per_turn}"
        );
        assert_eq!(at_receipt, 0, "a receipt visits no member");
        for (turn, wakes) in turns.iter().enumerate() {
            assert!(
                *wakes <= per_turn,
                "turn {turn} woke {wakes} tasks; a turn delivers at most {per_turn}"
            );
        }
        assert_eq!(turns, [per_turn, 1], "the 65th member waits for the next turn");
    }

    /// A drain call's preparation is bounded: it spends at most 64 work
    /// units (`COLD_PREPARATION_WORK_PER_TURN`, ADR-0181 R3), a seed's head
    /// inspection and finish cost 3, so a call issues at most 21 reads when
    /// no request can join another; and a seed inspects only the requests
    /// queued in its own and its two neighbouring buckets of its file. The
    /// fixture: 64 live requests on 64 files, which the device cap and the
    /// pool admit whole, then a 65th on a 65th file, cancelled, at the tail.
    /// One call at HEAD issues all 64, its seeds visiting 63 + 62 + … + 0 =
    /// 2,016 candidates, and its first seed's scan reaches the tail and
    /// purges the cancelled request, on a file no seed can join.
    #[test]
    #[ignore = "a drain call issues to the device cap and each seed rescans every queued request \
                of its class, so one call's work grows with the queue; red until a call spends at \
                most 64 work units and a seed inspects only requests in its own buckets"]
    fn a_drain_call_spends_a_bounded_preparation_allowance() {
        const QD: usize = 64;
        // A seed's head inspection and its finish cost 3 units.
        const ISSUES_PER_CALL_MAX: usize = (COLD_PREPARATION_WORK_PER_TURN / 3) as usize;
        const STRIDE: u64 = 16;
        let config =
            ColdReadConfig { qd_cap: QD, overflow_cap: 4 * QD, ..ColdReadConfig::default() };
        let cold = shaped(QD, config);
        let file = |rank: u64| TierFileId::new(u32::try_from(rank + 1).expect("fixture rank"));
        let ranks = u64::try_from(QD).expect("fixture depth");
        let live: Vec<ColdWait> =
            (0..ranks).map(|rank| ask(&cold, 3, file(rank), rank * STRIDE, 8)).collect();
        drop(ask(&cold, 3, file(ranks), ranks * STRIDE, 8));
        let ops = drain_ops(&cold);
        for (rank, op) in (0u64..).zip(&ops) {
            let IoOp::TierRead { offset, buf, .. } = op else { panic!("drain builds TierRead") };
            assert_eq!(offset.bytes(), rank * STRIDE, "engagement: FIFO admission order");
            assert_eq!(buf.len(), 8, "engagement: an unmerged window");
        }
        let purged = cold.counters().cancelled_queued;
        assert!(
            ops.len() <= ISSUES_PER_CALL_MAX,
            "one drain call issued {} reads over {QD} requests no seed can join, and a seed's \
             scan reached the cancelled request at the tail ({purged} purged); a call spends at \
             most 64 work units, 3 a seed",
            ops.len()
        );
        assert_eq!(purged, 0, "a seed's scan reached a request on another file");
        drop((live, ops));
    }

    /// The issuer never repeats a live identity. A token packs a word index
    /// into 56 bits, so a mint past the last pair would carry pair 0's bits.
    /// The first read, pair 0, stays queued; the issuer is set to its last
    /// pair, which a second read takes; the third must be refused for good
    /// with nothing changed, or carry a fresh identity, and the two admitted
    /// reads still issue.
    #[test]
    fn the_read_counter_never_repeats_a_live_identity() {
        let cold = path(2);
        let file = TierFileId::new(21);
        let first = ask(&cold, 3, file, 0, 64);
        let last_pair = COLD_IDENTITY_PAIRS_MAX - 1;
        cold.state.borrow_mut().issuer = PairIssuer::starting_at(last_pair).expect("the last pair");
        let last = ask(&cold, 3, file, FRAME, 64);
        let before = footprint(&cold, file);
        match cold.enqueue(3, file, 2 * FRAME, 64, ReadClass::Foreground, 0) {
            Ok(next) => {
                let state = cold.state.borrow();
                let live: std::collections::BTreeSet<u64> =
                    state.pending[0].iter().map(|intent| intent.logical.0.as_u64()).collect();
                assert_eq!(live.len(), 3, "three live requests carry three identities");
                drop(state);
                drop(next);
            }
            Err(refused) => {
                assert_ne!(refused, ColdRefused::QueueFull, "exhaustion is permanent, not a wait");
                assert_eq!(footprint(&cold, file), before, "a refusal changes nothing");
            }
        }
        let ops = drain_ops(&cold);
        assert_eq!(ops.len(), 2, "the admitted reads issue after the refusal");
        drop((first, last, ops));
    }

    /// ADR-0181 R4's order at the space's end: a request's own fault answers
    /// first, then the spent space, permanently, before the queue bound,
    /// even with the class queue full; nothing changes but the refusal's own
    /// count, and the admitted reads still issue.
    #[test]
    fn a_spent_identity_space_refuses_before_the_queue_bound() {
        let config = ColdReadConfig { overflow_cap: 2, merge: false, ..ColdReadConfig::default() };
        let cold = shaped(4, config);
        let file = TierFileId::new(22);
        let last_two = COLD_IDENTITY_PAIRS_MAX - 2;
        cold.state.borrow_mut().issuer = PairIssuer::starting_at(last_two).expect("in the space");
        assert!(PairIssuer::starting_at(COLD_IDENTITY_PAIRS_MAX).is_none(), "no pair past 2^55");
        let held = [ask(&cold, 3, file, 0, 64), ask(&cold, 3, file, FRAME, 64)];
        assert_eq!(cold.identity_pairs_used(), COLD_IDENTITY_PAIRS_MAX, "every pair spent");
        let before = footprint(&cold, file);
        let fault = cold.enqueue(3, file, u64::MAX, 64, ReadClass::Foreground, 0);
        assert!(matches!(fault, Err(ColdRefused::Unrepresentable(_))), "the request's own fault");
        for class in [ReadClass::Foreground, ReadClass::Maintain] {
            let asked = cold.enqueue(3, file, 2 * FRAME, 64, class, 0);
            assert!(
                matches!(asked, Err(ColdRefused::IdentityExhausted)),
                "{class:?}: a spent space refuses for good, the class queue full or not"
            );
        }
        assert_eq!(footprint(&cold, file), before, "a refusal changes nothing");
        assert_eq!(cold.identity_refusals(), 2, "each refusal counted once");
        assert_eq!(cold.counters().queue_full, 0, "never the temporary refusal");
        let ops = drain_ops(&cold);
        assert_eq!(ops.len(), 2, "the admitted reads issue after the refusals");
        drop((held, ops));
    }

    /// The logical id the newest queued request of `class` carries, read
    /// from the host's queue (its decode is the token layout's, not the
    /// issuer's).
    fn newest_logical(cold: &ColdReads, class: ReadClass) -> CompletionToken {
        let state = cold.state.borrow();
        state.pending[class.index()].back().expect("a request was just queued").logical.0
    }

    /// Every identity the issuer mints is fresh and typed by its parity
    /// (OC6): an independent record of every logical id the host queues and
    /// every device id its reads carry, over rounds that each admit one kept
    /// and two cancelled requests, purge the cancelled and issue the kept,
    /// finds no word index twice, every logical id even and every device id
    /// odd, and a singleton read's device id its own request's pair.
    #[test]
    fn every_identity_the_issuer_mints_is_fresh_and_typed_by_parity() {
        const ROUNDS: u32 = 8;
        let config =
            ColdReadConfig { qd_cap: 2, overflow_cap: 8, merge: false, ..Default::default() };
        let cold = shaped(2, config);
        let mut seen = std::collections::BTreeSet::new();
        let mut minted = 0u32;
        for round in 0..ROUNDS {
            let file = TierFileId::new(300 + round);
            let kept = ask(&cold, 3, file, 0, 64);
            let kept_logical = word_index(newest_logical(&cold, ReadClass::Foreground));
            assert!(seen.insert(kept_logical), "an id repeats: logical {kept_logical}");
            for at in 1..=2u64 {
                drop(ask(&cold, 3, file, at * FRAME, 64));
                let cancelled = word_index(newest_logical(&cold, ReadClass::Foreground));
                assert!(seen.insert(cancelled), "an id repeats: logical {cancelled}");
                assert_eq!(cancelled % 2, 0, "a logical id is even");
            }
            minted += 3;
            let ops = drain_ops(&cold);
            assert_eq!(ops.len(), 1, "round {round}: the kept request issues alone");
            let IoOp::TierRead { token, .. } = ops[0] else { panic!("drain builds TierRead") };
            let device = word_index(token);
            assert!(seen.insert(device), "an id repeats: device {device}");
            assert_eq!(kept_logical % 2, 0, "a logical id is even");
            assert_eq!(device, kept_logical + 1, "a singleton reads under its own pair");
            assert_eq!(deliver_all(&cold, token, Ok(()), 1), 1);
            drop((block_on_ready(kept), ops));
            assert!(drain_ops(&cold).is_empty(), "the purge issues nothing");
        }
        assert_eq!(cold.counters().cancelled_queued, u64::from(2 * ROUNDS), "engagement: purged");
        assert_eq!(cold.identity_pairs_used(), u64::from(minted), "one pair a request");
        assert_eq!(cold.reconcile(), Ok(()));
    }

    /// A custody case's terminal receipt: success or `EIO`.
    #[derive(Clone, Copy, Debug)]
    enum Receipt {
        Success,
        Error,
    }

    impl Receipt {
        fn result(self) -> Result<(), i32> {
            match self {
                Receipt::Success => Ok(()),
                Receipt::Error => Err(libc::EIO),
            }
        }

        /// A success reads the pool's zeroed bytes (no device wrote them);
        /// an error's bytes are never read.
        fn check(self, done: &ColdDone, len: usize) {
            match self {
                Receipt::Success => {
                    assert_eq!(done.outcome(), Ok(()));
                    done.bytes(|bytes| assert_eq!(bytes, vec![0u8; len].as_slice()));
                }
                Receipt::Error => assert_eq!(done.outcome(), Err(libc::EIO)),
            }
        }
    }

    /// The custody the host reports: its leak report, or all zeros.
    fn custody(cold: &ColdReads) -> ColdLeak {
        cold.reconcile().err().unwrap_or(ColdLeak {
            leased: 0,
            inflight: 0,
            pinned_files: 0,
            queued: 0,
            windows: 0,
        })
    }

    /// One custody case at a pool of 3, a device depth of 1 and class queues
    /// of 4, each window `members` identical requests; returns the planted
    /// controls it rejected.
    fn custody_case(members: usize, receipt: Receipt) -> u32 {
        const POOL: usize = 3;
        const CLASS_QUEUE: usize = 4;
        const QUEUED: usize = 2 * CLASS_QUEUE;
        const LEN: usize = 8;
        let config = ColdReadConfig {
            qd_cap: 1,
            overflow_cap: CLASS_QUEUE,
            merge: members > 1,
            ..ColdReadConfig::default()
        };
        let cold = shaped(POOL, config);
        assert_eq!(cold.pool_reserved_bytes(), (POOL * BUF) as u64);
        let window_file = |window: usize| TierFileId::new(100 + window as u32);
        let queued_file = |index: usize| TierFileId::new(200 + index as u32);
        let mut held: Vec<ColdWait> = Vec::new();
        for window in 0..POOL {
            for _ in 0..members {
                held.push(
                    cold.enqueue(30, window_file(window), 0, LEN, ReadClass::Foreground, 10)
                        .expect("room"),
                );
            }
            let mut ops = drain_ops(&cold);
            assert_eq!(ops.len(), 1, "one device read a window");
            let IoOp::TierRead { fd, offset, ref buf, token } = ops[0] else { panic!("TierRead") };
            assert_eq!((fd, offset.bytes(), buf.len() as usize), (30, 0, LEN));
            assert_eq!(cold.inflight_on(window_file(window)) as usize, members);
            assert_eq!(deliver_all(&cold, token, receipt.result(), 20) as usize, members);
            ops.clear();
            let expected = ColdLeak {
                leased: window + 1,
                inflight: 0,
                pinned_files: window + 1,
                queued: 0,
                windows: window + 1,
            };
            assert_eq!(custody(&cold), expected, "a completed window keeps its buffer and pin");
        }
        let mut queued: Vec<ColdWait> = (0..QUEUED)
            .map(|index| {
                let class =
                    if index < CLASS_QUEUE { ReadClass::Foreground } else { ReadClass::Maintain };
                cold.enqueue(30, queued_file(index), 0, LEN, class, 30).expect("the class's room")
            })
            .collect();
        let retained = ColdLeak {
            leased: POOL,
            inflight: 0,
            pinned_files: POOL + QUEUED,
            queued: QUEUED,
            windows: POOL,
        };
        assert_eq!(custody(&cold), retained);
        assert!(drain_ops(&cold).is_empty(), "every buffer is held: nothing issues");
        assert_eq!(custody(&cold), retained);
        let admitted = cold.counters().enqueued;
        for (index, class) in [ReadClass::Foreground, ReadClass::Maintain].into_iter().enumerate() {
            let refused = TierFileId::new(999 + index as u32);
            let asked = cold.enqueue(30, refused, 0, LEN, class, 30);
            assert!(matches!(asked, Err(ColdRefused::QueueFull)), "a full class queue refuses");
            assert_eq!(cold.inflight_on(refused), 0, "a refusal pins nothing");
        }
        assert_eq!(cold.counters().enqueued, admitted);
        assert_eq!(custody(&cold), retained);
        let mut plants = 0;
        let logical = held.len() + queued.len();
        // The holders' bound: both class queues, plus every window's members.
        let bound = 2 * CLASS_QUEUE + POOL * members;
        assert!(logical <= bound);
        if members > 1 {
            assert_eq!(logical, bound, "the merged fixture reaches its bound");
            // Two wrong bounds the predicate must reject: the device depth in
            // the pool's place, and one holder a window.
            for wrong in [2 * CLASS_QUEUE + config.qd_cap * members, POOL + QUEUED] {
                assert!(logical > wrong, "the plant engages and rejects bound {wrong}");
                plants += 1;
            }
        }
        let values: Vec<ColdDone> = held.drain(..).map(block_on_ready).collect();
        for (index, done) in values.iter().enumerate() {
            assert_eq!(done.file(), window_file(index / members));
            receipt.check(done, LEN);
        }
        assert_eq!(custody(&cold), retained, "polling a value moves no custody");
        if members > 1 {
            let at_poll = ColdLeak {
                leased: 0,
                inflight: 0,
                pinned_files: QUEUED,
                queued: QUEUED,
                windows: 0,
            };
            assert_ne!(custody(&cold), at_poll, "a release at the poll is rejected");
            plants += 1;
        }
        let mut values = values.into_iter();
        for done in values.by_ref().take(members - 1) {
            drop(done);
        }
        assert_eq!(cold.inflight_on(window_file(0)), 1, "the window's last value keeps its pin");
        assert_eq!(custody(&cold), retained);
        let one_returned = ColdLeak {
            leased: POOL - 1,
            inflight: 0,
            pinned_files: POOL + QUEUED - 1,
            queued: QUEUED,
            windows: POOL - 1,
        };
        if members > 1 {
            assert_ne!(custody(&cold), one_returned, "a release before the last value is rejected");
            plants += 1;
        }
        drop(values.next().expect("the first window's last value"));
        assert_eq!(cold.inflight_on(window_file(0)), 0);
        assert_eq!(custody(&cold), one_returned, "exactly that window's buffer and pin return");
        drop(values);
        let only_queued =
            ColdLeak { leased: 0, inflight: 0, pinned_files: QUEUED, queued: QUEUED, windows: 0 };
        assert_eq!(custody(&cold), only_queued);
        for (index, wait) in queued.drain(..).enumerate() {
            assert_eq!(cold.inflight_on(queued_file(index)), 1, "a queued request keeps its pin");
            drop(wait);
        }
        assert_eq!(custody(&cold), only_queued, "a cancelled request keeps its pin until a drain");
        assert!(drain_ops(&cold).is_empty(), "the purge issues nothing");
        assert_eq!(cold.reconcile(), Ok(()));
        let counts = cold.counters();
        assert_eq!(counts.enqueued, logical as u64);
        assert_eq!(counts.issued, POOL as u64);
        assert_eq!(counts.merged_waiters, (POOL * (members - 1)) as u64);
        assert_eq!(counts.delivered, (POOL * members) as u64);
        assert_eq!(counts.cancelled_queued, QUEUED as u64);
        assert_eq!((counts.unclaimed, counts.queue_full, counts.pool_dry), (0, 2, 1));
        assert_eq!(counts.queue_depth_high_water, QUEUED as u32);
        let (completed, errored) = match receipt {
            Receipt::Success => (POOL as u64, 0),
            Receipt::Error => (0, POOL as u64),
        };
        assert_eq!((counts.completed, counts.errored), (completed, errored));
        assert_eq!(
            counts.enqueued,
            counts.issued + counts.merged_waiters + counts.cancelled_queued
        );
        plants
    }

    /// The custody instrument: who holds a buffer, a window and a pin from
    /// enqueue to a window's last value. Each of three files takes one read,
    /// singleton or four identical requests merged, issued and completed,
    /// its values kept; both class queues then fill on other files, which no
    /// drain can issue with every buffer held, and a fifth request in each
    /// class is refused before it pins. Polling moves no custody; dropping
    /// all but a window's last value returns nothing; the last returns that
    /// window's buffer and pin alone; the cancelled queue purges with no
    /// device read. The planted controls (a wrong holder bound, a release at
    /// the poll or before the last value) must not match what was observed.
    #[test]
    fn custody_holds_until_a_windows_last_value() {
        let mut plants = 0;
        for receipt in [Receipt::Success, Receipt::Error] {
            for members in [1, 4] {
                plants += custody_case(members, receipt);
            }
        }
        assert_eq!(plants, 8, "every planted control engaged");
    }

    /// The fixed stores' entries as the cold host lays them out, built from
    /// the real component types (`SlotId<u32>`, `FileOffset`, `AlignedBufId`,
    /// `TierFileId`); the gate's hold, a claim on one holder slot, is one
    /// `SlotId<u32>`. They are measured by `size_of` before any store holds
    /// them.
    #[allow(dead_code, reason = "layout carriers: measured by size_of, never built")]
    mod layout {
        use core::num::NonZeroU64;

        use inf_alloc::AlignedBufId;
        use inf_foundation::FileOffset;
        use inf_foundation::bounded::SlotId;

        use crate::cold::TierFileId;
        use crate::driver::RawFd;

        /// A logical or device id: the packed `TierRead` token word, never
        /// zero (its class byte is `TierRead`'s), so an `Option` of one needs
        /// no tag.
        pub(super) struct Identity(NonZeroU64);

        /// The pin key: the tiered namespace's lifetime and its file.
        pub(super) struct PinKey {
            pub(super) lifetime: u32,
            pub(super) file: TierFileId,
        }

        /// A stored link: the handle the slab minted, or none.
        pub(super) type Link = Option<SlotId<u32>>;

        /// A queued request. The fields an inspection reads come first and
        /// end by byte 52.
        #[repr(C)]
        pub(super) struct IntentSlot {
            pub(super) offset: FileOffset,
            pub(super) fd: RawFd,
            pub(super) len: u32,
            pub(super) hold: SlotId<u32>,
            pub(super) bucket_next: Link,
            pub(super) bucket_prev: Link,
            pub(super) logical: Identity,
            pub(super) device: Identity,
            pub(super) key: PinKey,
            pub(super) bucket: u64,
            pub(super) enqueued_us: u64,
            pub(super) fifo_prev: Link,
            pub(super) fifo_next: Link,
        }

        /// One member's record in its window's waiter block.
        pub(super) struct WaiterRecord {
            pub(super) logical: Identity,
            pub(super) skip: u32,
            pub(super) len: u32,
            pub(super) enqueued_us: u64,
        }

        /// A block position: a record, or none.
        pub(super) type WaiterSlot = Option<WaiterRecord>;

        /// One issued window's records: at most 31 members.
        pub(super) type WaiterBlock = [WaiterSlot; 31];

        #[repr(u8)]
        pub(super) enum Phase {
            Issued,
            Ready,
            Held,
        }

        /// One issued window.
        pub(super) struct WindowDescriptor {
            pub(super) device: Identity,
            pub(super) completed_us: u64,
            pub(super) key: PinKey,
            pub(super) outcome: Result<(), i32>,
            pub(super) block: SlotId<u32>,
            pub(super) ready_next: Link,
            pub(super) buf: AlignedBufId,
            pub(super) initialized: u32,
            pub(super) outstanding: u32,
            pub(super) cursor: u32,
            pub(super) phase: Phase,
        }

        /// A candidate list's two ends.
        pub(super) struct ListEnds {
            pub(super) head: SlotId<u32>,
            pub(super) tail: SlotId<u32>,
        }
    }

    /// The entries fit their strides: a waiter record and its block
    /// position 24 B (the logical id's niche carries the position's tag), a
    /// block of 31 at 744 B, a window descriptor at most 104 B, a queued
    /// request at most 120 B, a candidate list's ends 24 B. Each field a
    /// request's inspection reads ends by byte 56, so with the store's 8 B
    /// slot header, on whichever side, it lies in the slot's first 64 B.
    #[test]
    fn the_fixed_stores_entries_fit_their_strides() {
        use core::mem::{offset_of, size_of};
        use layout::{
            IntentSlot, ListEnds, WaiterBlock, WaiterRecord, WaiterSlot, WindowDescriptor,
        };
        let sizes = [
            ("WaiterRecord", size_of::<WaiterRecord>(), 24),
            ("WaiterSlot", size_of::<WaiterSlot>(), 24),
            ("WaiterBlock", size_of::<WaiterBlock>(), 744),
            ("WindowDescriptor", size_of::<WindowDescriptor>(), 104),
            ("IntentSlot", size_of::<IntentSlot>(), 120),
            ("ListEnds", size_of::<ListEnds>(), 24),
        ];
        for (name, size, bound) in sizes {
            eprintln!("stride {name}: {size} B (bound {bound} B)");
            assert!(size <= bound, "{name} is {size} B, above its stride's {bound} B");
        }
        let link = size_of::<layout::Link>();
        let inspected = [
            ("offset", offset_of!(IntentSlot, offset), size_of::<inf_foundation::FileOffset>()),
            ("fd", offset_of!(IntentSlot, fd), size_of::<RawFd>()),
            ("len", offset_of!(IntentSlot, len), size_of::<u32>()),
            (
                "hold",
                offset_of!(IntentSlot, hold),
                size_of::<inf_foundation::bounded::SlotId<u32>>(),
            ),
            ("bucket_next", offset_of!(IntentSlot, bucket_next), link),
            ("bucket_prev", offset_of!(IntentSlot, bucket_prev), link),
        ];
        for (field, offset, size) in inspected {
            eprintln!("IntentSlot.{field}: bytes {offset}..{}", offset + size);
            assert!(offset + size <= 56, "IntentSlot.{field} ends at byte {}", offset + size);
        }
    }
}
